//! `WebhookInbox` contract (storage.md "Webhook inbox semantics", "Testing" → `WebhookInbox`;
//! intake.md "Port contract tests"): atomic accept with its job, both unique keys, tenant
//! resolution once, outcomes, purge in batches.

use std::sync::Arc;
use std::time::Duration;

use mage_core::domain::{JobKind, JobLane, JobPayload, OrgId};
use mage_core::ports::clock::Clock;
use mage_core::ports::jobs::{JobQueue, LeaseRequest, LeasedJob, NewJob};
use mage_core::testing::FakeClock;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::ports::inbox::{Accepted, Delivery, INBOX_PURGE_BATCH, InboxError, InboxOutcome, NewDelivery, WebhookInbox, WebhookSource};
use crate::testing::seed::Seed;

/// A fresh, empty inbox and the job queue its `accept` enqueues into.
pub struct InboxFixture {
    pub inbox: Arc<dyn WebhookInbox>,
    pub jobs: Arc<dyn JobQueue>,
    pub seed: Arc<dyn Seed>,
}

/// Runs every `WebhookInbox` scenario, each against a fresh fixture from `make`.
pub async fn contract_webhook_inbox<F, Fut>(make: F)
where
    F: Fn(Arc<FakeClock>) -> Fut,
    Fut: Future<Output = InboxFixture>,
{
    macro_rules! scenario {
        ($name:ident) => {{
            let clock = Arc::new(FakeClock::new());
            let fixture = make(clock.clone()).await;
            $name(&clock, &fixture).await;
        }};
    }
    scenario!(accept_stores_the_row_and_enqueues_its_job);
    scenario!(accept_with_the_wrong_job_is_invalid_and_stores_nothing);
    scenario!(duplicates_by_delivery_id_and_by_body_hash);
    scenario!(unknown_deliveries_are_not_found);
    scenario!(resolve_org_sets_the_tenant_once);
    scenario!(mark_processed_records_each_outcome);
    scenario!(purge_drops_processed_bodies_then_old_rows);
    scenario!(purge_works_through_more_than_one_batch);
}

fn org(n: u128) -> OrgId {
    OrgId::from_uuid(Uuid::from_u128(0x0a00 + n))
}

fn sha(body: &[u8]) -> [u8; 32] {
    Sha256::digest(body).into()
}

fn delivery(source: WebhookSource, id: &str, event: &str, org_id: Option<OrgId>, body: &[u8]) -> NewDelivery {
    NewDelivery { source, delivery_id: id.into(), event: event.into(), org_id, body: body.to_vec() }
}

/// The intake job `accept` requires for `(source, id)`.
fn job(source: WebhookSource, id: &str) -> NewJob {
    NewJob::new(JobPayload::IntakeDelivery { source, delivery_id: id.into() })
}

async fn accept(inbox: &dyn WebhookInbox, source: WebhookSource, id: &str, body: &[u8]) -> Accepted {
    inbox.accept(delivery(source, id, "event", None, body), job(source, id)).await.unwrap_or_else(|e| panic!("accept {id}: {e}"))
}

async fn leased(jobs: &dyn JobQueue) -> Vec<LeasedJob> {
    jobs.lease(&LeaseRequest { lane: JobLane::Server, kinds: None, worker: "contract".into(), max: 100 }).await.expect("lease")
}

