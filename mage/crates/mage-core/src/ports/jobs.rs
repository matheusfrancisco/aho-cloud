//! `JobQueue` port (storage.md "`JobQueue`" and "Job queue semantics"). Payloads, kinds, lanes
//! and retry policies are domain types (`mage_core::domain::job`); this module holds the queue's
//! request/response types and the deterministic backoff shared by every implementation.
//!
//! Semantics every implementation follows (the contract suite in `mage_core::testing` checks
//! them):
//!
//! - At most one **active** (`Queued` or `Running`) job per `(org_id, kind, dedupe_key)`;
//!   `enqueue` of an active key returns `Duplicate(existing)` and leaves the existing job
//!   untouched. Jobs without a dedupe key are never deduplicated.
//! - Every lease-scoped call carries a [`JobLease`] and only acts while the job is `Running` with
//!   that `lease_seq`; otherwise it fails with [`JobError::LeaseLost`] and changes nothing
//!   (follow-ups included). `lease_seq` grows by one per lease and never goes down.
//! - `lease` counts an attempt; `defer` gives it back. A job dies when a retryable failure or an
//!   expired lease finds `attempts >= max_attempts`, on a permanent failure, or when a `defer`
//!   would exceed the kind's `max_defers`.
//! - Timestamps come from the implementation's `Clock`.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use async_trait::async_trait;
use time::OffsetDateTime;

use crate::domain::ids::{JobId, OrgId};
use crate::domain::job::{JobKind, JobLane, JobPayload, RetryPolicy};
use crate::ports::common::{Page, PageRequest};

/// `jobs.last_error` is truncated to this many bytes.
pub const LAST_ERROR_MAX_BYTES: usize = 4096;

/// `last_error` written by `defer` when the defer limit is exceeded.
pub const DEFER_LIMIT_ERROR: &str = "defer limit";

/// `last_error` written by `reap_expired`.
pub const LEASE_EXPIRED_ERROR: &str = "lease expired";

/// A job to insert. `kind` must equal `payload.kind()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewJob {
    pub kind: JobKind,
    pub payload: JobPayload,
    pub dedupe_key: Option<String>,
    /// `None` = now (the queue's `Clock`).
    pub run_after: Option<OffsetDateTime>,
    /// `None` = `kind.retry_policy().max_attempts`.
    pub max_attempts: Option<u16>,
}

impl NewJob {
    /// Fills `kind` and the documented dedupe key (`JobPayload::dedupe_key`); kinds whose key needs
    /// data the payload lacks get `None` and the enqueuer sets it with [`Self::dedupe`].
    pub fn new(payload: JobPayload) -> Self {
        Self { kind: payload.kind(), dedupe_key: payload.dedupe_key(), payload, run_after: None, max_attempts: None }
    }

    pub fn dedupe(mut self, key: impl Into<String>) -> Self {
        self.dedupe_key = Some(key.into());
        self
    }

    /// Drop the dedupe key: the job is never deduplicated.
    pub fn without_dedupe(mut self) -> Self {
        self.dedupe_key = None;
        self
    }

    pub fn after(mut self, at: OffsetDateTime) -> Self {
        self.run_after = Some(at);
        self
    }

    pub fn with_max_attempts(mut self, max_attempts: u16) -> Self {
        self.max_attempts = Some(max_attempts);
        self
    }

    /// The checks every `enqueue` path (including outbox inserts) runs before writing.
    pub fn validate(&self) -> Result<(), JobError> {
        let payload_kind = self.payload.kind();
        if self.kind != payload_kind {
            return Err(JobError::Invalid(format!("job kind {} does not match payload kind {payload_kind}", self.kind)));
        }
        if self.max_attempts == Some(0) {
            return Err(JobError::Invalid("max_attempts must be at least 1".into()));
        }
        if self.dedupe_key.as_deref() == Some("") {
            return Err(JobError::Invalid("dedupe key is empty".into()));
        }
        Ok(())
    }

    pub fn effective_max_attempts(&self) -> u16 {
        self.max_attempts.unwrap_or(self.kind.retry_policy().max_attempts)
    }
}

/// Result of one enqueue. A duplicate is not an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Enqueued {
    New(JobId),
    /// An active job with the same `(org_id, kind, dedupe_key)` exists; it is returned unchanged.
    Duplicate(JobId),
}

