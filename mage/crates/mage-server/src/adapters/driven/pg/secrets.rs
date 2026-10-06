//! `PgSecretStore` (storage.md "`SecretStore`", "Encryption"): sealed secret versions and per-org
//! data keys; `SecretVault` does all cryptography. Pools (storage.md "Tenancy enforcement"):
//! platform-scope secrets, `list_wraps` and `rewrap` run on the platform pool; org-scope secrets,
//! data keys and `destroy_org` on the tenant pool inside `begin_org` (`data_keys` has RLS;
//! `secrets` is a platform table the tenant role may write, so every statement filters `org_id`).
//!
//! Version sequencing reads `max(version)` in the writing transaction; two concurrent writers of
//! the same next version collide on the `secrets_version` unique index (`Conflict`). An `Active`
//! insert or activation retires the previous `Active` first, so `secrets_active` /
//! `data_keys_active` never see two.

use std::sync::Arc;

use async_trait::async_trait;
use mage_core::domain::OrgId;
use mage_core::ports::clock::Clock;
use mage_core::ports::ids::IdGen;
use mage_infra::pg::{DbError, PgPools, Tx, TxError, begin_org, begin_platform, retry_tx};
use time::OffsetDateTime;
use uuid::Uuid;

use super::codec::{fixed, parse_enum, to_unsigned};
use crate::ports::audit::Actor;
use crate::ports::secrets::{
    DataKeyState, SealedDataKey, SealedSecret, SecretStore, SecretStoreError, WrapInfo, WrapRef, check_new_data_key, check_new_secret,
};
use crate::ports::vault::{SecretName, SecretScope, SecretState, SecretVersion};

pub struct PgSecretStore {
    pools: PgPools,
    clock: Arc<dyn Clock>,
}

impl PgSecretStore {
    /// Ids are never generated here (rows are keyed by scope, name and version).
    pub fn new(pools: PgPools, clock: Arc<dyn Clock>, _ids: Arc<dyn IdGen>) -> Self {
        Self { pools, clock }
    }

    /// Platform scope → platform pool; org scope → tenant pool scoped to the org.
    async fn begin(&self, scope: &SecretScope) -> Result<Tx, DbError> {
        match scope {
            SecretScope::Platform => begin_platform(&self.pools.platform).await,
            SecretScope::Org(org) => begin_org(&self.pools.tenant, *org).await,
        }
    }
}

impl From<DbError> for SecretStoreError {
    fn from(err: DbError) -> Self {
        match err {
            DbError::NotFound => SecretStoreError::NotFound,
            DbError::Conflict { .. } => SecretStoreError::Conflict,
            DbError::Invalid(msg) => SecretStoreError::Invalid(msg),
            DbError::Unavailable(msg) | DbError::Retry(msg) => SecretStoreError::Unavailable(msg),
            DbError::Timeout => SecretStoreError::Unavailable("statement timeout".into()),
        }
    }
}

type TxResult<T> = Result<T, TxError<SecretStoreError>>;

/// `int` version column.
fn version_param(version: u32) -> Result<i32, DbError> {
    i32::try_from(version).map_err(|_| DbError::Invalid(format!("version {version} out of range")))
}

fn org_param(scope: &SecretScope) -> Option<Uuid> {
    scope.org().map(|o| o.as_uuid())
}

/// `secrets` columns, in `SecretRow` order.
macro_rules! secret_columns {
    () => {
        "org_id, name, version, state, wrapped_dek, kek_version, nonce, ciphertext, fingerprint, created_by, created_at, state_changed_at"
    };
}

/// `$1` = org (NULL for the platform scope), `$2` = name.
macro_rules! scoped_name {
    () => {
        "org_id IS NOT DISTINCT FROM $1 AND name = $2"
    };
}

const MAX_VERSION: &str = concat!("SELECT COALESCE(max(version), 0) FROM secrets WHERE ", scoped_name!());
const RETIRE_ACTIVE: &str =
    concat!("UPDATE secrets SET state = 'retired', state_changed_at = $3 WHERE ", scoped_name!(), " AND state = 'active'");
