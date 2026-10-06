//! Real-Postgres harness for the contract suites (storage.md "Testing"). Opt-in: runs only when
//! `DATABASE_URL` points at a PostgreSQL ≥ 16 server whose user may create databases and roles
//! (e.g. Docker `postgres:16` with trust or password auth); otherwise tests print a skip line.
//!
//! [`with_pg`] migrates one template database per test, hands out a fresh copy per scenario
//! ([`PgTestEnv::fresh`]) and drops every database it created afterwards, also when the test
//! panics. The tenant pool of a fresh database logs in as `mage_test_tenant`, a member of the
//! `mage_tenant` group, so row-level security applies to tenant-pool methods exactly as in
//! production; the platform pool uses the `DATABASE_URL` user.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use futures::FutureExt;
use mage_core::domain::{ArtifactId, CustomerId, GateId, OrgId, RepoId, RunId, TicketId, UserId};
use mage_core::ports::clock::Clock;
use mage_infra::pg::PgPools;
use parking_lot::Mutex;
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use uuid::Uuid;

use crate::testing::seed::{Seed, SeedRun};

/// Login role of tenant pools in tests (member of `mage_tenant`).
const TENANT_ROLE: &str = "mage_test_tenant";

/// One test's databases: a migrated template and its fresh copies.
pub struct PgTestEnv {
    base: PgConnectOptions,
    admin: PgPool,
    template: String,
    tenant_login: bool,
    created: Mutex<Vec<String>>,
}

/// A fresh migrated database.
pub struct PgTestDb {
    /// Tenant pool (RLS applies) and platform pool.
    pub pools: PgPools,
    /// Same as `pools.platform`; for seeding and inspection.
    pub admin: PgPool,
}

impl PgTestDb {
    pub fn seed(&self, clock: Arc<dyn Clock>) -> Arc<PgSeed> {
        Arc::new(PgSeed { pool: self.admin.clone(), clock })
    }
}

/// Runs `body` against a [`PgTestEnv`] when `DATABASE_URL` is set; prints a skip line otherwise.
/// Every database created for the test is dropped afterwards, then a panic of `body` is resumed.
pub async fn with_pg<F, Fut>(test: &str, body: F)
where
    F: FnOnce(Arc<PgTestEnv>) -> Fut,
    Fut: Future<Output = ()>,
{
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!("skipping {test}: DATABASE_URL is not set");
        return;
    };
    let env = Arc::new(PgTestEnv::create(&url).await);
    let outcome = std::panic::AssertUnwindSafe(body(env.clone())).catch_unwind().await;
    env.cleanup().await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

impl PgTestEnv {
    async fn create(url: &str) -> PgTestEnv {
        let base = PgConnectOptions::from_str(url).expect("DATABASE_URL");
        let admin = PgPoolOptions::new().max_connections(2).connect_with(base.clone()).await.expect("connect DATABASE_URL");
        let template = format!("mage_test_tmpl_{}", Uuid::new_v4().simple());
        create_database(&admin, &template, None).await;
        let pool = PgPoolOptions::new().max_connections(2).connect_with(base.clone().database(&template)).await.expect("connect template");
        mage_infra::pg::schema::migrate(&pool).await.expect("migrate template");
        pool.close().await;
        let tenant_login = ensure_tenant_role(&admin).await;
        PgTestEnv { base, admin, template, tenant_login, created: Mutex::new(Vec::new()) }
    }

    /// A new database copied from the migrated template. Contract suites run scenarios one after
    /// another, so a new database ends the previous one: it is dropped `WITH (FORCE)`, which also
    /// closes connections its pools still hold (keeps the server under `max_connections`).
    pub async fn fresh(&self) -> PgTestDb {
        let previous: Vec<String> = std::mem::take(&mut *self.created.lock());
        for name in previous {
            drop_database(&self.admin, &name).await;
        }
        let name = format!("mage_test_{}", Uuid::new_v4().simple());
        create_database(&self.admin, &name, Some(&self.template)).await;
        self.created.lock().push(name.clone());
        let platform = PgPoolOptions::new().max_connections(4).connect_with(self.base.clone().database(&name)).await.expect("connect fresh db");
        let tenant = if self.tenant_login {
            let opts = self.base.clone().database(&name).username(TENANT_ROLE).password(TENANT_ROLE);
            PgPoolOptions::new().max_connections(4).connect_with(opts).await.expect("connect fresh db as the tenant role")
        } else {
            platform.clone()
        };
        PgTestDb { pools: PgPools::new(tenant, platform.clone()), admin: platform }
    }

