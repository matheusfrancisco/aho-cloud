//! `MemJobQueue`: in-memory `JobQueue` with the dedupe, lease fencing, backoff, defer, reaper and
//! dead-letter semantics of storage.md "Job queue semantics". Every call holds one lock, so each
//! is atomic like the Postgres transaction it mirrors.

use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::domain::ids::{JobId, OrgId};
use crate::domain::job::{JobKind, JobLane, JobPayload};
use crate::ports::clock::Clock;
use crate::ports::common::{Cursor, Page, PageRequest};
use crate::ports::ids::IdGen;
use crate::ports::jobs::{
    CancelOutcome, DEFER_LIMIT_ERROR, Enqueued, FailOutcome, Heartbeat, JobError, JobFailure, JobLease, JobQueue, JobRecord, JobState,
    LEASE_EXPIRED_ERROR, LeaseRequest, LeasedJob, NewJob, reap_delay, retry_delay, truncate_error,
};
use crate::testing::page::keyset_page;

pub struct MemJobQueue {
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGen>,
    rows: Mutex<BTreeMap<JobId, Row>>,
}

/// One `jobs` row (storage.md "Ops").
#[derive(Debug, Clone)]
struct Row {
    id: JobId,
    kind: JobKind,
    lane: JobLane,
    org_id: Option<OrgId>,
    payload: JobPayload,
    dedupe_key: Option<String>,
    priority: i16,
    state: JobState,
    run_after: OffsetDateTime,
    attempts: u16,
    max_attempts: u16,
    defers: u16,
    lease_seq: u32,
    leased_by: Option<String>,
    leased_until: Option<OffsetDateTime>,
    cancel_requested: bool,
    last_error: Option<String>,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
    finished_at: Option<OffsetDateTime>,
}

impl Row {
    fn record(&self) -> JobRecord {
        JobRecord {
            id: self.id,
            kind: self.kind,
            org_id: self.org_id,
            payload: self.payload.clone(),
            dedupe_key: self.dedupe_key.clone(),
            state: self.state,
            attempts: self.attempts,
            max_attempts: self.max_attempts,
            defers: self.defers,
            run_after: self.run_after,
            last_error: self.last_error.clone(),
            created_at: self.created_at,
            finished_at: self.finished_at,
        }
    }

    /// Leave `Running`: clears the lease columns.
    fn release(&mut self, now: OffsetDateTime) {
        self.leased_by = None;
        self.leased_until = None;
        self.updated_at = now;
    }

    fn finish(&mut self, state: JobState, now: OffsetDateTime) {
        self.state = state;
        self.finished_at = Some(now);
        self.release(now);
    }
}

impl MemJobQueue {
    pub fn new(clock: Arc<dyn Clock>, ids: Arc<dyn IdGen>) -> Self {
        Self { clock, ids, rows: Mutex::new(BTreeMap::new()) }
    }

    /// Every job, oldest first (test inspection).
    pub fn jobs(&self) -> Vec<JobRecord> {
        let rows = self.rows.lock();
        let mut out: Vec<&Row> = rows.values().collect();
        out.sort_by_key(|row| (row.created_at, row.id));
        out.into_iter().map(Row::record).collect()
    }

    /// The checks an outbox runs before its store writes anything (`NewJob::validate` on each).
    pub fn validate_all(jobs: &[NewJob]) -> Result<(), JobError> {
        jobs.iter().try_for_each(NewJob::validate)
    }

    /// Outbox insert for in-memory stores, after [`Self::validate_all`] passed: `enqueue` for each
    /// job, in order. Lets a fake store commit its write and its jobs together.
    pub fn insert_validated(&self, jobs: &[NewJob]) -> Vec<Enqueued> {
        let now = self.clock.now();
        let mut rows = self.rows.lock();
        jobs.iter().map(|job| self.insert(&mut rows, job, now)).collect()
    }