const INSERT_SECRET: &str =
    concat!("INSERT INTO secrets (", secret_columns!(), ") VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)");
const SELECT_ACTIVE: &str = concat!("SELECT ", secret_columns!(), " FROM secrets WHERE ", scoped_name!(), " AND state = 'active'");
const SELECT_VERSION: &str = concat!("SELECT ", secret_columns!(), " FROM secrets WHERE ", scoped_name!(), " AND version = $3");
const LOCK_VERSION: &str = concat!("SELECT ", secret_columns!(), " FROM secrets WHERE ", scoped_name!(), " AND version = $3 FOR UPDATE");
const SELECT_VERSIONS: &str = concat!("SELECT ", secret_columns!(), " FROM secrets WHERE ", scoped_name!(), " ORDER BY version");
const SELECT_SCOPE: &str =
    concat!("SELECT ", secret_columns!(), " FROM secrets WHERE org_id IS NOT DISTINCT FROM $1 ORDER BY name COLLATE \"C\", version");
/// `$4` = next state, `$5` = now. `Destroyed` wipes the DEK and ciphertext (CHECK constraints).
const SET_STATE: &str = concat!(
    "UPDATE secrets SET state = $4, state_changed_at = $5, \
            wrapped_dek = CASE WHEN $4 = 'destroyed' THEN NULL ELSE wrapped_dek END, \
            ciphertext  = CASE WHEN $4 = 'destroyed' THEN NULL ELSE ciphertext END \
     WHERE ",
    scoped_name!(),
    " AND version = $3 RETURNING ",
    secret_columns!()
);

/// Keyset after secret `($1 org or NULL, $2 name, $3 version)`, or from the start when `$2` is
/// NULL: platform rows (org NULL) first, then by org, name bytes, version.
const SECRET_WRAPS: &str = "SELECT org_id, name, version, kek_version FROM secrets \
     WHERE state <> 'destroyed' \
       AND ($2::text IS NULL \
            OR ($1::uuid IS NULL AND (org_id IS NOT NULL OR name COLLATE \"C\" > $2 OR (name = $2 AND version > $3))) \
            OR org_id > $1 \
            OR (org_id = $1 AND (name COLLATE \"C\" > $2 OR (name = $2 AND version > $3)))) \
     ORDER BY org_id NULLS FIRST, name COLLATE \"C\", version \
     LIMIT $4";

/// Keyset after data key `($1 org, $2 version)`, or from the start when `$1` is NULL.
const DATA_KEY_WRAPS: &str = "SELECT org_id, version, kek_version FROM data_keys \
     WHERE state <> 'destroyed' AND ($1::uuid IS NULL OR (org_id, version) > ($1, $2)) \
     ORDER BY org_id, version \
     LIMIT $3";

#[derive(sqlx::FromRow)]
struct SecretRow {
    org_id: Option<Uuid>,
    name: String,
    version: i32,
    state: String,
    wrapped_dek: Option<Vec<u8>>,
    kek_version: String,
    nonce: Vec<u8>,
    ciphertext: Option<Vec<u8>>,
    fingerprint: String,
    created_by: String,
    created_at: OffsetDateTime,
    state_changed_at: OffsetDateTime,
}

impl SecretRow {
    fn into_sealed(self) -> Result<SealedSecret, DbError> {
        Ok(SealedSecret {
            scope: scope_of(self.org_id),
            name: secret_name(self.name)?,
            version: SecretVersion(to_unsigned(i64::from(self.version), "secrets.version")?),
            state: parse_enum(&self.state, "secrets.state", SecretState::parse)?,
            wrapped_dek: self.wrapped_dek,
            kek_version: self.kek_version,
            nonce: fixed(self.nonce, "secrets.nonce")?,
            ciphertext: self.ciphertext,
            fingerprint: self.fingerprint,
            created_by: Actor::decode(&self.created_by).map_err(|e| DbError::Invalid(format!("secrets.created_by: {e}")))?,
            created_at: self.created_at,
            state_changed_at: self.state_changed_at,
        })
    }
}

