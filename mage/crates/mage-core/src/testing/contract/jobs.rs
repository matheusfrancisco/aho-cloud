//! `JobQueue` contract (storage.md "Testing" → `JobQueue`).

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use uuid::Uuid;

use crate::domain::ids::{CommitSha, JobId, OrgId, RepoId, RunId, TicketId};
use crate::domain::job::{IndexPriority, JobKind, JobLane, JobPayload, RetryPolicy};
use crate::ports::clock::Clock;
use crate::ports::common::PageRequest;
use crate::ports::jobs::{
    CancelOutcome, DEFER_LIMIT_ERROR, Enqueued, FailOutcome, Heartbeat, JobError, JobFailure, JobLease, JobQueue, JobState,
    LAST_ERROR_MAX_BYTES, LEASE_EXPIRED_ERROR, LeaseRequest, LeasedJob, NewJob, reap_delay, retry_delay,
};
use crate::testing::clock::FakeClock;
use crate::testing::sync::join_all;

/// Runs every `JobQueue` scenario, each against a fresh queue from `make`.
pub async fn contract_job_queue<F, Fut>(make: F)
where
    F: Fn(Arc<FakeClock>) -> Fut,
    Fut: Future<Output = Arc<dyn JobQueue>>,
{
    macro_rules! scenario {
        ($name:ident) => {{
            let clock = Arc::new(FakeClock::new());
            let queue = make(clock.clone()).await;
            $name(&clock, &*queue).await;
        }};
    }
    scenario!(dedupe_while_active_then_new_after_completion);
    scenario!(concurrent_enqueue_of_one_key_yields_one_new);
    scenario!(enqueue_validates_and_enqueue_all_is_atomic);
    scenario!(lease_filters_and_orders_ready_jobs);
    scenario!(lease_is_exclusive_across_workers);
    scenario!(stale_lease_seq_is_lease_lost_everywhere);
    scenario!(heartbeat_extends_and_reports_cancel);
    scenario!(complete_enqueues_follow_ups_with_the_same_key);
    scenario!(retryable_failures_back_off_exactly_then_die);
    scenario!(permanent_failure_dies_and_truncates_the_error);
    scenario!(defer_keeps_attempts_and_dies_past_max_defers);
    scenario!(reaper_requeues_kills_and_cancels_expired_leases);
    scenario!(cancel_in_each_state);
    scenario!(dead_letter_list_retry_discard);
    scenario!(purge_deletes_old_finished_jobs);
}

fn org(n: u128) -> OrgId {
    OrgId::from_uuid(Uuid::from_u128(0x0100 + n))
}

/// Server lane, `RetryPolicy::DEFAULT`, dedupe key `ops.retention:{org}`.
fn keyed(org: OrgId) -> NewJob {
    NewJob::new(JobPayload::OpsRetention { org_id: org })
}

/// Server lane, no documented dedupe key.
fn unkeyed(org: OrgId, n: u128) -> NewJob {
    NewJob::new(JobPayload::FixOpenPr {
        org_id: org,
        ticket_id: TicketId::from_uuid(Uuid::from_u128(n)),
        fix_run_id: RunId::from_uuid(Uuid::from_u128(n)),
    })
}

/// Indexer lane; priority from `prio`.
fn fetch(org: OrgId, n: u8, prio: IndexPriority) -> NewJob {
    NewJob::new(JobPayload::IndexFetch { org_id: org, repo_id: RepoId::from_uuid(Uuid::from_u128(7)), sha: CommitSha::from_bytes([n; 20]), prio })
}

fn server(max: u16) -> LeaseRequest {
    LeaseRequest { lane: JobLane::Server, kinds: None, worker: "worker-1".into(), max }
}

async fn enqueue(queue: &dyn JobQueue, job: NewJob) -> Enqueued {
    queue.enqueue(job).await.expect("enqueue")
}