async fn accept_stores_the_row_and_enqueues_its_job(clock: &FakeClock, f: &InboxFixture) {
    let t0 = clock.now();
    let body = br#"{"action":"create","type":"Issue"}"#;
    let accepted = f.inbox.accept(delivery(WebhookSource::Linear, "lin-1", "Issue", None, body), job(WebhookSource::Linear, "lin-1")).await;
    let Ok(Accepted::New { job_id }) = accepted else { panic!("a new delivery is accepted as new: {accepted:?}") };
    assert_eq!(
        f.inbox.get(WebhookSource::Linear, "lin-1").await,
        Ok(Delivery {
            source: WebhookSource::Linear,
            delivery_id: "lin-1".into(),
            event: "Issue".into(),
            org_id: None,
            received_at: t0,
            body: Some(body.to_vec()),
            body_sha256: sha(body),
            outcome: None,
            processed_at: None,
        }),
        "the adapter computes body_sha256 and received_at"
    );

    let jobs = leased(&*f.jobs).await;
    assert_eq!(jobs.len(), 1, "exactly the delivery's job is queued: {jobs:?}");
    assert_eq!(jobs[0].lease.job_id, job_id, "accept returns the id of the job it enqueued");
    assert_eq!(jobs[0].kind, JobKind::IntakeLinear);
    assert_eq!(jobs[0].org_id, None, "the tenant is resolved by the job");
    assert_eq!(jobs[0].payload, JobPayload::IntakeDelivery { source: WebhookSource::Linear, delivery_id: "lin-1".into() });

    f.seed.org(org(1)).await;
    clock.advance(Duration::from_secs(5));
    let body = br#"{"event":{"type":"message"}}"#;
    let accepted = f.inbox.accept(delivery(WebhookSource::Slack, "Ev01", "message", Some(org(1)), body), job(WebhookSource::Slack, "Ev01")).await;
    assert!(matches!(accepted, Ok(Accepted::New { .. })), "{accepted:?}");
    let stored = f.inbox.get(WebhookSource::Slack, "Ev01").await.unwrap();
    assert_eq!((stored.org_id, stored.received_at, stored.event.as_str()), (Some(org(1)), clock.now(), "message"), "a known tenant is stored at accept");
    let jobs = leased(&*f.jobs).await;
    assert_eq!(jobs.iter().map(|j| j.kind).collect::<Vec<_>>(), vec![JobKind::IntakeSlack]);
}

async fn accept_with_the_wrong_job_is_invalid_and_stores_nothing(_clock: &FakeClock, f: &InboxFixture) {
    let body = br#"{"zen":"Keep it logically awesome."}"#;
    let source = WebhookSource::GitHub;
    let wrong: Vec<(&str, NewJob)> = vec![
        ("another delivery's job", job(source, "gh-2")),
        ("another source's job", job(WebhookSource::GitHubFix, "gh-1")),
        ("a non-intake job", NewJob::new(JobPayload::IntakeInboxPurge)),
        ("a job whose kind is not its payload's kind", NewJob { kind: JobKind::IntakeGithubFix, ..job(source, "gh-1") }),
    ];
    for (what, bad) in wrong {
        let out = f.inbox.accept(delivery(source, "gh-1", "ping", None, body), bad).await;
        assert!(matches!(out, Err(InboxError::Invalid(_))), "{what}: {out:?}");
        assert_eq!(f.inbox.get(source, "gh-1").await, Err(InboxError::NotFound), "{what}: no row without its job");
        assert!(leased(&*f.jobs).await.is_empty(), "{what}: no job without its row");
    }
    assert!(
        matches!(accept(&*f.inbox, source, "gh-1", body).await, Accepted::New { .. }),
        "nothing of the failed accepts makes the delivery a duplicate"
    );
    assert_eq!(leased(&*f.jobs).await.len(), 1);
}