#[derive(sqlx::FromRow)]
struct DataKeyRow {
    org_id: Uuid,
    version: i32,
    wrapped_dek: Option<Vec<u8>>,
    kek_version: String,
    state: String,
    created_at: OffsetDateTime,
}

impl DataKeyRow {
    fn into_key(self) -> Result<SealedDataKey, DbError> {
        Ok(SealedDataKey {
            org: OrgId::from_uuid(self.org_id),
            version: to_unsigned(i64::from(self.version), "data_keys.version")?,
            wrapped_dek: self.wrapped_dek,
            kek_version: self.kek_version,
            state: parse_enum(&self.state, "data_keys.state", DataKeyState::parse)?,
            created_at: self.created_at,
        })
    }
}

fn scope_of(org: Option<Uuid>) -> SecretScope {
    org.map_or(SecretScope::Platform, |o| SecretScope::Org(OrgId::from_uuid(o)))
}

fn secret_name(name: String) -> Result<SecretName, DbError> {
    SecretName::new(name).map_err(|e| DbError::Invalid(format!("secrets.name: {e}")))
}

fn sealed_rows(rows: Vec<SecretRow>) -> Result<Vec<SealedSecret>, DbError> {
    rows.into_iter().map(SecretRow::into_sealed).collect()
}

impl PgSecretStore {
    /// `set_secret_state` inside `tx` (committed by the caller).
    async fn set_state_in(
        &self,
        tx: &mut Tx,
        scope: &SecretScope,
        name: &SecretName,
        version: i32,
        next: SecretState,
    ) -> TxResult<SealedSecret> {
        let org = org_param(scope);
        let row: Option<SecretRow> =
            sqlx::query_as(LOCK_VERSION).bind(org).bind(name.as_str()).bind(version).fetch_optional(&mut **tx).await?;
        let current = row.ok_or(TxError::Port(SecretStoreError::NotFound))?.into_sealed()?;
        if current.state == next {
            return Ok(current);
        }
        if !current.state.can_become(next) {
            return Err(TxError::Port(SecretStoreError::Invalid(format!("secret state {} cannot become {next}", current.state))));
        }
        let now = self.clock.now();
        if next == SecretState::Active {
            sqlx::query(RETIRE_ACTIVE).bind(org).bind(name.as_str()).bind(now).execute(&mut **tx).await?;
        }
        let row: SecretRow = sqlx::query_as(SET_STATE)
            .bind(org)
            .bind(name.as_str())
            .bind(version)
            .bind(next.as_str())
            .bind(now)
            .fetch_one(&mut **tx)
            .await?;
        Ok(row.into_sealed()?)
    }

    async fn list_wraps_in(&self, tx: &mut Tx, after: Option<&WrapRef>, limit: i64) -> TxResult<Vec<WrapInfo>> {
        let mut out = Vec::new();
        if !matches!(after, Some(WrapRef::DataKey { .. })) {
            let (org, name, version) = match after {
                Some(WrapRef::Secret { scope, name, version }) => (org_param(scope), Some(name.as_str()), Some(version_param(version.0)?)),
                _ => (None, None, None),
            };
            let rows: Vec<(Option<Uuid>, String, i32, String)> =
                sqlx::query_as(SECRET_WRAPS).bind(org).bind(name).bind(version).bind(limit).fetch_all(&mut **tx).await?;
            for (org, name, version, kek_version) in rows {
                let version = SecretVersion(to_unsigned(i64::from(version), "secrets.version")?);
                out.push(WrapInfo { r: WrapRef::Secret { scope: scope_of(org), name: secret_name(name)?, version }, kek_version });
            }
        }
        let remaining = limit - out.len() as i64;
        if remaining > 0 {
            let (org, version) = match after {
                Some(WrapRef::DataKey { org, version }) => (Some(org.as_uuid()), Some(version_param(*version)?)),
                _ => (None, None),
            };
            let rows: Vec<(Uuid, i32, String)> =
                sqlx::query_as(DATA_KEY_WRAPS).bind(org).bind(version).bind(remaining).fetch_all(&mut **tx).await?;
            for (org, version, kek_version) in rows {
                let version = to_unsigned(i64::from(version), "data_keys.version")?;
                out.push(WrapInfo { r: WrapRef::DataKey { org: OrgId::from_uuid(org), version }, kek_version });
            }
        }
        Ok(out)
    }