async fn lease_one(queue: &dyn JobQueue, req: &LeaseRequest) -> LeasedJob {
    let mut leased = queue.lease(req).await.expect("lease");
    assert_eq!(leased.len(), 1, "expected exactly one leasable job, got {leased:?}");
    leased.remove(0)
}

async fn state_of(queue: &dyn JobQueue, id: JobId) -> JobState {
    queue.get(id).await.expect("get").state
}

async fn dedupe_while_active_then_new_after_completion(clock: &FakeClock, q: &dyn JobQueue) {
    let a = enqueue(q, keyed(org(1))).await;
    let Enqueued::New(first) = a else { panic!("first enqueue must be New, got {a:?}") };
    let first_record = q.get(first).await.unwrap();

    let later = clock.now() + Duration::from_secs(3600);
    assert_eq!(enqueue(q, keyed(org(1)).after(later)).await, Enqueued::Duplicate(first), "duplicate while queued");
    assert_eq!(q.get(first).await.unwrap(), first_record, "a duplicate leaves the existing job untouched");

    let lease = lease_one(q, &server(10)).await;
    assert_eq!(lease.lease.job_id, first);
    assert_eq!(enqueue(q, keyed(org(1))).await, Enqueued::Duplicate(first), "duplicate while running");

    assert!(enqueue(q, keyed(org(2))).await.is_new(), "dedupe keys are scoped per org");

    q.complete(&lease.lease, &[]).await.unwrap();
    assert_eq!(state_of(q, first).await, JobState::Succeeded);
    let again = enqueue(q, keyed(org(1))).await;
    assert!(again.is_new() && again.job_id() != first, "the key is free again after completion: {again:?}");

    let x = enqueue(q, unkeyed(org(1), 1)).await;
    let y = enqueue(q, unkeyed(org(1), 1)).await;
    assert!(x.is_new() && y.is_new() && x.job_id() != y.job_id(), "jobs without a dedupe key are never deduplicated");

    let platform = NewJob::new(JobPayload::IndexGc);
    let p = enqueue(q, platform.clone()).await;
    assert_eq!(enqueue(q, platform).await, Enqueued::Duplicate(p.job_id()), "platform jobs (no org) dedupe too");
    assert_eq!(q.get(p.job_id()).await.unwrap().org_id, None);
    assert_eq!(q.get(JobId::from_uuid(Uuid::from_u128(u128::MAX))).await, Err(JobError::NotFound));
}

async fn concurrent_enqueue_of_one_key_yields_one_new(_: &FakeClock, q: &dyn JobQueue) {
    let results = join_all((0..8).map(|_| q.enqueue(keyed(org(1))))).await;
    let results: Vec<Enqueued> = results.into_iter().map(|r| r.expect("enqueue")).collect();
    let news: Vec<&Enqueued> = results.iter().filter(|e| e.is_new()).collect();
    assert_eq!(news.len(), 1, "exactly one New among {results:?}");
    let id = news[0].job_id();
    assert!(results.iter().all(|e| e.job_id() == id), "every duplicate names the one job: {results:?}");
}

async fn enqueue_validates_and_enqueue_all_is_atomic(_: &FakeClock, q: &dyn JobQueue) {
    let wrong_kind = NewJob { kind: JobKind::IndexGc, ..keyed(org(1)) };
    assert!(matches!(q.enqueue(wrong_kind.clone()).await, Err(JobError::Invalid(_))));
    assert!(matches!(q.enqueue(keyed(org(1)).with_max_attempts(0)).await, Err(JobError::Invalid(_))));

    let batch = q.enqueue_all(&[keyed(org(1)), wrong_kind]).await;
    assert!(matches!(batch, Err(JobError::Invalid(_))), "{batch:?}");
    assert!(enqueue(q, keyed(org(1))).await.is_new(), "a failed enqueue_all inserts nothing");

    let results = q.enqueue_all(&[keyed(org(2)), keyed(org(2)), keyed(org(1)), unkeyed(org(1), 9)]).await.unwrap();
    assert_eq!(results.len(), 4);
    assert!(results[0].is_new());
    assert_eq!(results[1], Enqueued::Duplicate(results[0].job_id()), "duplicates inside one batch");
    assert!(!results[2].is_new(), "org(1) was already queued");
    assert!(results[3].is_new());

    let record = q.get(results[0].job_id()).await.unwrap();
    assert_eq!(record.max_attempts, JobKind::OpsRetention.retry_policy().max_attempts, "max_attempts defaults to the kind's policy");
    assert_eq!(record.attempts, 0);
    assert_eq!(record.dedupe_key, keyed(org(2)).dedupe_key);
}

