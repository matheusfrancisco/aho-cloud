//! `PgJobQueue` against Postgres 16: the shared `JobQueue` contract plus the Postgres-only
//! guarantees (NOTIFY at commit, outbox inside a caller's transaction, tenant-role grants).
//! `DATABASE_URL`-gated; see `support` for the harness.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;
use std::time::Duration;

use mage_core::domain::{JobPayload, OrgId};
use mage_core::ports::clock::Clock;
use mage_core::ports::jobs::{JobQueue, NewJob};
use mage_core::testing::{FakeClock, SeqIdGen, contract_job_queue};
use mage_core::domain::JobLane;
use mage_infra::pg::jobs::{JOBS_CHANNEL, enqueue_all_in, spawn_wakeup_listener};
use mage_infra::pg::{DbError, PgJobQueue, begin_org};
use sqlx::postgres::PgListener;
use support::with_test_db;
use uuid::Uuid;

/// Orgs the contract suite uses (`org(n)` = 0x0100 + n).
fn contract_orgs() -> Vec<Uuid> {
    (1..=3).map(|n| Uuid::from_u128(0x0100 + n)).collect()
}

#[tokio::test]
async fn pg_job_queue_passes_the_contract() {
    with_test_db("pg_job_queue_passes_the_contract", |db| async move {
        db.orgs(&contract_orgs()).await;
        let db = &db;
        contract_job_queue(|clock| async move {
            db.truncate(&["jobs"]).await;
            Arc::new(PgJobQueue::new(db.pools.clone(), clock, Arc::new(SeqIdGen::new()))) as Arc<dyn JobQueue>
        })
        .await;
    })
    .await;
}

#[tokio::test]
async fn outbox_jobs_commit_and_notify_with_the_callers_transaction() {
    with_test_db("outbox_jobs_commit_and_notify_with_the_callers_transaction", |db| async move {
        let org = OrgId::from_uuid(Uuid::from_u128(0x0901));
        db.orgs(&[org.as_uuid()]).await;
        let clock = Arc::new(FakeClock::new());
        let ids = SeqIdGen::new();
        let queue = PgJobQueue::new(db.pools.clone(), clock.clone(), Arc::new(SeqIdGen::new()));
        let mut listener = PgListener::connect_with(&db.owner).await.unwrap();
        listener.listen(JOBS_CHANNEL).await.unwrap();

        // Rolled back: no job, no notification.
        let mut tx = begin_org(&db.pools.tenant, org).await.unwrap();
        let job = NewJob::new(JobPayload::OpsRetention { org_id: org });
        let first = enqueue_all_in(&mut tx, std::slice::from_ref(&job), clock.now(), &ids).await.unwrap();
        tx.rollback().await.unwrap();
        assert!(queue.get(first[0].job_id()).await.is_err(), "a rolled-back outbox insert leaves no job");
        assert!(tokio::time::timeout(Duration::from_millis(300), listener.recv()).await.is_err(), "no wakeup for a rollback");

        // Committed: the job exists and the lane is notified once the transaction commits.
        let mut tx = begin_org(&db.pools.tenant, org).await.unwrap();
        let kept = enqueue_all_in(&mut tx, &[job.clone(), job.clone()], clock.now(), &ids).await.unwrap();
        assert!(kept[0].is_new());
        assert_eq!(kept[1].job_id(), kept[0].job_id(), "a duplicate inside one outbox is silent");
        assert!(tokio::time::timeout(Duration::from_millis(300), listener.recv()).await.is_err(), "nothing before commit");
        tx.commit().await.unwrap();
        let note = tokio::time::timeout(Duration::from_secs(5), listener.recv()).await.expect("notified at commit").unwrap();
        assert_eq!(note.payload(), "server");
        assert_eq!(queue.get(kept[0].job_id()).await.unwrap().org_id, Some(org));

        // An invalid job aborts the whole outbox before anything is written.
        let mut tx = begin_org(&db.pools.tenant, org).await.unwrap();
        let wrong = NewJob { kind: mage_core::domain::JobKind::IndexGc, ..NewJob::new(JobPayload::OpsPurge) };
        let err = enqueue_all_in(&mut tx, &[NewJob::new(JobPayload::OpsPurge), wrong], clock.now(), &ids).await;
        assert!(matches!(err, Err(DbError::Invalid(_))), "{err:?}");
        tx.rollback().await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn wakeup_listener_wakes_only_its_lane() {
    with_test_db("wakeup_listener_wakes_only_its_lane", |db| async move {
        let org = OrgId::from_uuid(Uuid::from_u128(0x0902));
        db.orgs(&[org.as_uuid()]).await;
        let queue = PgJobQueue::new(db.pools.clone(), Arc::new(FakeClock::new()), Arc::new(SeqIdGen::new()));
        let wake = Arc::new(tokio::sync::Notify::new());
        let shutdown = tokio_util::sync::CancellationToken::new();
        let task = spawn_wakeup_listener(db.owner.clone(), JobLane::Indexer, wake.clone(), shutdown.clone());
        tokio::time::timeout(Duration::from_secs(5), wake.notified()).await.expect("woken once LISTEN is up");

        queue.enqueue(NewJob::new(JobPayload::OpsRetention { org_id: org })).await.unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(300), wake.notified()).await.is_err(), "a server-lane job does not wake the indexer");
        queue.enqueue(NewJob::new(JobPayload::IndexGc)).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), wake.notified()).await.expect("an indexer-lane job wakes the indexer");

        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), task).await.expect("stops on shutdown").unwrap();
    })
    .await;
}
