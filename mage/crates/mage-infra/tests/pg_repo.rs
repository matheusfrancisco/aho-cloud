//! `PgRepoStore` against Postgres 16: the shared `RepoStore` contract (with `PgJobQueue` as the
//! outbox's queue), plus tenancy (RLS on the tenant role) and outbox atomicity on a real
//! database failure. `DATABASE_URL`-gated; see `support` for the harness.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;

use mage_core::domain::{InstallationId, JobPayload, OrgId, Repo, RepoAccess, RepoId};
use mage_core::ports::clock::Clock;
use mage_core::ports::jobs::{JobQueue, NewJob};
use mage_core::ports::repos::{RepoStore, RepoStoreError};
use mage_core::testing::{FakeClock, RepoStoreFixture, SeqIdGen, contract_repo_store};
use mage_infra::pg::{PgJobQueue, PgRepoStore, begin_org};
use support::with_test_db;
use uuid::Uuid;

/// Orgs the contract suite uses (`org(n)` = 0x0200 + n).
fn contract_orgs() -> Vec<Uuid> {
    (1..=3).map(|n| Uuid::from_u128(0x0200 + n)).collect()
}

const TABLES: &[&str] = &["layer_batches", "layer_status", "repo_version_reasons", "repo_versions", "recipes", "deployments", "repos", "jobs"];

#[tokio::test]
async fn pg_repo_store_passes_the_contract() {
    with_test_db("pg_repo_store_passes_the_contract", |db| async move {
        db.orgs(&contract_orgs()).await;
        let db = &db;
        contract_repo_store(|clock| async move {
            db.truncate(TABLES).await;
            let ids = Arc::new(SeqIdGen::new());
            let jobs = Arc::new(PgJobQueue::new(db.pools.clone(), clock.clone(), ids.clone()));
            let repos = Arc::new(PgRepoStore::new(db.pools.clone(), clock, ids));
            RepoStoreFixture { repos: repos as Arc<dyn RepoStore>, jobs: jobs as Arc<dyn JobQueue> }
        })
        .await;
    })
    .await;
}

fn repo(org: OrgId, n: u128, clock: &FakeClock) -> Repo {
    Repo {
        id: RepoId::from_uuid(Uuid::from_u128(0x0a00 + n)),
        org_id: org,
        external_id: 7000 + n as u64,
        owner: "acme".into(),
        name: format!("r{n}"),
        default_branch: "main".into(),
        installation_id: InstallationId(1),
        access: RepoAccess::Granted,
        created_at: clock.now(),
    }
}

#[tokio::test]
async fn tenant_transactions_see_only_their_org() {
    with_test_db("tenant_transactions_see_only_their_org", |db| async move {
        let (a, b) = (OrgId::from_uuid(Uuid::from_u128(0x0a01)), OrgId::from_uuid(Uuid::from_u128(0x0a02)));
        db.orgs(&[a.as_uuid(), b.as_uuid()]).await;
        let clock = Arc::new(FakeClock::new());
        let store = PgRepoStore::new(db.pools.clone(), clock.clone(), Arc::new(SeqIdGen::new()));
        store.upsert_repo(&repo(a, 1, &clock), &[]).await.unwrap();
        store.upsert_repo(&repo(b, 2, &clock), &[]).await.unwrap();

        // RLS on the tenant role, even for a statement without an org filter.
        let count = |org: Option<OrgId>| {
            let pool = db.pools.tenant.clone();
            async move {
                let mut tx = match org {
                    Some(org) => begin_org(&pool, org).await.unwrap(),
                    None => pool.begin().await.unwrap(),
                };
                let n: i64 = sqlx::query_scalar("SELECT count(*) FROM repos").fetch_one(&mut *tx).await.unwrap();
                tx.rollback().await.unwrap();
                n
            }
        };
        assert_eq!(count(Some(a)).await, 1, "org A sees its own repo only");
        assert_eq!(count(Some(b)).await, 1);
        assert_eq!(count(None).await, 0, "a tenant connection without mage.org_id sees zero rows");

        // A tenant transaction scoped to A cannot write a row of B.
        let mut tx = begin_org(&db.pools.tenant, a).await.unwrap();
        let written = sqlx::query("UPDATE repos SET access = 'lost' WHERE org_id = $1").bind(b.as_uuid()).execute(&mut *tx).await.unwrap();
        assert_eq!(written.rows_affected(), 0, "B's rows are invisible to A's transaction");
        let insert = sqlx::query(
            "INSERT INTO repos (org_id, id, external_id, owner, name, default_branch, installation_id, access, created_at) \
             VALUES ($1, $2, 1, 'x', 'y', 'main', 1, 'granted', now())",
        )
        .bind(b.as_uuid())
        .bind(Uuid::from_u128(0x0aff))
        .execute(&mut *tx)
        .await;
        assert!(insert.is_err(), "WITH CHECK refuses a row of another org");
        tx.rollback().await.unwrap();

        assert_eq!(store.repo(b, repo(b, 2, &clock).id).await.map(|r| r.access), Ok(RepoAccess::Granted));
        assert_eq!(store.repo(a, repo(b, 2, &clock).id).await, Err(RepoStoreError::NotFound));
    })
    .await;
}

#[tokio::test]
async fn a_failed_outbox_insert_rolls_the_state_write_back() {
    with_test_db("a_failed_outbox_insert_rolls_the_state_write_back", |db| async move {
        let org = OrgId::from_uuid(Uuid::from_u128(0x0b01));
        db.orgs(&[org.as_uuid()]).await;
        let clock = Arc::new(FakeClock::new());
        let ids = Arc::new(SeqIdGen::new());
        let store = PgRepoStore::new(db.pools.clone(), clock.clone(), ids.clone());
        let queue = PgJobQueue::new(db.pools.clone(), clock.clone(), ids);

        // Valid by itself, but its org has no `orgs` row: the jobs insert fails in the database
        // after the repo row was written in the same transaction.
        let ghost = OrgId::from_uuid(Uuid::from_u128(0x0bff));
        let job = NewJob::new(JobPayload::IntakeGithubResync { org_id: ghost });
        let r = repo(org, 1, &clock);
        let result = store.upsert_repo(&r, &[job]).await;
        assert!(matches!(&result, Err(RepoStoreError::Invalid(msg)) if msg.contains("dangling reference")), "{result:?}");
        assert_eq!(store.repo(org, r.id).await, Err(RepoStoreError::NotFound), "the repo write rolled back with the job");

        let ok = NewJob::new(JobPayload::IntakeGithubResync { org_id: org });
        store.upsert_repo(&r, std::slice::from_ref(&ok)).await.unwrap();
        assert_eq!(queue.enqueue(ok).await.map(|e| e.is_new()), Ok(false), "the job committed with the repo");
    })
    .await;
}