async fn lease_filters_and_orders_ready_jobs(clock: &FakeClock, q: &dyn JobQueue) {
    let t0 = clock.now();
    let low = enqueue(q, fetch(org(1), 1, IndexPriority::Low)).await.job_id();
    let high = enqueue(q, fetch(org(1), 2, IndexPriority::High)).await.job_id();
    clock.advance(Duration::from_secs(1));
    let normal_late = enqueue(q, fetch(org(1), 3, IndexPriority::Normal)).await.job_id();
    let normal_early = enqueue(q, fetch(org(1), 4, IndexPriority::Normal).after(t0)).await.job_id();
    let delayed = enqueue(q, fetch(org(1), 5, IndexPriority::Highest).after(t0 + Duration::from_secs(60))).await.job_id();
    let server_job = enqueue(q, keyed(org(1))).await.job_id();

    let indexer = |max| LeaseRequest { lane: JobLane::Indexer, kinds: None, worker: "idx".into(), max };
    assert!(q.lease(&LeaseRequest { kinds: Some(vec![]), ..indexer(10) }).await.unwrap().is_empty(), "an empty kind list matches nothing");
    assert!(q.lease(&LeaseRequest { kinds: Some(vec![JobKind::IndexGc]), ..indexer(10) }).await.unwrap().is_empty());
    assert!(q.lease(&indexer(0)).await.unwrap().is_empty(), "max 0 leases nothing");

    let now = clock.now();
    let leased = q.lease(&indexer(3)).await.unwrap();
    let order: Vec<JobId> = leased.iter().map(|l| l.lease.job_id).collect();
    assert_eq!(order, vec![high, normal_early, normal_late], "priority DESC, run_after, id; delayed job not ready; max respected");
    let first = &leased[0];
    assert_eq!((first.kind, first.org_id, first.attempt), (JobKind::IndexFetch, Some(org(1)), 1));
    assert_eq!(first.max_attempts, JobKind::IndexFetch.retry_policy().max_attempts);
    assert_eq!(first.leased_until, now + JobKind::IndexFetch.retry_policy().lease);
    assert_eq!(first.enqueued_at, t0);
    assert_eq!(state_of(q, high).await, JobState::Running);

    assert_eq!(lease_one(q, &indexer(10)).await.lease.job_id, low);
    assert!(q.lease(&indexer(10)).await.unwrap().is_empty(), "the delayed job waits for run_after");
    clock.set(t0 + Duration::from_secs(60));
    assert_eq!(lease_one(q, &indexer(10)).await.lease.job_id, delayed);

    let kinds = LeaseRequest { kinds: Some(vec![JobKind::OpsRetention]), ..server(10) };
    assert_eq!(lease_one(q, &kinds).await.lease.job_id, server_job, "the server lane leases its own kinds only");
}

async fn lease_is_exclusive_across_workers(_: &FakeClock, q: &dyn JobQueue) {
    for n in 0..10 {
        enqueue(q, unkeyed(org(1), n)).await;
    }
    let requests: Vec<LeaseRequest> = (0..4).map(|w| LeaseRequest { worker: format!("worker-{w}"), ..server(10) }).collect();
    let rounds = join_all(requests.iter().map(|req| q.lease(req))).await;
    let leased: Vec<JobId> = rounds.into_iter().flat_map(|r| r.expect("lease")).map(|l| l.lease.job_id).collect();
    let distinct: BTreeSet<JobId> = leased.iter().copied().collect();
    assert_eq!(leased.len(), 10, "every job leased once: {leased:?}");
    assert_eq!(distinct.len(), 10, "no job leased twice: {leased:?}");
    assert!(q.lease(&server(10)).await.unwrap().is_empty());
}

