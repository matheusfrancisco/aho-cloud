//! Pools and the start-time schema check (storage.md "Adapter internals", "Migrations").
//!
//! Each process holds two pools: **tenant** (login role in group `mage_tenant`, RLS applies) and
//! **platform** (group `mage_platform`, the enumerated cross-org methods). Dev and tests may use
//! one pool for both ([`PgPools::single`]). `statement_timeout` is set per pool at connect
//! (server 5 s, indexer 60 s, `mage migrate` none); `acquire_timeout` 5 s; `idle_timeout` 10 min.

use std::collections::BTreeMap;
use std::str::FromStr;
use std::time::Duration;

use serde::Deserialize;
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

use super::error::DbError;
use super::schema::MIGRATOR;
use crate::config::SecretSource;

/// The two pools every pg adapter takes.
#[derive(Debug, Clone)]
pub struct PgPools {
    /// Tenant-scoped statements (`begin_org`), RLS on.
    pub tenant: PgPool,
    /// Platform methods (job leasing, cross-org sweeps, platform tables).
    pub platform: PgPool,
}

impl PgPools {
    pub fn new(tenant: PgPool, platform: PgPool) -> Self {
        Self { tenant, platform }
    }

    /// One pool for both roles (local dev, tests).
    pub fn single(pool: PgPool) -> Self {
        Self { tenant: pool.clone(), platform: pool }
    }

    pub async fn close(&self) {
        self.tenant.close().await;
        self.platform.close().await;
    }
}

/// `[<role>.database]` section (server.md "Config"): `url_source = "env:MAGE_DATABASE_URL"`,
/// `pool_max = 15`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PgConfig {
    /// Tenant pool connection URL (a secret reference: `env:` / `file:` / `gcpsm:`).
    pub url_source: SecretSource,
    /// Platform pool URL; `None` = the tenant URL (dev).
    #[serde(default)]
    pub platform_url_source: Option<SecretSource>,
    /// Tenant pool size.
    #[serde(default = "default_tenant_max")]
    pub pool_max: u32,
    #[serde(default = "default_platform_max")]
    pub platform_pool_max: u32,
    /// Per-statement limit; `0` = none (`mage migrate`). Server 5 s, indexer 60 s.
    #[serde(default = "default_statement_timeout_ms")]
    pub statement_timeout_ms: u64,
    #[serde(default = "default_acquire_timeout_ms")]
    pub acquire_timeout_ms: u64,
    #[serde(default = "default_idle_timeout_ms")]
    pub idle_timeout_ms: u64,
}

const fn default_tenant_max() -> u32 {
    10
}
const fn default_platform_max() -> u32 {
    4
}
const fn default_statement_timeout_ms() -> u64 {
    5_000
}
const fn default_acquire_timeout_ms() -> u64 {
    5_000
}
const fn default_idle_timeout_ms() -> u64 {
    600_000
}

impl PgConfig {
    /// Defaults for `url_source` (server values: 10 + 4 connections, 5 s statements).
    pub fn new(url_source: SecretSource) -> Self {
        Self {
            url_source,
            platform_url_source: None,
            pool_max: default_tenant_max(),
            platform_pool_max: default_platform_max(),
            statement_timeout_ms: default_statement_timeout_ms(),
            acquire_timeout_ms: default_acquire_timeout_ms(),
            idle_timeout_ms: default_idle_timeout_ms(),
        }
    }
}

/// Connect both pools; one connection each is opened here so a bad URL or password fails at start.
pub async fn connect(config: &PgConfig) -> Result<PgPools, DbError> {
    let tenant_url = config.url_source.resolve().await.map_err(|e| DbError::Invalid(format!("database.url_source: {e}")))?;
    let tenant = connect_pool(tenant_url.expose(), config.pool_max, config).await?;
    let platform = match &config.platform_url_source {
        None => tenant.clone(),
        Some(source) => {
            let url = source.resolve().await.map_err(|e| DbError::Invalid(format!("database.platform_url_source: {e}")))?;
            connect_pool(url.expose(), config.platform_pool_max, config).await?
        }
    };
    Ok(PgPools { tenant, platform })
}

/// One pool from a URL with the config's timeouts.
pub async fn connect_pool(url: &str, max_connections: u32, config: &PgConfig) -> Result<PgPool, DbError> {
    let mut options = PgConnectOptions::from_str(url).map_err(|e| DbError::Invalid(format!("postgres url: {e}")))?;
    if config.statement_timeout_ms > 0 {
        options = options.options([("statement_timeout", config.statement_timeout_ms.to_string())]);
    }
    let pool = PgPoolOptions::new()
        .max_connections(max_connections.max(1))
        .acquire_timeout(Duration::from_millis(config.acquire_timeout_ms))
        .idle_timeout(Some(Duration::from_millis(config.idle_timeout_ms)))
        .connect_with(options)
        .await?;
    Ok(pool)
}

/// Outcome of comparing the embedded migrations with `_sqlx_migrations`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaStatus {
    /// Every embedded migration is applied.
    Current,
    /// The database also has migrations this binary does not know (expand/contract: start).
    Ahead { unknown: Vec<i64> },
}

/// Start-time check: the database behind the binary (a missing or failed migration) or an edited
/// applied migration (checksum mismatch) is `Invalid` and the role refuses to start; a database
/// ahead of the binary is fine.
pub async fn check_schema(pool: &PgPool) -> Result<SchemaStatus, DbError> {
    let rows: Vec<(i64, bool, Vec<u8>)> = sqlx::query_as("SELECT version, success, checksum FROM _sqlx_migrations")
        .fetch_all(pool)
        .await
        .map_err(|e| match DbError::from(e) {
            DbError::Unavailable(msg) if msg.contains("42P01") => DbError::Invalid("database has no migrations applied; run `mage migrate`".into()),
            other => other,
        })?;
    let applied: BTreeMap<i64, (bool, Vec<u8>)> = rows.into_iter().map(|(v, ok, sum)| (v, (ok, sum))).collect();
    let mut missing = Vec::new();
    for migration in MIGRATOR.iter().filter(|m| m.migration_type.is_up_migration()) {
        match applied.get(&migration.version) {
            Some((true, checksum)) if checksum.as_slice() == migration.checksum.as_ref() => {}
            Some((true, _)) => {
                return Err(DbError::Invalid(format!("migration {} was edited after it was applied (checksum mismatch)", migration.version)));
            }
            Some((false, _)) | None => missing.push(migration.version),
        }
    }
    if !missing.is_empty() {
        return Err(DbError::Invalid(format!("database is behind this binary: migrations {missing:?} not applied; run `mage migrate`")));
    }
    let known: Vec<i64> = MIGRATOR.iter().map(|m| m.version).collect();
    let unknown: Vec<i64> = applied.keys().copied().filter(|v| !known.contains(v)).collect();
    Ok(if unknown.is_empty() { SchemaStatus::Current } else { SchemaStatus::Ahead { unknown } })
}