impl Enqueued {
    pub fn job_id(&self) -> JobId {
        match self {
            Self::New(id) | Self::Duplicate(id) => *id,
        }
    }

    pub fn is_new(&self) -> bool {
        matches!(self, Self::New(_))
    }
}

/// Fencing token of one lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JobLease {
    pub job_id: JobId,
    pub lease_seq: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseRequest {
    pub lane: JobLane,
    /// `None` = every kind of the lane; `Some(vec![])` matches nothing.
    pub kinds: Option<Vec<JobKind>>,
    /// Written to `jobs.leased_by`.
    pub worker: String,
    /// At most this many jobs (free worker slots).
    pub max: u16,
}

/// A job handed to a worker. Ordering among ready jobs is `priority DESC, run_after, id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeasedJob {
    pub lease: JobLease,
    pub kind: JobKind,
    pub org_id: Option<OrgId>,
    pub payload: JobPayload,
    /// 1-based number of this attempt (`jobs.attempts` after the lease).
    pub attempt: u16,
    pub max_attempts: u16,
    /// `now + kind.retry_policy().lease`; heartbeat every `lease / 3`.
    pub leased_until: OffsetDateTime,
    pub enqueued_at: OffsetDateTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Heartbeat {
    Extended { until: OffsetDateTime },
    /// The lease was extended, and `cancel` asked the handler to stop and call `fail(Cancelled)`.
    CancelRequested { until: OffsetDateTime },
}