async fn stale_lease_seq_is_lease_lost_everywhere(clock: &FakeClock, q: &dyn JobQueue) {
    let id = enqueue(q, keyed(org(1))).await.job_id();
    let stale = lease_one(q, &server(1)).await.lease;
    clock.advance(JobKind::OpsRetention.retry_policy().lease + Duration::from_secs(1));
    assert_eq!(q.reap_expired(10).await.unwrap(), 1);
    clock.advance(Duration::from_secs(3600));
    let fresh = lease_one(q, &server(1)).await.lease;
    assert_eq!(fresh.job_id, id);
    assert!(fresh.lease_seq > stale.lease_seq, "lease_seq grows per lease");

    let follow_up = unkeyed(org(1), 77).dedupe("follow-up");
    assert_eq!(q.heartbeat(&stale).await, Err(JobError::LeaseLost));
    assert_eq!(q.complete(&stale, std::slice::from_ref(&follow_up)).await, Err(JobError::LeaseLost));
    assert_eq!(q.fail(&stale, JobFailure::Permanent { error: "x".into() }).await, Err(JobError::LeaseLost));
    assert_eq!(q.defer(&stale, clock.now()).await, Err(JobError::LeaseLost));
    assert!(enqueue(q, follow_up).await.is_new(), "a rejected complete enqueues no follow-ups");
    assert_eq!(state_of(q, id).await, JobState::Running, "stale calls change nothing");

    q.complete(&fresh, &[]).await.unwrap();
    assert_eq!(q.complete(&fresh, &[]).await, Err(JobError::LeaseLost), "a finished job has no lease");
    assert_eq!(q.heartbeat(&fresh).await, Err(JobError::LeaseLost));
    let missing = JobLease { job_id: JobId::from_uuid(Uuid::from_u128(u128::MAX)), lease_seq: 1 };
    assert_eq!(q.heartbeat(&missing).await, Err(JobError::LeaseLost));
}

async fn heartbeat_extends_and_reports_cancel(clock: &FakeClock, q: &dyn JobQueue) {
    let id = enqueue(q, keyed(org(1))).await.job_id();
    let lease = lease_one(q, &server(1)).await.lease;
    let policy = JobKind::OpsRetention.retry_policy();
    clock.advance(Duration::from_secs(20));
    assert_eq!(q.heartbeat(&lease).await, Ok(Heartbeat::Extended { until: clock.now() + policy.lease }));

    clock.advance(policy.lease + Duration::from_secs(5));
    let late = q.heartbeat(&lease).await;
    assert_eq!(late, Ok(Heartbeat::Extended { until: clock.now() + policy.lease }), "a late heartbeat before the reaper still holds the job");
    assert_eq!(q.reap_expired(10).await.unwrap(), 0, "the extended lease is not expired");

    assert_eq!(q.cancel(id).await, Ok(CancelOutcome::Requested));
    assert_eq!(q.heartbeat(&lease).await, Ok(Heartbeat::CancelRequested { until: clock.now() + policy.lease }));
    assert_eq!(q.fail(&lease, JobFailure::Cancelled).await, Ok(FailOutcome::Cancelled));
    let record = q.get(id).await.unwrap();
    assert_eq!((record.state, record.finished_at), (JobState::Cancelled, Some(clock.now())));
}

