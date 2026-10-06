//! Postgres harness for the `DATABASE_URL`-gated tests: a fresh database per test (migrated with
//! `mage_infra::pg::schema::migrate`, dropped afterwards even when the test panics) and
//! role-separated pools, so the adapters run with the grants and RLS of production:
//!
//! - `pools.tenant`: every connection runs `SET ROLE mage_tenant` (RLS on, tenant grants);
//! - `pools.platform`: every connection runs `SET ROLE mage_platform`;
//! - `owner`: the migrating (superuser) connection, for fixtures such as `orgs` rows.
//!
//! `DATABASE_URL` must name a PostgreSQL >= 16 server whose role may create databases and
//! `SET ROLE` to the group roles (a superuser, e.g. Docker
//! `docker run -d --name mage-pg-test -e POSTGRES_PASSWORD=pg -p 55432:5432 postgres:16` with
//! `DATABASE_URL=postgres://postgres:pg@localhost:55432/postgres`). Unset → the test prints why
//! and passes.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::panic::{AssertUnwindSafe, resume_unwind};
use std::str::FromStr;

use futures::FutureExt;
use mage_infra::pg::PgPools;
use mage_infra::pg::schema::migrate;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Connection, PgConnection, PgPool};
use uuid::Uuid;

pub struct TestDb {
    /// Superuser pool on the test database (fixtures, inspection).
    pub owner: PgPool,
    /// Role-separated adapter pools.
    pub pools: PgPools,
}

impl TestDb {
    /// Insert `orgs` rows (platform table) for the given ids; existing ones are kept.
    pub async fn orgs(&self, ids: &[Uuid]) {
        for id in ids {
            sqlx::query(
                "INSERT INTO orgs (id, slug, name, state, retention, created_at) \
                 VALUES ($1, $2, $2, 'active', '{}'::jsonb, now()) ON CONFLICT DO NOTHING",
            )
            .bind(id)
            .bind(format!("org-{}", id.simple()))
            .execute(&self.owner)
            .await
            .expect("insert org");
        }
    }

    /// `TRUNCATE … CASCADE` the given tables (fresh store per contract scenario).
    pub async fn truncate(&self, tables: &[&str]) {
        sqlx::raw_sql(&format!("TRUNCATE {} CASCADE", tables.join(", "))).execute(&self.owner).await.expect("truncate");
    }
}

/// Run `body` against a fresh migrated database, or skip when `DATABASE_URL` is unset.
pub async fn with_test_db<F, Fut>(test: &str, body: F)
where
    F: FnOnce(TestDb) -> Fut,
    Fut: Future<Output = ()>,
{
    let Some(url) = std::env::var("DATABASE_URL").ok().filter(|u| !u.is_empty()) else {
        eprintln!("skipping {test}: DATABASE_URL is not set (PostgreSQL >= 16; the test creates and drops its own database)");
        return;
    };
    let admin_opts = PgConnectOptions::from_str(&url).expect("DATABASE_URL parses");
    let mut admin = PgConnection::connect_with(&admin_opts).await.expect("connect DATABASE_URL");
    let name = format!("mage_infra_{}", Uuid::new_v4().simple());
    sqlx::raw_sql(&format!(r#"CREATE DATABASE "{name}""#)).execute(&mut admin).await.expect("create test database");

    let opts = admin_opts.clone().database(&name);
    let owner = PgPoolOptions::new().max_connections(4).connect_with(opts.clone()).await.expect("connect test database");
    migrate(&owner).await.expect("migrations apply");
    let tenant = role_pool(opts.clone(), "mage_tenant").await;
    let platform = role_pool(opts, "mage_platform").await;
    let db = TestDb { owner: owner.clone(), pools: PgPools::new(tenant.clone(), platform.clone()) };

    let outcome = AssertUnwindSafe(body(db)).catch_unwind().await;

    tenant.close().await;
    platform.close().await;
    owner.close().await;
    sqlx::raw_sql(&format!(r#"DROP DATABASE "{name}" WITH (FORCE)"#)).execute(&mut admin).await.expect("drop test database");
    let _ = admin.close().await;
    if let Err(panic) = outcome {
        resume_unwind(panic);
    }
}

async fn role_pool(opts: PgConnectOptions, role: &'static str) -> PgPool {
    let set_role: &'static str = match role {
        "mage_tenant" => "SET ROLE mage_tenant",
        _ => "SET ROLE mage_platform",
    };
    PgPoolOptions::new()
        .max_connections(12)
        .after_connect(move |conn, _meta| {
            Box::pin(async move {
                sqlx::Executor::execute(&mut *conn, set_role).await?;
                Ok(())
            })
        })
        .connect_with(opts)
        .await
        .expect("connect role pool")
}