    fn insert(&self, rows: &mut BTreeMap<JobId, Row>, job: &NewJob, now: OffsetDateTime) -> Enqueued {
        let org_id = job.payload.org_id();
        if let Some(key) = &job.dedupe_key {
            let active = rows
                .values()
                .find(|row| row.state.is_active() && row.org_id == org_id && row.kind == job.kind && row.dedupe_key.as_ref() == Some(key));
            if let Some(row) = active {
                return Enqueued::Duplicate(row.id);
            }
        }
        let id: JobId = JobId::from_uuid(self.ids.uuid_v7());
        rows.insert(id, Row {
            id,
            kind: job.kind,
            lane: job.kind.lane(),
            org_id,
            payload: job.payload.clone(),
            dedupe_key: job.dedupe_key.clone(),
            priority: job.payload.priority(),
            state: JobState::Queued,
            run_after: job.run_after.unwrap_or(now),
            attempts: 0,
            max_attempts: job.effective_max_attempts(),
            defers: 0,
            lease_seq: 0,
            leased_by: None,
            leased_until: None,
            cancel_requested: false,
            last_error: None,
            created_at: now,
            updated_at: now,
            finished_at: None,
        });
        Enqueued::New(id)
    }
}

/// The row a lease-scoped call may act on: running under exactly this lease.
fn leased<'a>(rows: &'a mut BTreeMap<JobId, Row>, lease: &JobLease) -> Result<&'a mut Row, JobError> {
    match rows.get_mut(&lease.job_id) {
        Some(row) if row.state == JobState::Running && row.lease_seq == lease.lease_seq => Ok(row),
        _ => Err(JobError::LeaseLost),
    }
}

/// Dead-letter cursor: `{finished_at unix nanos}/{job uuid}`.
type DeadKey = (Reverse<i128>, Reverse<JobId>);

fn encode_dead(key: &DeadKey) -> String {
    format!("{}/{}", (key.0).0, (key.1).0.as_uuid())
}

fn decode_dead(cursor: &Cursor) -> Result<DeadKey, JobError> {
    let invalid = || JobError::Invalid(format!("bad cursor {:?}", cursor.0));
    let (nanos, id) = cursor.0.split_once('/').ok_or_else(invalid)?;
    let nanos: i128 = nanos.parse().map_err(|_| invalid())?;
    let id = Uuid::try_parse(id).map_err(|_| invalid())?;
    Ok((Reverse(nanos), Reverse(JobId::from_uuid(id))))
}

#[async_trait]
impl JobQueue for MemJobQueue {
    async fn enqueue(&self, job: NewJob) -> Result<Enqueued, JobError> {
        job.validate()?;
        let now = self.clock.now();
        let mut rows = self.rows.lock();
        Ok(self.insert(&mut rows, &job, now))
    }

    async fn enqueue_all(&self, jobs: &[NewJob]) -> Result<Vec<Enqueued>, JobError> {
        Self::validate_all(jobs)?;
        Ok(self.insert_validated(jobs))
    }

    async fn lease(&self, req: &LeaseRequest) -> Result<Vec<LeasedJob>, JobError> {
        let now = self.clock.now();
        let mut rows = self.rows.lock();
        let mut ready: Vec<(Reverse<i16>, OffsetDateTime, JobId)> = rows
            .values()
            .filter(|row| row.state == JobState::Queued && row.lane == req.lane && row.run_after <= now)
            .filter(|row| req.kinds.as_ref().is_none_or(|kinds| kinds.contains(&row.kind)))
            .map(|row| (Reverse(row.priority), row.run_after, row.id))
            .collect();
        ready.sort();
        ready.truncate(usize::from(req.max));
        let mut leased = Vec::with_capacity(ready.len());
        for (_, _, id) in ready {
            let Some(row) = rows.get_mut(&id) else { continue };
            let until = now + row.kind.retry_policy().lease;
            row.state = JobState::Running;
            row.attempts += 1;
            row.lease_seq += 1;
            row.leased_by = Some(req.worker.clone());
            row.leased_until = Some(until);
            row.updated_at = now;
            leased.push(LeasedJob {
                lease: JobLease { job_id: row.id, lease_seq: row.lease_seq },
                kind: row.kind,
                org_id: row.org_id,
                payload: row.payload.clone(),
                attempt: row.attempts,
                max_attempts: row.max_attempts,
                leased_until: until,
                enqueued_at: row.created_at,
            });
        }
        Ok(leased)
    }

    async fn heartbeat(&self, lease: &JobLease) -> Result<Heartbeat, JobError> {
        let now = self.clock.now();
        let mut rows = self.rows.lock();
        let row = leased(&mut rows, lease)?;
        let until = now + row.kind.retry_policy().lease;
        row.leased_until = Some(until);
        row.updated_at = now;
        Ok(if row.cancel_requested { Heartbeat::CancelRequested { until } } else { Heartbeat::Extended { until } })
    }