async fn complete_enqueues_follow_ups_with_the_same_key(clock: &FakeClock, q: &dyn JobQueue) {
    let first = enqueue(q, keyed(org(1))).await.job_id();
    let lease = lease_one(q, &server(1)).await.lease;

    let invalid = NewJob { kind: JobKind::IndexGc, ..keyed(org(1)) };
    assert!(matches!(q.complete(&lease, &[invalid]).await, Err(JobError::Invalid(_))));
    assert_eq!(state_of(q, first).await, JobState::Running, "an invalid follow-up rolls the completion back");

    let next_run = clock.now() + Duration::from_secs(86_400);
    q.complete(&lease, &[keyed(org(1)).after(next_run), unkeyed(org(1), 5)]).await.unwrap();
    let record = q.get(first).await.unwrap();
    assert_eq!((record.state, record.finished_at), (JobState::Succeeded, Some(clock.now())));

    let successor = enqueue(q, keyed(org(1))).await;
    let Enqueued::Duplicate(successor_id) = successor else { panic!("the chain successor must be active: {successor:?}") };
    assert_ne!(successor_id, first);
    let successor = q.get(successor_id).await.unwrap();
    assert_eq!((successor.state, successor.run_after), (JobState::Queued, next_run));
    assert!(q.lease(&server(10)).await.unwrap().iter().all(|l| l.lease.job_id != successor_id), "the successor waits for run_after");
}

async fn retryable_failures_back_off_exactly_then_die(clock: &FakeClock, q: &dyn JobQueue) {
    let policy: RetryPolicy = JobKind::OpsRetention.retry_policy();
    let id = enqueue(q, keyed(org(1)).with_max_attempts(3)).await.job_id();

    let lease = lease_one(q, &server(1)).await.lease;
    let expected = clock.now() + retry_delay(&policy, id, 1);
    let outcome = q.fail(&lease, JobFailure::Retryable { error: "boom 1".into(), retry_at: None }).await;
    assert_eq!(outcome, Ok(FailOutcome::Retrying { at: expected }), "failure 1 uses the deterministic backoff");
    let record = q.get(id).await.unwrap();
    assert_eq!((record.state, record.run_after, record.attempts), (JobState::Queued, expected, 1));
    assert_eq!(record.last_error.as_deref(), Some("boom 1"));
    assert!(q.lease(&server(1)).await.unwrap().is_empty(), "not leasable before the backoff ends");

    clock.set(expected);
    let lease = lease_one(q, &server(1)).await;
    assert_eq!(lease.attempt, 2);
    let explicit = clock.now() + Duration::from_secs(7);
    let outcome = q.fail(&lease.lease, JobFailure::Retryable { error: "rate limited".into(), retry_at: Some(explicit) }).await;
    assert_eq!(outcome, Ok(FailOutcome::Retrying { at: explicit }), "retry_at overrides the backoff");

    clock.set(explicit);
    let lease = lease_one(q, &server(1)).await;
    assert_eq!(lease.attempt, 3);
    assert_eq!(q.fail(&lease.lease, JobFailure::Retryable { error: "boom 3".into(), retry_at: None }).await, Ok(FailOutcome::Dead));
    let record = q.get(id).await.unwrap();
    assert_eq!((record.state, record.attempts, record.finished_at), (JobState::Dead, 3, Some(clock.now())));
    assert_eq!(record.last_error.as_deref(), Some("boom 3"));
    assert!(enqueue(q, keyed(org(1))).await.is_new(), "a dead job leaves the active set");

    // Second failure of a fresh job: the schedule depends on (job id, attempt) only.
    let other = enqueue(q, unkeyed(org(2), 1)).await.job_id();
    let lease = lease_one(q, &LeaseRequest { kinds: Some(vec![JobKind::FixOpenPr]), ..server(1) }).await.lease;
    let outcome = q.fail(&lease, JobFailure::Retryable { error: "e".into(), retry_at: None }).await;
    assert_eq!(outcome, Ok(FailOutcome::Retrying { at: clock.now() + retry_delay(&JobKind::FixOpenPr.retry_policy(), other, 1) }));
}