    /// CAS on `kek_version`; 0 rows → the live wrap is missing (`NotFound`) or newer (`Conflict`).
    async fn rewrap_in(&self, tx: &mut Tx, r: &WrapRef, expect_kek_version: &str, wrapped_dek: &[u8], kek_version: &str) -> TxResult<()> {
        let live = match r {
            WrapRef::Secret { scope, name, version } => {
                let version = version_param(version.0)?;
                let updated = sqlx::query(concat!(
                    "UPDATE secrets SET wrapped_dek = $4, kek_version = $5 WHERE ",
                    scoped_name!(),
                    " AND version = $3 AND state <> 'destroyed' AND kek_version = $6"
                ))
                .bind(org_param(scope))
                .bind(name.as_str())
                .bind(version)
                .bind(wrapped_dek)
                .bind(kek_version)
                .bind(expect_kek_version)
                .execute(&mut **tx)
                .await?;
                if updated.rows_affected() > 0 {
                    return Ok(());
                }
                let exists: bool = sqlx::query_scalar(concat!(
                    "SELECT EXISTS (SELECT 1 FROM secrets WHERE ",
                    scoped_name!(),
                    " AND version = $3 AND state <> 'destroyed')"
                ))
                .bind(org_param(scope))
                .bind(name.as_str())
                .bind(version)
                .fetch_one(&mut **tx)
                .await?;
                exists
            }
            WrapRef::DataKey { org, version } => {
                let version = version_param(*version)?;
                let updated = sqlx::query(
                    "UPDATE data_keys SET wrapped_dek = $3, kek_version = $4 \
                     WHERE org_id = $1 AND version = $2 AND state <> 'destroyed' AND kek_version = $5",
                )
                .bind(org.as_uuid())
                .bind(version)
                .bind(wrapped_dek)
                .bind(kek_version)
                .bind(expect_kek_version)
                .execute(&mut **tx)
                .await?;
                if updated.rows_affected() > 0 {
                    return Ok(());
                }
                let exists: bool = sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM data_keys WHERE org_id = $1 AND version = $2 AND state <> 'destroyed')",
                )
                .bind(org.as_uuid())
                .bind(version)
                .fetch_one(&mut **tx)
                .await?;
                exists
            }
        };
        Err(TxError::Port(if live { SecretStoreError::Conflict } else { SecretStoreError::NotFound }))
    }
}

#[async_trait]
impl SecretStore for PgSecretStore {
    async fn next_secret_version(&self, scope: &SecretScope, name: &SecretName) -> Result<SecretVersion, SecretStoreError> {
        retry_tx(|| async {
            let mut tx = self.begin(scope).await?;
            let max: i32 = sqlx::query_scalar(MAX_VERSION).bind(org_param(scope)).bind(name.as_str()).fetch_one(&mut *tx).await?;
            tx.commit().await?;
            let max: u32 = to_unsigned(i64::from(max), "secrets.version")?;
            Ok::<_, TxError<SecretStoreError>>(SecretVersion(max + 1))
        })
        .await
    }

    async fn insert_secret(&self, s: &SealedSecret) -> Result<(), SecretStoreError> {
        check_new_secret(s)?;
        let version = version_param(s.version.0)?;
        let org = org_param(&s.scope);
        let created_by = s.created_by.encode();
        let now = self.clock.now();
        retry_tx(|| async {
            let mut tx = self.begin(&s.scope).await?;
            let max: i32 = sqlx::query_scalar(MAX_VERSION).bind(org).bind(s.name.as_str()).fetch_one(&mut *tx).await?;
            if i64::from(version) != i64::from(max) + 1 {
                return Err(TxError::Port(SecretStoreError::Conflict));
            }
            if s.state == SecretState::Active {
                sqlx::query(RETIRE_ACTIVE).bind(org).bind(s.name.as_str()).bind(now).execute(&mut *tx).await?;
            }
            sqlx::query(INSERT_SECRET)
                .bind(org)
                .bind(s.name.as_str())
                .bind(version)
                .bind(s.state.as_str())
                .bind(s.wrapped_dek.as_deref())
                .bind(&s.kek_version)
                .bind(s.nonce.as_slice())
                .bind(s.ciphertext.as_deref())
                .bind(&s.fingerprint)
                .bind(&created_by)
                .bind(s.created_at)
                .bind(s.state_changed_at)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            Ok::<_, TxError<SecretStoreError>>(())
        })
        .await
    }