    async fn complete(&self, lease: &JobLease, follow_ups: &[NewJob]) -> Result<(), JobError> {
        let now = self.clock.now();
        let mut rows = self.rows.lock();
        leased(&mut rows, lease)?;
        Self::validate_all(follow_ups)?;
        if let Some(row) = rows.get_mut(&lease.job_id) {
            row.finish(JobState::Succeeded, now);
        }
        for job in follow_ups {
            self.insert(&mut rows, job, now);
        }
        Ok(())
    }

    async fn fail(&self, lease: &JobLease, failure: JobFailure) -> Result<FailOutcome, JobError> {
        let now = self.clock.now();
        let mut rows = self.rows.lock();
        let row = leased(&mut rows, lease)?;
        let outcome = match failure {
            JobFailure::Retryable { error, retry_at } if row.attempts < row.max_attempts => {
                let at = retry_at.unwrap_or_else(|| now + retry_delay(&row.kind.retry_policy(), row.id, row.attempts));
                row.state = JobState::Queued;
                row.run_after = at;
                row.last_error = Some(truncate_error(&error));
                row.release(now);
                FailOutcome::Retrying { at }
            }
            JobFailure::Retryable { error, .. } | JobFailure::Permanent { error } => {
                row.last_error = Some(truncate_error(&error));
                row.finish(JobState::Dead, now);
                FailOutcome::Dead
            }
            JobFailure::Cancelled => {
                row.finish(JobState::Cancelled, now);
                FailOutcome::Cancelled
            }
        };
        Ok(outcome)
    }

    async fn defer(&self, lease: &JobLease, until: OffsetDateTime) -> Result<(), JobError> {
        let now = self.clock.now();
        let mut rows = self.rows.lock();
        let row = leased(&mut rows, lease)?;
        row.run_after = until;
        row.attempts -= 1;
        row.defers += 1;
        if row.defers > row.kind.retry_policy().max_defers {
            row.last_error = Some(DEFER_LIMIT_ERROR.to_owned());
            row.finish(JobState::Dead, now);
        } else {
            row.state = JobState::Queued;
            row.release(now);
        }
        Ok(())
    }

    async fn cancel(&self, job: JobId) -> Result<CancelOutcome, JobError> {
        let now = self.clock.now();
        let mut rows = self.rows.lock();
        let row = rows.get_mut(&job).ok_or(JobError::NotFound)?;
        Ok(match row.state {
            JobState::Queued => {
                row.finish(JobState::Cancelled, now);
                CancelOutcome::Cancelled
            }
            JobState::Running => {
                row.cancel_requested = true;
                row.updated_at = now;
                CancelOutcome::Requested
            }
            JobState::Succeeded | JobState::Dead | JobState::Cancelled => CancelOutcome::AlreadyFinished,
        })
    }

    async fn cancel_org(&self, org: OrgId) -> Result<u64, JobError> {
        let now = self.clock.now();
        let mut rows = self.rows.lock();
        let mut count = 0;
        for row in rows.values_mut().filter(|row| row.org_id == Some(org) && row.state.is_active()) {
            if row.state == JobState::Queued {
                row.finish(JobState::Cancelled, now);
            } else {
                row.cancel_requested = true;
                row.updated_at = now;
            }
            count += 1;
        }
        Ok(count)
    }

    async fn reap_expired(&self, limit: u32) -> Result<u32, JobError> {
        let now = self.clock.now();
        let mut rows = self.rows.lock();
        let mut expired: Vec<(OffsetDateTime, JobId)> = rows
            .values()
            .filter(|row| row.state == JobState::Running)
            .filter_map(|row| row.leased_until.filter(|until| *until < now).map(|until| (until, row.id)))
            .collect();
        expired.sort();
        expired.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        let reaped = u32::try_from(expired.len()).unwrap_or(u32::MAX);
        for (_, id) in expired {
            let Some(row) = rows.get_mut(&id) else { continue };
            row.last_error = Some(LEASE_EXPIRED_ERROR.to_owned());
            if row.cancel_requested {
                row.finish(JobState::Cancelled, now);
            } else if row.attempts < row.max_attempts {
                row.state = JobState::Queued;
                row.run_after = now + reap_delay(&row.kind.retry_policy(), row.attempts);
                row.release(now);
            } else {
                row.finish(JobState::Dead, now);
            }
        }
        Ok(reaped)
    }