async fn permanent_failure_dies_and_truncates_the_error(clock: &FakeClock, q: &dyn JobQueue) {
    let id = enqueue(q, keyed(org(1))).await.job_id();
    let lease = lease_one(q, &server(1)).await.lease;
    let huge = "x".repeat(LAST_ERROR_MAX_BYTES * 2);
    assert_eq!(q.fail(&lease, JobFailure::Permanent { error: huge }).await, Ok(FailOutcome::Dead));
    let record = q.get(id).await.unwrap();
    assert_eq!((record.state, record.attempts, record.finished_at), (JobState::Dead, 1, Some(clock.now())));
    assert_eq!(record.last_error.map(|e| e.len()), Some(LAST_ERROR_MAX_BYTES), "last_error is truncated to 4 KiB");
}

async fn defer_keeps_attempts_and_dies_past_max_defers(clock: &FakeClock, q: &dyn JobQueue) {
    let id = enqueue(q, keyed(org(1))).await.job_id();
    let lease = lease_one(q, &server(1)).await.lease;
    let until = clock.now() + Duration::from_secs(30);
    q.defer(&lease, until).await.unwrap();
    let record = q.get(id).await.unwrap();
    assert_eq!((record.state, record.attempts, record.defers, record.run_after), (JobState::Queued, 0, 1, until));
    assert_eq!(enqueue(q, keyed(org(1))).await, Enqueued::Duplicate(id), "the dedupe key stays with a deferred job");
    assert!(q.lease(&server(1)).await.unwrap().is_empty(), "deferred until `until`");
    clock.set(until);
    let again = lease_one(q, &server(1)).await;
    assert_eq!(again.attempt, 1, "defer gives the attempt back");
    assert!(again.lease.lease_seq > lease.lease_seq);
    assert_eq!(q.defer(&lease, until).await, Err(JobError::LeaseLost), "the returned lease stays stale");

    let max_defers = JobKind::OpsRetention.retry_policy().max_defers;
    let mut current = again.lease;
    for n in 2..max_defers {
        q.defer(&current, clock.now()).await.unwrap();
        current = lease_one(q, &server(1)).await.lease;
        assert_eq!(q.get(id).await.unwrap().defers, n, "defers count once per call");
    }
    q.defer(&current, clock.now()).await.unwrap();
    let record = q.get(id).await.unwrap();
    assert_eq!((record.state, record.defers), (JobState::Queued, max_defers), "max_defers defers are allowed");
    let last = lease_one(q, &server(1)).await.lease;
    q.defer(&last, clock.now()).await.unwrap();
    let record = q.get(id).await.unwrap();
    assert_eq!(record.state, JobState::Dead, "one defer past max_defers kills the job");
    assert_eq!(record.last_error.as_deref(), Some(DEFER_LIMIT_ERROR));
    assert_eq!(record.finished_at, Some(clock.now()));
}

async fn reaper_requeues_kills_and_cancels_expired_leases(clock: &FakeClock, q: &dyn JobQueue) {
    let policy = JobKind::FixOpenPr.retry_policy();
    let id = enqueue(q, unkeyed(org(1), 1).with_max_attempts(2)).await.job_id();
    lease_one(q, &server(1)).await;
    assert_eq!(q.reap_expired(10).await.unwrap(), 0, "an unexpired lease is not reaped");

    clock.advance(policy.lease + Duration::from_secs(1));
    assert_eq!(q.reap_expired(10).await.unwrap(), 1);
    let record = q.get(id).await.unwrap();
    assert_eq!(record.state, JobState::Queued, "attempts left → requeued");
    assert_eq!(record.run_after, clock.now() + reap_delay(&policy, 1), "unjittered backoff");
    assert_eq!(record.last_error.as_deref(), Some(LEASE_EXPIRED_ERROR));
    assert_eq!(record.attempts, 1, "the expired lease consumed its attempt");

    clock.set(record.run_after);
    assert_eq!(lease_one(q, &server(1)).await.attempt, 2);
    clock.advance(policy.lease + Duration::from_secs(1));
    assert_eq!(q.reap_expired(10).await.unwrap(), 1);
    let record = q.get(id).await.unwrap();
    assert_eq!((record.state, record.finished_at), (JobState::Dead, Some(clock.now())), "attempts exhausted → dead");

    let cancelled = enqueue(q, unkeyed(org(1), 2)).await.job_id();
    lease_one(q, &server(1)).await;
    assert_eq!(q.cancel(cancelled).await, Ok(CancelOutcome::Requested));
    clock.advance(policy.lease + Duration::from_secs(1));
    assert_eq!(q.reap_expired(10).await.unwrap(), 1);
    assert_eq!(state_of(q, cancelled).await, JobState::Cancelled, "cancel requested → cancelled");

    for n in 10..13 {
        enqueue(q, unkeyed(org(1), n)).await;
    }
    assert_eq!(q.lease(&server(10)).await.unwrap().len(), 3);
    clock.advance(policy.lease + Duration::from_secs(1));
    assert_eq!(q.reap_expired(2).await.unwrap(), 2, "limit respected");
    assert_eq!(q.reap_expired(2).await.unwrap(), 1);
    assert_eq!(q.reap_expired(2).await.unwrap(), 0);
}