    async fn cleanup(&self) {
        let mut names: Vec<String> = std::mem::take(&mut *self.created.lock());
        names.push(self.template.clone());
        for name in names {
            drop_database(&self.admin, &name).await;
        }
        self.admin.close().await;
    }
}

async fn drop_database(admin: &PgPool, name: &str) {
    let sql = format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)");
    if let Err(err) = sqlx::raw_sql(&sql).execute(admin).await {
        eprintln!("could not drop test database {name}: {err}");
    }
}

async fn create_database(admin: &PgPool, name: &str, template: Option<&str>) {
    let sql = match template {
        Some(t) => format!("CREATE DATABASE \"{name}\" TEMPLATE \"{t}\""),
        None => format!("CREATE DATABASE \"{name}\""),
    };
    sqlx::raw_sql(&sql).execute(admin).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
}

/// Creates the tenant login role if needed (roles are cluster-wide; the migrations created
/// `mage_tenant`). Without the right to do so the tenant pool falls back to the platform user
/// (RLS not exercised) and a note is printed.
async fn ensure_tenant_role(admin: &PgPool) -> bool {
    let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = $1)")
        .bind(TENANT_ROLE)
        .fetch_one(admin)
        .await
        .expect("pg_roles");
    if !exists {
        let sql = format!("CREATE ROLE {TENANT_ROLE} LOGIN PASSWORD '{TENANT_ROLE}' IN ROLE mage_tenant");
        if let Err(err) = sqlx::raw_sql(&sql).execute(admin).await {
            let raced: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = $1)")
                .bind(TENANT_ROLE)
                .fetch_one(admin)
                .await
                .expect("pg_roles");
            if !raced {
                eprintln!("note: cannot create {TENANT_ROLE} ({err}); tenant pool uses the DATABASE_URL user, RLS not exercised");
                return false;
            }
        }
    }
    true
}

/// [`Seed`] over a real database (platform user, bypasses RLS).
pub struct PgSeed {
    pool: PgPool,
    clock: Arc<dyn Clock>,
}

impl PgSeed {
    async fn exec(&self, what: &str, query: sqlx::query::Query<'_, sqlx::Postgres, sqlx::postgres::PgArguments>) {
        query.execute(&self.pool).await.unwrap_or_else(|e| panic!("seed {what}: {e}"));
    }
}

/// Stable positive `bigint` from a uuid (seeded external ids).
fn small_id(uuid: Uuid) -> i64 {
    (uuid.as_u128() as u64 & 0x3fff_ffff_ffff_ffff) as i64
}

#[async_trait]
impl Seed for PgSeed {
    async fn org(&self, org: OrgId) {
        let id = org.as_uuid();
        let q = sqlx::query(
            "INSERT INTO orgs (id, slug, name, state, retention, created_at) \
             VALUES ($1, $2, 'Seed org', 'active', \
                     '{\"run_artifacts_days\":90,\"run_events_days\":90,\"keep_referenced_artifacts\":true}', $3) \
             ON CONFLICT DO NOTHING",
        )
        .bind(id)
        .bind(format!("seed-{}", id.simple()))
        .bind(self.clock.now());
        self.exec("org", q).await;
    }

    async fn user(&self, user: UserId) {
        let id = user.as_uuid();
        let q = sqlx::query("INSERT INTO users (id, email, name, created_at) VALUES ($1, $2, 'Seed user', $3) ON CONFLICT DO NOTHING")
            .bind(id)
            .bind(format!("{}@seed.test", id.simple()))
            .bind(self.clock.now());
        self.exec("user", q).await;
    }

    async fn repo(&self, org: OrgId, repo: RepoId) {
        let id = repo.as_uuid();
        let q = sqlx::query(
            "INSERT INTO repos (org_id, id, external_id, owner, name, default_branch, installation_id, access, created_at) \
             VALUES ($1, $2, $3, 'seed', $4, 'main', 1, 'granted', $5) ON CONFLICT DO NOTHING",
        )
        .bind(org.as_uuid())
        .bind(id)
        .bind(small_id(id))
        .bind(format!("repo-{}", id.simple()))
        .bind(self.clock.now());
        self.exec("repo", q).await;
    }