impl Heartbeat {
    pub fn until(&self) -> OffsetDateTime {
        match self {
            Self::Extended { until } | Self::CancelRequested { until } => *until,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobFailure {
    /// Retry at `retry_at`, or after the backoff ([`retry_delay`]) when `None`, while attempts
    /// remain; otherwise the job dies.
    Retryable { error: String, retry_at: Option<OffsetDateTime> },
    Permanent { error: String },
    /// The handler stopped after `Heartbeat::CancelRequested`.
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailOutcome {
    Retrying { at: OffsetDateTime },
    Dead,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelOutcome {
    /// Was queued; now cancelled.
    Cancelled,
    /// Is running; the holder sees `CancelRequested` on its next heartbeat.
    Requested,
    /// Already succeeded, dead or cancelled.
    AlreadyFinished,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JobState {
    Queued,
    Running,
    Succeeded,
    Dead,
    Cancelled,
}

impl JobState {
    /// `jobs.state` value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Dead => "dead",
            Self::Cancelled => "cancelled",
        }
    }

    /// `Queued` or `Running`: the states the dedupe key is unique in.
    pub const fn is_active(self) -> bool {
        matches!(self, Self::Queued | Self::Running)
    }
}

impl fmt::Display for JobState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A string that is not a `JobState::as_str()` value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown job state `{0}`")]
pub struct UnknownJobState(pub String);

impl FromStr for JobState {
    type Err = UnknownJobState;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        [Self::Queued, Self::Running, Self::Succeeded, Self::Dead, Self::Cancelled]
            .into_iter()
            .find(|state| state.as_str() == s)
            .ok_or_else(|| UnknownJobState(s.to_owned()))
    }
}

/// One `jobs` row as seen by operators (`get`, dead-letter listing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRecord {
    pub id: JobId,
    pub kind: JobKind,
    pub org_id: Option<OrgId>,
    pub payload: JobPayload,
    pub dedupe_key: Option<String>,
    pub state: JobState,
    pub attempts: u16,
    pub max_attempts: u16,
    pub defers: u16,
    pub run_after: OffsetDateTime,
    pub last_error: Option<String>,
    pub created_at: OffsetDateTime,
    pub finished_at: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JobError {
    #[error("job not found")]
    NotFound,
    #[error("job conflict")]
    Conflict,
    #[error("invalid job request: {0}")]
    Invalid(String),
    #[error("job queue unavailable: {0}")]
    Unavailable(String),
    /// The lease is stale (expired and reaped, re-leased, or finished): stop and drop the result.
    #[error("job lease lost")]
    LeaseLost,
}

#[async_trait]
pub trait JobQueue: Send + Sync {
    /// Insert `job` as `Queued` (`run_after` defaults to now), or return the active duplicate.
    async fn enqueue(&self, job: NewJob) -> Result<Enqueued, JobError>;
    /// Like `enqueue` for each job, in one transaction (all or none); results in input order.
    async fn enqueue_all(&self, jobs: &[NewJob]) -> Result<Vec<Enqueued>, JobError>;
    /// Lease up to `req.max` ready jobs (`Queued`, `run_after <= now`) of the lane.
    async fn lease(&self, req: &LeaseRequest) -> Result<Vec<LeasedJob>, JobError>;
    /// Extend the lease by the kind's lease length.
    async fn heartbeat(&self, lease: &JobLease) -> Result<Heartbeat, JobError>;
    /// Mark succeeded, then enqueue `follow_ups` in the same transaction (the finished job has
    /// left the active set, so a follow-up may reuse its dedupe key).
    async fn complete(&self, lease: &JobLease, follow_ups: &[NewJob]) -> Result<(), JobError>;
    async fn fail(&self, lease: &JobLease, failure: JobFailure) -> Result<FailOutcome, JobError>;
    /// Requeue at `until` without consuming the attempt; dies with "defer limit" past
    /// `max_defers`.
    async fn defer(&self, lease: &JobLease, until: OffsetDateTime) -> Result<(), JobError>;
    async fn cancel(&self, job: JobId) -> Result<CancelOutcome, JobError>;
    /// `cancel` for every active job of `org`; returns how many were cancelled or requested.
    async fn cancel_org(&self, org: OrgId) -> Result<u64, JobError>;
    /// Requeue (attempts left), kill (exhausted) or cancel (`cancel` requested) up to `limit`
    /// running jobs whose lease ended before now; returns how many.
    async fn reap_expired(&self, limit: u32) -> Result<u32, JobError>;
    async fn get(&self, job: JobId) -> Result<JobRecord, JobError>;
    /// Dead jobs, newest `finished_at` first.
    async fn list_dead(&self, kind: Option<JobKind>, page: PageRequest) -> Result<Page<JobRecord>, JobError>;
    /// `Dead` → `Queued` with attempts and defers reset; `Conflict` if not dead or if an active
    /// job holds the same dedupe key.
    async fn retry_dead(&self, job: JobId) -> Result<(), JobError>;
    /// `Dead` → `Cancelled`; `Conflict` if not dead.
    async fn discard_dead(&self, job: JobId) -> Result<(), JobError>;
    /// Delete succeeded / cancelled jobs finished before `succeeded_before` and dead jobs finished
    /// before `dead_before`, at most `limit`; returns how many.
    async fn purge_finished(&self, succeeded_before: OffsetDateTime, dead_before: OffsetDateTime, limit: u32) -> Result<u64, JobError>;
}

/// FNV-1a, 64 bit.
fn fnv1a64(bytes: impl IntoIterator<Item = u8>) -> u64 {
    bytes.into_iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3))
}

/// Deterministic jitter `u` in `[0, 1)` for failure number `attempt` of `job` (storage.md
/// "Backoff"): `(fnv1a64(job_id ‖ attempt) >> 11) / 2^53`, hashing the job id's 16 uuid bytes
/// followed by `attempt` as big-endian `u16`.
pub fn backoff_jitter(job: JobId, attempt: u16) -> f64 {
    let hash = fnv1a64(job.as_uuid().as_bytes().iter().copied().chain(attempt.to_be_bytes()));
    // 53 significant bits convert to f64 exactly.
    (hash >> 11) as f64 / (1u64 << 53) as f64
}

/// Delay before the next attempt after failure number `attempt` (1-based) under `policy`:
/// `policy.backoff(attempt, backoff_jitter(job, attempt))`, truncated to whole microseconds
/// (timestamptz precision, so stores and fakes agree). Used by `fail(Retryable { retry_at: None })`.
pub fn retry_delay(policy: &RetryPolicy, job: JobId, attempt: u16) -> Duration {
    to_micros(policy.backoff(u32::from(attempt), backoff_jitter(job, attempt)))
}

/// Delay `reap_expired` puts before a requeued job's next attempt: the unjittered backoff of the
/// kind's policy for `attempts` consumed leases, truncated to whole microseconds.
pub fn reap_delay(policy: &RetryPolicy, attempts: u16) -> Duration {
    to_micros(policy.backoff(u32::from(attempts), 1.0))
}

fn to_micros(d: Duration) -> Duration {
    Duration::from_micros(u64::try_from(d.as_micros()).unwrap_or(u64::MAX))
}

/// `error` cut to at most [`LAST_ERROR_MAX_BYTES`] on a char boundary.
pub fn truncate_error(error: &str) -> String {
    if error.len() <= LAST_ERROR_MAX_BYTES {
        return error.to_owned();
    }
    let mut end = LAST_ERROR_MAX_BYTES;
    while !error.is_char_boundary(end) {
        end -= 1;
    }
    error[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;
    use crate::domain::ids::RepoId;
    use crate::domain::job::IndexPriority;

    fn job(n: u128) -> JobId {
        JobId::from_uuid(Uuid::from_u128(n))
    }

    #[test]
    fn fnv1a64_matches_reference_vectors() {
        assert_eq!(fnv1a64(*b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(*b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(*b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn jitter_is_deterministic_in_unit_interval_and_varies_by_job_and_attempt() {
        let u = backoff_jitter(job(1), 1);
        assert_eq!(u, backoff_jitter(job(1), 1));
        assert!((0.0..1.0).contains(&u));
        assert_ne!(u, backoff_jitter(job(1), 2));
        assert_ne!(u, backoff_jitter(job(2), 1));
    }

    #[test]
    fn default_schedule_stays_within_documented_windows() {
        // storage.md "Backoff" table for the default policy.
        let windows = [(1, 2.5, 5.0), (2, 5.0, 10.0), (3, 10.0, 20.0), (5, 40.0, 80.0), (8, 320.0, 640.0), (9, 450.0, 900.0)];
        for n in 0..64 {
            for (attempt, lo, hi) in windows {
                let secs = retry_delay(&RetryPolicy::DEFAULT, job(n), attempt).as_secs_f64();
                assert!((lo..=hi).contains(&secs), "attempt {attempt}: {secs}s outside {lo}..={hi}");
            }
        }
        assert_eq!(reap_delay(&RetryPolicy::DEFAULT, 3), Duration::from_secs(20));
        assert_eq!(reap_delay(&RetryPolicy::DEFAULT, 20), Duration::from_secs(900));
    }

    #[test]
    fn new_job_fills_kind_and_documented_key_and_validates() {
        let org_id = OrgId::from_uuid(Uuid::from_u128(9));
        let payload = JobPayload::IndexFetch {
            org_id,
            repo_id: RepoId::from_uuid(Uuid::from_u128(3)),
            sha: crate::domain::CommitSha::from_bytes([1; 20]),
            prio: IndexPriority::Normal,
        };
        let new = NewJob::new(payload.clone());
        assert_eq!(new.kind, JobKind::IndexFetch);
        assert_eq!(new.dedupe_key, payload.dedupe_key());
        assert_eq!(new.effective_max_attempts(), JobKind::IndexFetch.retry_policy().max_attempts);
        assert_eq!(new.validate(), Ok(()));

        let wrong_kind = NewJob { kind: JobKind::IndexGc, ..new.clone() };
        assert!(matches!(wrong_kind.validate(), Err(JobError::Invalid(_))));
        assert!(matches!(new.clone().with_max_attempts(0).validate(), Err(JobError::Invalid(_))));
        assert!(matches!(new.clone().dedupe("").validate(), Err(JobError::Invalid(_))));
        assert_eq!(new.without_dedupe().dedupe_key, None);
    }

    #[test]
    fn job_state_round_trips_through_its_column_value() {
        for state in [JobState::Queued, JobState::Running, JobState::Succeeded, JobState::Dead, JobState::Cancelled] {
            assert_eq!(state.as_str().parse::<JobState>(), Ok(state));
        }
        assert!("done".parse::<JobState>().is_err());
    }

    #[test]
    fn truncate_error_respects_char_boundaries() {
        let long = "é".repeat(LAST_ERROR_MAX_BYTES);
        let cut = truncate_error(&long);
        assert!(cut.len() <= LAST_ERROR_MAX_BYTES && cut.len() >= LAST_ERROR_MAX_BYTES - 1);
        assert!(cut.chars().all(|c| c == 'é'));
        assert_eq!(truncate_error("short"), "short");
    }
}