    async fn active_secret(&self, scope: &SecretScope, name: &SecretName) -> Result<Option<SealedSecret>, SecretStoreError> {
        retry_tx(|| async {
            let mut tx = self.begin(scope).await?;
            let row: Option<SecretRow> =
                sqlx::query_as(SELECT_ACTIVE).bind(org_param(scope)).bind(name.as_str()).fetch_optional(&mut *tx).await?;
            tx.commit().await?;
            Ok::<_, TxError<SecretStoreError>>(row.map(SecretRow::into_sealed).transpose()?)
        })
        .await
    }

    async fn secret_version(
        &self,
        scope: &SecretScope,
        name: &SecretName,
        version: SecretVersion,
    ) -> Result<SealedSecret, SecretStoreError> {
        let version = version_param(version.0)?;
        retry_tx(|| async {
            let mut tx = self.begin(scope).await?;
            let row: Option<SecretRow> =
                sqlx::query_as(SELECT_VERSION).bind(org_param(scope)).bind(name.as_str()).bind(version).fetch_optional(&mut *tx).await?;
            tx.commit().await?;
            let row = row.ok_or(TxError::Port(SecretStoreError::NotFound))?;
            Ok::<_, TxError<SecretStoreError>>(row.into_sealed()?)
        })
        .await
    }

    async fn secret_versions(&self, scope: &SecretScope, name: &SecretName) -> Result<Vec<SealedSecret>, SecretStoreError> {
        retry_tx(|| async {
            let mut tx = self.begin(scope).await?;
            let rows: Vec<SecretRow> =
                sqlx::query_as(SELECT_VERSIONS).bind(org_param(scope)).bind(name.as_str()).fetch_all(&mut *tx).await?;
            tx.commit().await?;
            Ok::<_, TxError<SecretStoreError>>(sealed_rows(rows)?)
        })
        .await
    }

    async fn list_secrets(&self, scope: &SecretScope) -> Result<Vec<SealedSecret>, SecretStoreError> {
        retry_tx(|| async {
            let mut tx = self.begin(scope).await?;
            let rows: Vec<SecretRow> = sqlx::query_as(SELECT_SCOPE).bind(org_param(scope)).fetch_all(&mut *tx).await?;
            tx.commit().await?;
            Ok::<_, TxError<SecretStoreError>>(sealed_rows(rows)?)
        })
        .await
    }

    async fn set_secret_state(
        &self,
        scope: &SecretScope,
        name: &SecretName,
        version: SecretVersion,
        state: SecretState,
    ) -> Result<SealedSecret, SecretStoreError> {
        let version = version_param(version.0)?;
        retry_tx(|| async {
            let mut tx = self.begin(scope).await?;
            let sealed = self.set_state_in(&mut tx, scope, name, version, state).await?;
            tx.commit().await?;
            Ok::<_, TxError<SecretStoreError>>(sealed)
        })
        .await
    }