    async fn get(&self, job: JobId) -> Result<JobRecord, JobError> {
        self.rows.lock().get(&job).map(Row::record).ok_or(JobError::NotFound)
    }

    async fn list_dead(&self, kind: Option<JobKind>, page: PageRequest) -> Result<Page<JobRecord>, JobError> {
        let after = page.after.as_ref().map(decode_dead).transpose()?;
        let rows = self.rows.lock();
        let items: Vec<(DeadKey, JobRecord)> = rows
            .values()
            .filter(|row| row.state == JobState::Dead && kind.is_none_or(|kind| row.kind == kind))
            .map(|row| {
                let finished = row.finished_at.unwrap_or(row.updated_at).unix_timestamp_nanos();
                ((Reverse(finished), Reverse(row.id)), row.record())
            })
            .collect();
        let (items, next) = keyset_page(items, after.as_ref(), page.effective_limit(), encode_dead);
        Ok(Page { items, next })
    }

    async fn retry_dead(&self, job: JobId) -> Result<(), JobError> {
        let now = self.clock.now();
        let mut rows = self.rows.lock();
        let row = rows.get(&job).ok_or(JobError::NotFound)?;
        if row.state != JobState::Dead {
            return Err(JobError::Conflict);
        }
        if let Some(key) = &row.dedupe_key {
            let (org_id, kind) = (row.org_id, row.kind);
            let taken = rows
                .values()
                .any(|other| other.state.is_active() && other.org_id == org_id && other.kind == kind && other.dedupe_key.as_ref() == Some(key));
            if taken {
                return Err(JobError::Conflict);
            }
        }
        let row = rows.get_mut(&job).ok_or(JobError::NotFound)?;
        row.state = JobState::Queued;
        row.attempts = 0;
        row.defers = 0;
        row.run_after = now;
        row.finished_at = None;
        row.cancel_requested = false;
        row.updated_at = now;
        Ok(())
    }

    async fn discard_dead(&self, job: JobId) -> Result<(), JobError> {
        let now = self.clock.now();
        let mut rows = self.rows.lock();
        let row = rows.get_mut(&job).ok_or(JobError::NotFound)?;
        if row.state != JobState::Dead {
            return Err(JobError::Conflict);
        }
        row.finish(JobState::Cancelled, now);
        Ok(())
    }

    async fn purge_finished(&self, succeeded_before: OffsetDateTime, dead_before: OffsetDateTime, limit: u32) -> Result<u64, JobError> {
        let mut rows = self.rows.lock();
        let mut doomed: Vec<(OffsetDateTime, JobId)> = rows
            .values()
            .filter_map(|row| {
                let finished = row.finished_at?;
                let expired = match row.state {
                    JobState::Succeeded | JobState::Cancelled => finished < succeeded_before,
                    JobState::Dead => finished < dead_before,
                    JobState::Queued | JobState::Running => false,
                };
                expired.then_some((finished, row.id))
            })
            .collect();
        doomed.sort();
        doomed.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        for (_, id) in &doomed {
            rows.remove(id);
        }
        Ok(doomed.len() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{FakeClock, SeqIdGen, contract_job_queue};

    #[tokio::test]
    async fn mem_job_queue_passes_the_contract() {
        contract_job_queue(|clock| async move { Arc::new(MemJobQueue::new(clock, Arc::new(SeqIdGen::new()))) as Arc<dyn JobQueue> }).await;
    }

    #[tokio::test]
    async fn outbox_insert_dedupes_like_enqueue_and_jobs_lists_in_creation_order() {
        let clock = Arc::new(FakeClock::new());
        let queue = MemJobQueue::new(clock, Arc::new(SeqIdGen::new()));
        let job = NewJob::new(JobPayload::IndexGc);
        assert_eq!(MemJobQueue::validate_all(std::slice::from_ref(&job)), Ok(()));
        let first = queue.insert_validated(&[job.clone(), job.clone()]);
        assert!(first[0].is_new());
        assert_eq!(first[1], Enqueued::Duplicate(first[0].job_id()));
        let second = queue.enqueue(NewJob::new(JobPayload::OpsPurge)).await.unwrap();
        let ids: Vec<JobId> = queue.jobs().iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![first[0].job_id(), second.job_id()]);
    }
}