async fn cancel_in_each_state(clock: &FakeClock, q: &dyn JobQueue) {
    let queued = enqueue(q, keyed(org(1))).await.job_id();
    assert_eq!(q.cancel(queued).await, Ok(CancelOutcome::Cancelled));
    let record = q.get(queued).await.unwrap();
    assert_eq!((record.state, record.finished_at), (JobState::Cancelled, Some(clock.now())));
    assert_eq!(q.cancel(queued).await, Ok(CancelOutcome::AlreadyFinished));
    assert!(enqueue(q, keyed(org(1))).await.is_new(), "a cancelled job frees its key");
    assert_eq!(q.cancel(JobId::from_uuid(Uuid::from_u128(u128::MAX))).await, Err(JobError::NotFound));

    let running = lease_one(q, &server(1)).await;
    assert_eq!(q.cancel(running.lease.job_id).await, Ok(CancelOutcome::Requested));
    assert_eq!(state_of(q, running.lease.job_id).await, JobState::Running, "a running job only gets the request");
    q.complete(&running.lease, &[]).await.unwrap();
    assert_eq!(q.cancel(running.lease.job_id).await, Ok(CancelOutcome::AlreadyFinished));

    let a_queued = enqueue(q, unkeyed(org(1), 1)).await.job_id();
    let a_running_id = enqueue(q, unkeyed(org(1), 2)).await.job_id();
    let b_queued = enqueue(q, unkeyed(org(2), 3)).await.job_id();
    let a_done = enqueue(q, unkeyed(org(1), 4)).await.job_id();
    assert_eq!(q.cancel(a_done).await, Ok(CancelOutcome::Cancelled));
    let leased = q.lease(&server(10)).await.unwrap();
    let a_running = leased.iter().find(|l| l.lease.job_id == a_running_id).expect("org 1 job leased").lease;
    for other in leased.iter().filter(|l| l.lease.job_id != a_running_id) {
        q.defer(&other.lease, clock.now()).await.unwrap();
    }
    assert_eq!(q.cancel_org(org(1)).await, Ok(2), "the queued and the running job of org 1");
    assert_eq!(state_of(q, a_queued).await, JobState::Cancelled);
    assert_eq!(q.heartbeat(&a_running).await.map(|h| matches!(h, Heartbeat::CancelRequested { .. })), Ok(true));
    assert_eq!(state_of(q, b_queued).await, JobState::Queued, "other orgs untouched");
}