    async fn run(&self, org: OrgId, run: RunId, subject: SeedRun) {
        let (kind, ticket, cause, repo, sha) = match subject {
            SeedRun::Triage { ticket } => ("triage", Some(ticket.as_uuid()), None, None, None),
            SeedRun::Diagnose { cause, repo, sha } => ("diagnose", None, Some(cause.as_uuid()), Some(repo.as_uuid()), Some(sha.as_bytes().to_vec())),
            SeedRun::Fix { cause, repo, sha } => ("fix", None, Some(cause.as_uuid()), Some(repo.as_uuid()), Some(sha.as_bytes().to_vec())),
        };
        let q = sqlx::query(
            "INSERT INTO runs (org_id, id, kind, ticket_id, cause_id, repo_id, sha, status, attempt, version, spec, slot_class, \
                               priority, source_ready, budget, usage_reported, usage_metered, created_at, finished_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, 'succeeded', 1, 1, '{}', 'work', 0, true, '{}', '{}', '{}', $8, $8) \
             ON CONFLICT DO NOTHING",
        )
        .bind(org.as_uuid())
        .bind(run.as_uuid())
        .bind(kind)
        .bind(ticket)
        .bind(cause)
        .bind(repo)
        .bind(sha)
        .bind(self.clock.now());
        self.exec("run", q).await;
    }

    async fn ticket(&self, org: OrgId, ticket: TicketId) {
        let id = ticket.as_uuid();
        let q = sqlx::query(
            "INSERT INTO tickets (org_id, id, source, source_issue_id, source_identifier, source_url, title, body, content_hash, \
                                  status, rev, created_at, updated_at) \
             VALUES ($1, $2, 'linear', $3, 'SEED-1', 'https://linear.app/seed/issue/SEED-1', 'Seed ticket', '', $4, 'new', 1, $5, $5) \
             ON CONFLICT DO NOTHING",
        )
        .bind(org.as_uuid())
        .bind(id)
        .bind(id.to_string())
        .bind(vec![0u8; 32])
        .bind(self.clock.now());
        self.exec("ticket", q).await;
    }

    async fn customer(&self, org: OrgId, customer: CustomerId) {
        let q = sqlx::query("INSERT INTO customers (org_id, id, name, created_at, updated_at) VALUES ($1, $2, 'Seed customer', $3, $3) ON CONFLICT DO NOTHING")
            .bind(org.as_uuid())
            .bind(customer.as_uuid())
            .bind(self.clock.now());
        self.exec("customer", q).await;
    }

    async fn gate(&self, org: OrgId, gate: GateId, run: RunId) {
        let q = sqlx::query(
            "INSERT INTO gates (org_id, id, run_id, kind, attempt, status, created_at, decided_at) \
             VALUES ($1, $2, $3, 'repro', \
                     (SELECT COALESCE(max(attempt), 0) + 1 FROM gates WHERE org_id = $1 AND run_id = $3 AND kind = 'repro'), \
                     'passed', $4, $4) \
             ON CONFLICT DO NOTHING",
        )
        .bind(org.as_uuid())
        .bind(gate.as_uuid())
        .bind(run.as_uuid())
        .bind(self.clock.now());
        self.exec("gate", q).await;
    }

    async fn artifact(&self, org: OrgId, artifact: ArtifactId, run: RunId) {
        let name = format!("a-{}", artifact.as_uuid().simple());
        let key = format!("org/{}/runs/{}/fix_brief/{name}", org.as_uuid(), run.as_uuid());
        let q = sqlx::query(
            "INSERT INTO artifacts (org_id, id, run_id, attempt, kind, name, key, content_type, state, created_at) \
             VALUES ($1, $2, $3, 1, 'fix_brief', $4, $5, 'text/markdown', 'pending', $6) ON CONFLICT DO NOTHING",
        )
        .bind(org.as_uuid())
        .bind(artifact.as_uuid())
        .bind(run.as_uuid())
        .bind(name)
        .bind(key)
        .bind(self.clock.now());
        self.exec("artifact", q).await;
    }
}