    async fn insert_data_key(&self, key: &SealedDataKey) -> Result<(), SecretStoreError> {
        check_new_data_key(key)?;
        let version = version_param(key.version)?;
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, key.org).await?;
            if key.state == DataKeyState::Active {
                sqlx::query("UPDATE data_keys SET state = 'retired' WHERE org_id = $1 AND state = 'active'")
                    .bind(key.org.as_uuid())
                    .execute(&mut *tx)
                    .await?;
            }
            // An existing version is a primary-key violation (`Conflict`) and rolls the retire back.
            sqlx::query(
                "INSERT INTO data_keys (org_id, version, wrapped_dek, kek_version, state, created_at) VALUES ($1, $2, $3, $4, $5, $6)",
            )
            .bind(key.org.as_uuid())
            .bind(version)
            .bind(key.wrapped_dek.as_deref())
            .bind(&key.kek_version)
            .bind(key.state.as_str())
            .bind(key.created_at)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            Ok::<_, TxError<SecretStoreError>>(())
        })
        .await
    }

    async fn active_data_key(&self, org: OrgId) -> Result<Option<SealedDataKey>, SecretStoreError> {
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let row: Option<DataKeyRow> = sqlx::query_as(
                "SELECT org_id, version, wrapped_dek, kek_version, state, created_at FROM data_keys WHERE org_id = $1 AND state = 'active'",
            )
            .bind(org.as_uuid())
            .fetch_optional(&mut *tx)
            .await?;
            tx.commit().await?;
            Ok::<_, TxError<SecretStoreError>>(row.map(DataKeyRow::into_key).transpose()?)
        })
        .await
    }

    async fn data_key(&self, org: OrgId, version: u32) -> Result<SealedDataKey, SecretStoreError> {
        let version = version_param(version)?;
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let row: Option<DataKeyRow> = sqlx::query_as(
                "SELECT org_id, version, wrapped_dek, kek_version, state, created_at FROM data_keys WHERE org_id = $1 AND version = $2",
            )
            .bind(org.as_uuid())
            .bind(version)
            .fetch_optional(&mut *tx)
            .await?;
            tx.commit().await?;
            let row = row.ok_or(TxError::Port(SecretStoreError::NotFound))?;
            Ok::<_, TxError<SecretStoreError>>(row.into_key()?)
        })
        .await
    }

    async fn list_wraps(&self, after: Option<&WrapRef>, limit: u32) -> Result<Vec<WrapInfo>, SecretStoreError> {
        let limit = i64::from(limit);
        retry_tx(|| async {
            let mut tx = begin_platform(&self.pools.platform).await?;
            let wraps = self.list_wraps_in(&mut tx, after, limit).await?;
            tx.commit().await?;
            Ok::<_, TxError<SecretStoreError>>(wraps)
        })
        .await
    }

    async fn rewrap(&self, r: &WrapRef, expect_kek_version: &str, wrapped_dek: &[u8], kek_version: &str) -> Result<(), SecretStoreError> {
        retry_tx(|| async {
            let mut tx = begin_platform(&self.pools.platform).await?;
            self.rewrap_in(&mut tx, r, expect_kek_version, wrapped_dek, kek_version).await?;
            tx.commit().await?;
            Ok::<_, TxError<SecretStoreError>>(())
        })
        .await
    }

    async fn destroy_org(&self, org: OrgId) -> Result<u64, SecretStoreError> {
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let secrets = sqlx::query("DELETE FROM secrets WHERE org_id = $1").bind(org.as_uuid()).execute(&mut *tx).await?;
            let keys = sqlx::query("DELETE FROM data_keys WHERE org_id = $1").bind(org.as_uuid()).execute(&mut *tx).await?;
            tx.commit().await?;
            Ok::<_, TxError<SecretStoreError>>(secrets.rows_affected() + keys.rows_affected())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use mage_core::testing::SeqIdGen;

    use super::*;
    use crate::testing::contract::{SecretFixture, contract_secret_store};
    use crate::testing::pg::with_pg;

    #[tokio::test]
    async fn pg_secret_store_passes_the_contract() {
        with_pg("pg_secrets_contract", |env| async move {
            contract_secret_store(|clock| {
                let env = env.clone();
                async move {
                    let db = env.fresh().await;
                    let seed = db.seed(clock.clone());
                    SecretFixture { secrets: Arc::new(PgSecretStore::new(db.pools.clone(), clock, Arc::new(SeqIdGen::new()))), seed }
                }
            })
            .await;
        })
        .await;
    }
}