async fn dead_letter_list_retry_discard(clock: &FakeClock, q: &dyn JobQueue) {
    let mut dead = Vec::new();
    for n in 0..3 {
        let id = enqueue(q, unkeyed(org(1), n)).await.job_id();
        let lease = lease_one(q, &server(1)).await.lease;
        q.fail(&lease, JobFailure::Permanent { error: format!("dead {n}") }).await.unwrap();
        dead.push(id);
        clock.advance(Duration::from_secs(1));
    }
    let keyed_id = enqueue(q, keyed(org(1))).await.job_id();
    let lease = lease_one(q, &server(1)).await.lease;
    q.fail(&lease, JobFailure::Permanent { error: "keyed".into() }).await.unwrap();
    clock.advance(Duration::from_secs(1));

    let first = q.list_dead(Some(JobKind::FixOpenPr), PageRequest::first(2)).await.unwrap();
    assert_eq!(first.items.iter().map(|r| r.id).collect::<Vec<_>>(), vec![dead[2], dead[1]], "newest finished first");
    let cursor = first.next.clone().expect("a second page");
    let second = q.list_dead(Some(JobKind::FixOpenPr), PageRequest::first(2).after(cursor)).await.unwrap();
    assert_eq!((second.items.iter().map(|r| r.id).collect::<Vec<_>>(), second.next), (vec![dead[0]], None));
    assert_eq!(q.list_dead(None, PageRequest::first(10)).await.unwrap().items.len(), 4);

    q.retry_dead(dead[0]).await.unwrap();
    let record = q.get(dead[0]).await.unwrap();
    assert_eq!((record.state, record.attempts, record.defers, record.finished_at), (JobState::Queued, 0, 0, None));
    assert_eq!(record.run_after, clock.now());
    assert_eq!(q.retry_dead(dead[0]).await, Err(JobError::Conflict), "only dead jobs can be retried");

    let blocker = enqueue(q, keyed(org(1))).await;
    assert!(blocker.is_new());
    assert_eq!(q.retry_dead(keyed_id).await, Err(JobError::Conflict), "an active job holds the dedupe key");

    q.discard_dead(dead[1]).await.unwrap();
    assert_eq!(state_of(q, dead[1]).await, JobState::Cancelled);
    assert_eq!(q.discard_dead(dead[1]).await, Err(JobError::Conflict));
    assert_eq!(q.retry_dead(JobId::from_uuid(Uuid::from_u128(u128::MAX))).await, Err(JobError::NotFound));
    assert_eq!(q.discard_dead(JobId::from_uuid(Uuid::from_u128(u128::MAX))).await, Err(JobError::NotFound));
    let remaining: Vec<JobId> = q.list_dead(None, PageRequest::first(10)).await.unwrap().items.iter().map(|r| r.id).collect();
    assert_eq!(remaining, vec![keyed_id, dead[2]]);
}

async fn purge_deletes_old_finished_jobs(clock: &FakeClock, q: &dyn JobQueue) {
    let t0 = clock.now();
    let succeeded_old = enqueue(q, unkeyed(org(1), 1)).await.job_id();
    let lease = lease_one(q, &server(1)).await.lease;
    q.complete(&lease, &[]).await.unwrap();
    let dead_old = enqueue(q, unkeyed(org(1), 2)).await.job_id();
    let lease = lease_one(q, &server(1)).await.lease;
    q.fail(&lease, JobFailure::Permanent { error: "x".into() }).await.unwrap();

    clock.advance(Duration::from_secs(86_400));
    let cancelled_new = enqueue(q, unkeyed(org(1), 3)).await.job_id();
    q.cancel(cancelled_new).await.unwrap();
    let queued = enqueue(q, unkeyed(org(1), 4)).await.job_id();

    let cutoff = t0 + Duration::from_secs(1);
    assert_eq!(q.purge_finished(cutoff, t0, 10).await, Ok(1), "only the old succeeded job (dead cutoff is earlier)");
    assert_eq!(q.get(succeeded_old).await, Err(JobError::NotFound));
    assert_eq!(state_of(q, dead_old).await, JobState::Dead);
    assert_eq!(q.purge_finished(clock.now() + Duration::from_secs(1), cutoff, 1).await, Ok(1), "limit respected");
    assert_eq!(q.purge_finished(clock.now() + Duration::from_secs(1), cutoff, 10).await, Ok(1));
    assert_eq!(q.get(dead_old).await, Err(JobError::NotFound));
    assert_eq!(q.get(cancelled_new).await, Err(JobError::NotFound));
    assert_eq!(state_of(q, queued).await, JobState::Queued, "active jobs are never purged");
}