async fn duplicates_by_delivery_id_and_by_body_hash(clock: &FakeClock, f: &InboxFixture) {
    let (a, b) = (br#"{"n":"a"}"#.as_slice(), br#"{"n":"b"}"#.as_slice());
    let t0 = clock.now();
    assert!(matches!(accept(&*f.inbox, WebhookSource::Linear, "d-1", a).await, Accepted::New { .. }));

    clock.advance(Duration::from_secs(60));
    let t1 = clock.now();
    assert_eq!(accept(&*f.inbox, WebhookSource::Linear, "d-1", b).await, Accepted::Duplicate { first_seen_at: t0 }, "same delivery id");
    assert_eq!(
        accept(&*f.inbox, WebhookSource::Linear, "d-forged", a).await,
        Accepted::Duplicate { first_seen_at: t0 },
        "the same body replayed under a forged delivery id"
    );
    assert_eq!(f.inbox.get(WebhookSource::Linear, "d-forged").await, Err(InboxError::NotFound), "a duplicate stores nothing");
    assert_eq!(f.inbox.get(WebhookSource::Linear, "d-1").await.unwrap().body.as_deref(), Some(a), "the first body stays");
    assert!(matches!(accept(&*f.inbox, WebhookSource::GitHub, "d-1", a).await, Accepted::New { .. }), "both keys are per source");
    assert!(matches!(accept(&*f.inbox, WebhookSource::Linear, "d-2", b).await, Accepted::New { .. }));

    clock.advance(Duration::from_secs(60));
    assert_eq!(
        accept(&*f.inbox, WebhookSource::Linear, "d-2", a).await,
        Accepted::Duplicate { first_seen_at: t0 },
        "matching d-2 by id (t1) and d-1 by hash (t0) reports the earliest"
    );
    assert_eq!(accept(&*f.inbox, WebhookSource::Linear, "d-3", b).await, Accepted::Duplicate { first_seen_at: t1 });

    let mut queued: Vec<JobPayload> = leased(&*f.jobs).await.into_iter().map(|j| j.payload).collect();
    queued.sort_by_key(|p| format!("{p:?}"));
    let mut expected = vec![
        JobPayload::IntakeDelivery { source: WebhookSource::Linear, delivery_id: "d-1".into() },
        JobPayload::IntakeDelivery { source: WebhookSource::GitHub, delivery_id: "d-1".into() },
        JobPayload::IntakeDelivery { source: WebhookSource::Linear, delivery_id: "d-2".into() },
    ];
    expected.sort_by_key(|p| format!("{p:?}"));
    assert_eq!(queued, expected, "one job per new delivery, none per duplicate");
}

async fn unknown_deliveries_are_not_found(_clock: &FakeClock, f: &InboxFixture) {
    f.seed.org(org(1)).await;
    accept(&*f.inbox, WebhookSource::GitHub, "gh-1", b"{}").await;
    assert_eq!(f.inbox.get(WebhookSource::GitHub, "gh-2").await, Err(InboxError::NotFound));
    assert_eq!(f.inbox.get(WebhookSource::GitHubFix, "gh-1").await, Err(InboxError::NotFound), "the key includes the source");
    assert_eq!(f.inbox.resolve_org(WebhookSource::GitHub, "gh-2", org(1)).await, Err(InboxError::NotFound));
    assert_eq!(f.inbox.mark_processed(WebhookSource::GitHub, "gh-2", InboxOutcome::Processed).await, Err(InboxError::NotFound));
}

async fn resolve_org_sets_the_tenant_once(_clock: &FakeClock, f: &InboxFixture) {
    f.seed.org(org(1)).await;
    f.seed.org(org(2)).await;
    accept(&*f.inbox, WebhookSource::GitHub, "gh-1", b"{\"installation\":{\"id\":1}}").await;
    assert_eq!(f.inbox.resolve_org(WebhookSource::GitHub, "gh-1", org(1)).await, Ok(()));
    assert_eq!(f.inbox.get(WebhookSource::GitHub, "gh-1").await.unwrap().org_id, Some(org(1)));
    assert_eq!(f.inbox.resolve_org(WebhookSource::GitHub, "gh-1", org(1)).await, Ok(()), "the same org again is a no-op");
    assert_eq!(f.inbox.resolve_org(WebhookSource::GitHub, "gh-1", org(2)).await, Err(InboxError::Conflict));
    assert_eq!(f.inbox.get(WebhookSource::GitHub, "gh-1").await.unwrap().org_id, Some(org(1)), "a conflicting resolve changes nothing");

    f.inbox.accept(delivery(WebhookSource::Linear, "lin-1", "Issue", Some(org(2)), b"{}"), job(WebhookSource::Linear, "lin-1")).await.unwrap();
    assert_eq!(f.inbox.resolve_org(WebhookSource::Linear, "lin-1", org(1)).await, Err(InboxError::Conflict), "a tenant set at accept counts");
}

async fn mark_processed_records_each_outcome(clock: &FakeClock, f: &InboxFixture) {
    let outcomes = [
        ("p", InboxOutcome::Processed),
        ("i", InboxOutcome::Ignored("bot comment".into())),
        ("r", InboxOutcome::Rejected("unknown installation".into())),
    ];
    for (id, _) in &outcomes {
        accept(&*f.inbox, WebhookSource::Linear, id, id.as_bytes()).await;
    }
    for (id, outcome) in &outcomes {
        clock.advance(Duration::from_secs(1));
        assert_eq!(f.inbox.mark_processed(WebhookSource::Linear, id, outcome.clone()).await, Ok(()));
        let stored = f.inbox.get(WebhookSource::Linear, id).await.unwrap();
        assert_eq!((stored.outcome, stored.processed_at), (Some(outcome.clone()), Some(clock.now())), "{id}");
        assert_eq!(stored.body.as_deref(), Some(id.as_bytes()), "{id}: processing keeps the body");
    }
}

async fn purge_drops_processed_bodies_then_old_rows(clock: &FakeClock, f: &InboxFixture) {
    let t0 = clock.now();
    accept(&*f.inbox, WebhookSource::GitHub, "early-processed", b"a").await;
    accept(&*f.inbox, WebhookSource::GitHub, "unprocessed", b"b").await;
    clock.advance(Duration::from_secs(3600));
    f.inbox.mark_processed(WebhookSource::GitHub, "early-processed", InboxOutcome::Processed).await.unwrap();
    clock.advance(Duration::from_secs(3600));
    let t2 = clock.now();
    accept(&*f.inbox, WebhookSource::GitHub, "late-processed", b"c").await;
    f.inbox.mark_processed(WebhookSource::GitHub, "late-processed", InboxOutcome::Ignored("ping".into())).await.unwrap();

    assert_eq!(f.inbox.purge(t2, t0).await, Ok(1), "only the body processed before the cutoff");
    let early = f.inbox.get(WebhookSource::GitHub, "early-processed").await.unwrap();
    assert_eq!((early.body, early.body_sha256, early.outcome), (None, sha(b"a"), Some(InboxOutcome::Processed)), "the row and its hash stay");
    assert_eq!(f.inbox.get(WebhookSource::GitHub, "unprocessed").await.unwrap().body.as_deref(), Some(b"b".as_slice()), "unprocessed bodies stay");
    assert_eq!(f.inbox.get(WebhookSource::GitHub, "late-processed").await.unwrap().body.as_deref(), Some(b"c".as_slice()), "the cutoff is exclusive");
    assert_eq!(f.inbox.purge(t2, t0).await, Ok(0), "purge is idempotent");
    assert_eq!(
        accept(&*f.inbox, WebhookSource::GitHub, "replayed", b"a").await,
        Accepted::Duplicate { first_seen_at: t0 },
        "a purged body still deduplicates by hash"
    );

    clock.advance(Duration::from_secs(1));
    assert_eq!(f.inbox.purge(clock.now(), t2).await, Ok(3), "one more body, then the two rows received before t2");
    assert_eq!(f.inbox.get(WebhookSource::GitHub, "early-processed").await, Err(InboxError::NotFound));
    assert_eq!(f.inbox.get(WebhookSource::GitHub, "unprocessed").await, Err(InboxError::NotFound), "row purge removes unprocessed rows too");
    let late = f.inbox.get(WebhookSource::GitHub, "late-processed").await.unwrap();
    assert_eq!((late.body, late.received_at), (None, t2));
    assert!(
        matches!(accept(&*f.inbox, WebhookSource::GitHub, "early-processed", b"a").await, Accepted::New { .. }),
        "after row purge the delivery is new again"
    );
}

async fn purge_works_through_more_than_one_batch(clock: &FakeClock, f: &InboxFixture) {
    let t0 = clock.now();
    let n = INBOX_PURGE_BATCH as usize + 1;
    for i in 0..n {
        let id = format!("d-{i}");
        accept(&*f.inbox, WebhookSource::Slack, &id, id.as_bytes()).await;
        f.inbox.mark_processed(WebhookSource::Slack, &id, InboxOutcome::Processed).await.unwrap();
    }
    clock.advance(Duration::from_secs(1));
    assert_eq!(f.inbox.purge(clock.now(), t0).await, Ok(n as u64), "every processed body, across batches");
    assert_eq!(f.inbox.get(WebhookSource::Slack, &format!("d-{}", n - 1)).await.unwrap().body, None);
    assert_eq!(f.inbox.purge(clock.now(), clock.now()).await, Ok(n as u64), "every old row, across batches");
    assert_eq!(f.inbox.get(WebhookSource::Slack, "d-0").await, Err(InboxError::NotFound));
}

#[cfg(test)]
mod tests {
    use mage_core::testing::{MemJobQueue, SeqIdGen};

    use super::*;
    use crate::testing::{MemWebhookInbox, NoSeed};

    #[tokio::test]
    async fn mem_webhook_inbox_passes_the_contract() {
        contract_webhook_inbox(|clock| async move {
            let jobs = Arc::new(MemJobQueue::new(clock.clone(), Arc::new(SeqIdGen::new())));
            InboxFixture { inbox: Arc::new(MemWebhookInbox::new(clock, jobs.clone())), jobs, seed: Arc::new(NoSeed) }
        })
        .await;
    }
}
