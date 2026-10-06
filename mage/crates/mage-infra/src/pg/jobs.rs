//! `PgJobQueue` and the transactional outbox (storage.md "Job queue semantics", "Transactional
//! outbox"). One `jobs` table is both the queue and the outbox: store writes insert their jobs with
//! [`enqueue_in`] / [`enqueue_all_in`] inside their own transaction, workers lease with
//! `FOR UPDATE SKIP LOCKED`, and every lease-scoped call is fenced by `lease_seq`.
//!
//! Pools: `enqueue` / `enqueue_all` use the tenant pool (the tenant role may insert jobs and read
//! the dedupe columns); every other method is a platform-pool method (storage.md "Tenancy").
//! Wakeups: `pg_notify('mage_jobs', lane)` per new job, delivered at commit.

use std::collections::BTreeSet;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mage_core::domain::{JobId, JobKind, JobLane, JobPayload, OrgId};
use mage_core::ports::clock::Clock;
use mage_core::ports::common::{Cursor, Page, PageRequest};
use mage_core::ports::ids::IdGen;
use mage_core::ports::jobs::{
    CancelOutcome, DEFER_LIMIT_ERROR, Enqueued, FailOutcome, Heartbeat, JobError, JobFailure, JobLease, JobQueue, JobRecord, JobState,
    LEASE_EXPIRED_ERROR, LeaseRequest, LeasedJob, NewJob, reap_delay, retry_delay, truncate_error,
};
use mage_core::domain::job::RetryPolicy;
use serde_json::{Map, Value};
use sqlx::types::Json;
use sqlx::{PgConnection, Row};
use time::OffsetDateTime;
use uuid::Uuid;

use super::error::DbError;
use super::pool::PgPools;
use super::tx::{TxError, begin_platform, notify, retry_tx};

/// NOTIFY channel workers listen on; the payload is the lane (`server` / `indexer`).
pub const JOBS_CHANNEL: &str = "mage_jobs";

/// INSERT attempts when the active duplicate finished between the INSERT and the lookup.
const ENQUEUE_ATTEMPTS: u32 = 3;

/// How long a leased row whose kind or payload this binary cannot decode waits before another
/// worker (a newer binary) may lease it.
const UNDECODABLE_BACKOFF: Duration = Duration::from_secs(60);

/// `t` truncated to whole microseconds: the value `timestamptz` stores and reads back.
fn micros(t: OffsetDateTime) -> OffsetDateTime {
    t.replace_nanosecond(t.nanosecond() / 1_000 * 1_000).unwrap_or(t)
}

fn duration_micros(d: Duration) -> i64 {
    i64::try_from(d.as_micros()).unwrap_or(i64::MAX)
}

// ---- outbox -----------------------------------------------------------------------------------

/// Insert `job` as `queued` on `conn` (inside the caller's transaction when there is one), or find
/// the active duplicate: storage.md "Enqueue". A duplicate is not an error; the existing job is left
/// untouched. New jobs NOTIFY `mage_jobs` (delivered at commit).
pub async fn enqueue_in(conn: &mut PgConnection, job: &NewJob, now: OffsetDateTime, ids: &dyn IdGen) -> Result<Enqueued, DbError> {
    job.validate().map_err(job_error_to_db)?;
    let enqueued = insert_job(conn, job, now, ids).await?;
    if enqueued.is_new() {
        notify(conn, JOBS_CHANNEL, job.kind.lane().as_str()).await?;
    }
    Ok(enqueued)
}

/// [`enqueue_in`] for each job, in order, after validating all of them (an invalid job inserts
/// nothing). One NOTIFY per lane that got a new job.
pub async fn enqueue_all_in(conn: &mut PgConnection, jobs: &[NewJob], now: OffsetDateTime, ids: &dyn IdGen) -> Result<Vec<Enqueued>, DbError> {
    jobs.iter().try_for_each(NewJob::validate).map_err(job_error_to_db)?;
    let mut out = Vec::with_capacity(jobs.len());
    let mut lanes = BTreeSet::new();
    for job in jobs {
        let enqueued = insert_job(conn, job, now, ids).await?;
        if enqueued.is_new() {
            lanes.insert(job.kind.lane().as_str());
        }
        out.push(enqueued);
    }
    for lane in lanes {
        notify(conn, JOBS_CHANNEL, lane).await?;
    }
    Ok(out)
}

fn job_error_to_db(err: JobError) -> DbError {
    match err {
        JobError::NotFound => DbError::NotFound,
        JobError::Conflict | JobError::LeaseLost => DbError::conflict(),
        JobError::Invalid(msg) => DbError::Invalid(msg),
        JobError::Unavailable(msg) => DbError::Unavailable(msg),
    }
}

async fn insert_job(conn: &mut PgConnection, job: &NewJob, now: OffsetDateTime, ids: &dyn IdGen) -> Result<Enqueued, DbError> {
    let org = job.payload.org_id().map(|o| o.as_uuid());
    let kind = job.kind.as_str();
    let max_attempts = i16::try_from(job.effective_max_attempts()).map_err(|_| DbError::Invalid("max_attempts above 32767".into()))?;
    for _ in 0..ENQUEUE_ATTEMPTS {
        let id = ids.uuid_v7();
        let inserted: Option<Uuid> = sqlx::query_scalar(
            "INSERT INTO jobs (id, org_id, kind, lane, payload, dedupe_key, priority, state, run_after, max_attempts, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, 'queued', $8, $9, $10, $10) \
             ON CONFLICT DO NOTHING \
             RETURNING id",
        )
        .bind(id)
        .bind(org)
        .bind(kind)
        .bind(job.kind.lane().as_str())
        .bind(Json(&job.payload))
        .bind(job.dedupe_key.as_deref())
        .bind(job.payload.priority())
        .bind(job.run_after.unwrap_or(now))
        .bind(max_attempts)
        .bind(now)
        .fetch_optional(&mut *conn)
        .await?;
        if let Some(id) = inserted {
            return Ok(Enqueued::New(JobId::from_uuid(id)));
        }
        let Some(key) = job.dedupe_key.as_deref() else {
            // Without a dedupe key only the primary key can conflict.
            return Err(DbError::Conflict { constraint: Some("jobs_pkey".into()) });
        };
        let existing: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM jobs WHERE org_id IS NOT DISTINCT FROM $1 AND kind = $2 AND dedupe_key = $3 AND state IN ('queued','running')",
        )
        .bind(org)
        .bind(kind)
        .bind(key)
        .fetch_optional(&mut *conn)
        .await?;
        if let Some(id) = existing {
            return Ok(Enqueued::Duplicate(JobId::from_uuid(id)));
        }
    }
    Err(DbError::Unavailable(format!("enqueue {kind}: the active duplicate kept finishing ({ENQUEUE_ATTEMPTS} attempts)")))
}

// ---- queue ------------------------------------------------------------------------------------

/// Postgres `JobQueue`.
pub struct PgJobQueue {
    pools: PgPools,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGen>,
    /// `{kind: lease micros}` for the lease and heartbeat SQL.
    lease_by_kind: Value,
    /// `{kind: max_defers}` for the defer SQL.
    max_defers_by_kind: Value,
}

impl PgJobQueue {
    pub fn new(pools: PgPools, clock: Arc<dyn Clock>, ids: Arc<dyn IdGen>) -> Self {
        let per_kind = |f: fn(RetryPolicy) -> Value| -> Value {
            Value::Object(JobKind::all().iter().map(|k| (k.as_str().to_owned(), f(k.retry_policy()))).collect::<Map<String, Value>>())
        };
        Self {
            pools,
            clock,
            ids,
            lease_by_kind: per_kind(|p| Value::from(duration_micros(p.lease))),
            max_defers_by_kind: per_kind(|p| Value::from(p.max_defers)),
        }
    }

    fn now(&self) -> OffsetDateTime {
        micros(self.clock.now())
    }
}

const DEFAULT_LEASE_MICROS: i64 = 60_000_000;

/// `RetryPolicy` of a stored kind; kinds this binary does not know use the default.
fn policy_of(kind: &str) -> RetryPolicy {
    JobKind::from_str(kind).map(JobKind::retry_policy).unwrap_or(RetryPolicy::DEFAULT)
}

fn parse_kind(kind: &str) -> Result<JobKind, DbError> {
    JobKind::from_str(kind).map_err(|e| DbError::Invalid(e.to_string()))
}

fn to_u16(n: i16) -> u16 {
    u16::try_from(n).unwrap_or(0)
}

fn log_dead(kind: &str, id: Uuid, org: Option<Uuid>, error: Option<&str>) {
    tracing::error!(
        metric = "mage_jobs_dead_total",
        kind,
        job_id = %JobId::from_uuid(id),
        org_id = ?org.map(OrgId::from_uuid),
        last_error = error.unwrap_or(""),
        "job dead-lettered"
    );
}

const RECORD_COLUMNS: &str =
    "id, kind, org_id, payload, dedupe_key, state, attempts, max_attempts, defers, run_after, last_error, created_at, finished_at";

fn record_from_row(row: &sqlx::postgres::PgRow) -> Result<JobRecord, DbError> {
    let kind: String = row.try_get("kind")?;
    let state: String = row.try_get("state")?;
    let Json(payload): Json<JobPayload> = row.try_get("payload")?;
    Ok(JobRecord {
        id: JobId::from_uuid(row.try_get("id")?),
        kind: parse_kind(&kind)?,
        org_id: row.try_get::<Option<Uuid>, _>("org_id")?.map(OrgId::from_uuid),
        payload,
        dedupe_key: row.try_get("dedupe_key")?,
        state: JobState::from_str(&state).map_err(|e| DbError::Invalid(e.to_string()))?,
        attempts: to_u16(row.try_get("attempts")?),
        max_attempts: to_u16(row.try_get("max_attempts")?),
        defers: to_u16(row.try_get("defers")?),
        run_after: row.try_get("run_after")?,
        last_error: row.try_get("last_error")?,
        created_at: row.try_get("created_at")?,
        finished_at: row.try_get("finished_at")?,
    })
}

/// Dead-letter cursor: `{finished_at unix micros}/{job uuid}`.
fn encode_dead(finished: OffsetDateTime, id: Uuid) -> Cursor {
    Cursor(format!("{}/{}", finished.unix_timestamp_nanos() / 1_000, id))
}

fn decode_dead(cursor: &Cursor) -> Result<(OffsetDateTime, Uuid), JobError> {
    let invalid = || JobError::Invalid(format!("bad cursor {:?}", cursor.0));
    let (us, id) = cursor.0.split_once('/').ok_or_else(invalid)?;
    let us: i128 = us.parse().map_err(|_| invalid())?;
    let at = OffsetDateTime::from_unix_timestamp_nanos(us * 1_000).map_err(|_| invalid())?;
    Ok((at, Uuid::try_parse(id).map_err(|_| invalid())?))
}

#[async_trait]
impl JobQueue for PgJobQueue {
    async fn enqueue(&self, job: NewJob) -> Result<Enqueued, JobError> {
        job.validate()?;
        let now = self.now();
        retry_tx(|| async {
            let mut tx = self.pools.tenant.begin().await?;
            let enqueued = enqueue_in(&mut tx, &job, now, &*self.ids).await?;
            tx.commit().await?;
            Ok::<_, TxError<JobError>>(enqueued)
        })
        .await
    }

    async fn enqueue_all(&self, jobs: &[NewJob]) -> Result<Vec<Enqueued>, JobError> {
        jobs.iter().try_for_each(NewJob::validate)?;
        let now = self.now();
        retry_tx(|| async {
            let mut tx = self.pools.tenant.begin().await?;
            let enqueued = enqueue_all_in(&mut tx, jobs, now, &*self.ids).await?;
            tx.commit().await?;
            Ok::<_, TxError<JobError>>(enqueued)
        })
        .await
    }

    async fn lease(&self, req: &LeaseRequest) -> Result<Vec<LeasedJob>, JobError> {
        if req.max == 0 || req.kinds.as_ref().is_some_and(Vec::is_empty) {
            return Ok(Vec::new());
        }
        let now = self.now();
        let kinds: Option<Vec<&str>> = req.kinds.as_ref().map(|ks| ks.iter().map(|k| k.as_str()).collect());
        retry_tx(|| async {
            let mut tx = begin_platform(&self.pools.platform).await?;
            let rows = sqlx::query(
                "WITH picked AS ( \
                   SELECT id FROM jobs \
                   WHERE state = 'queued' AND lane = $1 AND run_after <= $2 \
                     AND ($3::text[] IS NULL OR kind = ANY ($3)) \
                   ORDER BY priority DESC, run_after, id \
                   LIMIT $4 \
                   FOR UPDATE SKIP LOCKED) \
                 UPDATE jobs j \
                 SET state = 'running', attempts = j.attempts + 1, lease_seq = j.lease_seq + 1, leased_by = $5, \
                     leased_until = $2 + COALESCE(($6::jsonb ->> j.kind)::bigint, $7) * interval '1 microsecond', \
                     updated_at = $2 \
                 FROM picked WHERE j.id = picked.id \
                 RETURNING j.id, j.lease_seq, j.kind, j.org_id, j.payload, j.attempts, j.max_attempts, j.leased_until, \
                           j.created_at, j.priority, j.run_after",
            )
            .bind(req.lane.as_str())
            .bind(now)
            .bind(kinds.as_deref())
            .bind(i64::from(req.max))
            .bind(&req.worker)
            .bind(&self.lease_by_kind)
            .bind(DEFAULT_LEASE_MICROS)
            .fetch_all(&mut *tx)
            .await?;

            let mut leased = Vec::with_capacity(rows.len());
            let mut undecodable = Vec::new();
            for row in &rows {
                let id: Uuid = row.try_get("id")?;
                let kind: String = row.try_get("kind")?;
                let decoded = JobKind::from_str(&kind)
                    .map_err(|e| e.to_string())
                    .and_then(|k| row.try_get::<Json<JobPayload>, _>("payload").map(|p| (k, p.0)).map_err(|e| e.to_string()));
                let (kind, payload) = match decoded {
                    Ok(decoded) => decoded,
                    Err(error) => {
                        tracing::warn!(job_id = %JobId::from_uuid(id), kind, error, "leased a job this binary cannot decode; returning it");
                        undecodable.push(id);
                        continue;
                    }
                };
                let priority: i16 = row.try_get("priority")?;
                let run_after: OffsetDateTime = row.try_get("run_after")?;
                let seq: i32 = row.try_get("lease_seq")?;
                leased.push((
                    (std::cmp::Reverse(priority), run_after, id),
                    LeasedJob {
                        lease: JobLease { job_id: JobId::from_uuid(id), lease_seq: u32::try_from(seq).unwrap_or(0) },
                        kind,
                        org_id: row.try_get::<Option<Uuid>, _>("org_id")?.map(OrgId::from_uuid),
                        payload,
                        attempt: to_u16(row.try_get("attempts")?),
                        max_attempts: to_u16(row.try_get("max_attempts")?),
                        leased_until: row.try_get("leased_until")?,
                        enqueued_at: row.try_get("created_at")?,
                    },
                ));
            }
            if !undecodable.is_empty() {
                sqlx::query(
                    "UPDATE jobs SET state = 'queued', attempts = attempts - 1, leased_by = NULL, leased_until = NULL, \
                            run_after = $2, updated_at = $3 \
                     WHERE id = ANY ($1)",
                )
                .bind(&undecodable)
                .bind(now + UNDECODABLE_BACKOFF)
                .bind(now)
                .execute(&mut *tx)
                .await?;
            }
            tx.commit().await?;
            // UPDATE … RETURNING does not keep the CTE's order.
            leased.sort_by(|a, b| a.0.cmp(&b.0));
            Ok::<_, TxError<JobError>>(leased.into_iter().map(|(_, job)| job).collect())
        })
        .await
    }

    async fn heartbeat(&self, lease: &JobLease) -> Result<Heartbeat, JobError> {
        let now = self.now();
        let row: Option<(OffsetDateTime, bool)> = sqlx::query_as(
            "UPDATE jobs SET leased_until = $3 + COALESCE(($4::jsonb ->> kind)::bigint, $5) * interval '1 microsecond', updated_at = $3 \
             WHERE id = $1 AND lease_seq = $2 AND state = 'running' \
             RETURNING leased_until, cancel_requested",
        )
        .bind(lease.job_id.as_uuid())
        .bind(lease_seq(lease))
        .bind(now)
        .bind(&self.lease_by_kind)
        .bind(DEFAULT_LEASE_MICROS)
        .fetch_optional(&self.pools.platform)
        .await
        .map_err(DbError::from)?;
        match row {
            None => Err(JobError::LeaseLost),
            Some((until, true)) => Ok(Heartbeat::CancelRequested { until }),
            Some((until, false)) => Ok(Heartbeat::Extended { until }),
        }
    }

    async fn complete(&self, lease: &JobLease, follow_ups: &[NewJob]) -> Result<(), JobError> {
        let now = self.now();
        retry_tx(|| async {
            let mut tx = begin_platform(&self.pools.platform).await?;
            let done = sqlx::query(
                "UPDATE jobs SET state = 'succeeded', leased_until = NULL, leased_by = NULL, finished_at = $3, updated_at = $3 \
                 WHERE id = $1 AND lease_seq = $2 AND state = 'running'",
            )
            .bind(lease.job_id.as_uuid())
            .bind(lease_seq(lease))
            .bind(now)
            .execute(&mut *tx)
            .await?;
            if done.rows_affected() == 0 {
                return Err(TxError::Port(JobError::LeaseLost));
            }
            enqueue_all_in(&mut tx, follow_ups, now, &*self.ids).await?;
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    async fn fail(&self, lease: &JobLease, failure: JobFailure) -> Result<FailOutcome, JobError> {
        let now = self.now();
        retry_tx(|| async {
            let mut tx = begin_platform(&self.pools.platform).await?;
            let row: Option<(i16, i16, String, Option<Uuid>)> = sqlx::query_as(
                "SELECT attempts, max_attempts, kind, org_id FROM jobs \
                 WHERE id = $1 AND lease_seq = $2 AND state = 'running' FOR UPDATE",
            )
            .bind(lease.job_id.as_uuid())
            .bind(lease_seq(lease))
            .fetch_optional(&mut *tx)
            .await?;
            let Some((attempts, max_attempts, kind, org)) = row else {
                return Err(TxError::Port(JobError::LeaseLost));
            };
            let id = lease.job_id.as_uuid();
            let (state, run_after, last_error, outcome): (&str, Option<OffsetDateTime>, Option<String>, FailOutcome) = match &failure {
                JobFailure::Retryable { error, retry_at } if attempts < max_attempts => {
                    let at = micros(retry_at.unwrap_or_else(|| now + retry_delay(&policy_of(&kind), lease.job_id, to_u16(attempts))));
                    ("queued", Some(at), Some(truncate_error(error)), FailOutcome::Retrying { at })
                }
                JobFailure::Retryable { error, .. } | JobFailure::Permanent { error } => ("dead", None, Some(truncate_error(error)), FailOutcome::Dead),
                JobFailure::Cancelled => ("cancelled", None, None, FailOutcome::Cancelled),
            };
            let finished = (state != "queued").then_some(now);
            sqlx::query(
                "UPDATE jobs SET state = $2, run_after = COALESCE($3, run_after), last_error = COALESCE($4, last_error), \
                        finished_at = $5, leased_until = NULL, leased_by = NULL, updated_at = $6 \
                 WHERE id = $1",
            )
            .bind(id)
            .bind(state)
            .bind(run_after)
            .bind(last_error.as_deref())
            .bind(finished)
            .bind(now)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            if outcome == FailOutcome::Dead {
                log_dead(&kind, id, org, last_error.as_deref());
            }
            Ok(outcome)
        })
        .await
    }

    async fn defer(&self, lease: &JobLease, until: OffsetDateTime) -> Result<(), JobError> {
        let now = self.now();
        let row: Option<(String, String, Option<Uuid>)> = sqlx::query_as(
            "UPDATE jobs SET \
               state        = CASE WHEN defers + 1 > COALESCE(($4::jsonb ->> kind)::int, $5) THEN 'dead' ELSE 'queued' END, \
               finished_at  = CASE WHEN defers + 1 > COALESCE(($4::jsonb ->> kind)::int, $5) THEN $3 END, \
               last_error   = CASE WHEN defers + 1 > COALESCE(($4::jsonb ->> kind)::int, $5) THEN $6 ELSE last_error END, \
               run_after    = $7, attempts = attempts - 1, defers = defers + 1, \
               leased_until = NULL, leased_by = NULL, updated_at = $3 \
             WHERE id = $1 AND lease_seq = $2 AND state = 'running' \
             RETURNING state, kind, org_id",
        )
        .bind(lease.job_id.as_uuid())
        .bind(lease_seq(lease))
        .bind(now)
        .bind(&self.max_defers_by_kind)
        .bind(i32::from(RetryPolicy::DEFAULT.max_defers))
        .bind(DEFER_LIMIT_ERROR)
        .bind(until)
        .fetch_optional(&self.pools.platform)
        .await
        .map_err(DbError::from)?;
        match row {
            None => Err(JobError::LeaseLost),
            Some((state, kind, org)) => {
                if state == "dead" {
                    log_dead(&kind, lease.job_id.as_uuid(), org, Some(DEFER_LIMIT_ERROR));
                }
                Ok(())
            }
        }
    }

    async fn cancel(&self, job: JobId) -> Result<CancelOutcome, JobError> {
        let now = self.now();
        retry_tx(|| async {
            let mut tx = begin_platform(&self.pools.platform).await?;
            let state: Option<String> =
                sqlx::query_scalar("SELECT state FROM jobs WHERE id = $1 FOR UPDATE").bind(job.as_uuid()).fetch_optional(&mut *tx).await?;
            let outcome = match state.as_deref() {
                None => return Err(TxError::Port(JobError::NotFound)),
                Some("queued") => {
                    sqlx::query("UPDATE jobs SET state = 'cancelled', finished_at = $2, updated_at = $2 WHERE id = $1")
                        .bind(job.as_uuid())
                        .bind(now)
                        .execute(&mut *tx)
                        .await?;
                    CancelOutcome::Cancelled
                }
                Some("running") => {
                    sqlx::query("UPDATE jobs SET cancel_requested = true, updated_at = $2 WHERE id = $1")
                        .bind(job.as_uuid())
                        .bind(now)
                        .execute(&mut *tx)
                        .await?;
                    CancelOutcome::Requested
                }
                Some(_) => CancelOutcome::AlreadyFinished,
            };
            tx.commit().await?;
            Ok(outcome)
        })
        .await
    }

    async fn cancel_org(&self, org: OrgId) -> Result<u64, JobError> {
        let now = self.now();
        let count: i64 = sqlx::query_scalar(
            "WITH queued AS ( \
               UPDATE jobs SET state = 'cancelled', finished_at = $2, updated_at = $2 \
               WHERE org_id = $1 AND state = 'queued' RETURNING 1), \
             running AS ( \
               UPDATE jobs SET cancel_requested = true, updated_at = $2 \
               WHERE org_id = $1 AND state = 'running' RETURNING 1) \
             SELECT (SELECT count(*) FROM queued) + (SELECT count(*) FROM running)",
        )
        .bind(org.as_uuid())
        .bind(now)
        .fetch_one(&self.pools.platform)
        .await
        .map_err(DbError::from)?;
        Ok(u64::try_from(count).unwrap_or(0))
    }

    async fn reap_expired(&self, limit: u32) -> Result<u32, JobError> {
        let now = self.now();
        retry_tx(|| async {
            let mut tx = begin_platform(&self.pools.platform).await?;
            let rows: Vec<(Uuid, String, Option<Uuid>, i16, i16, bool)> = sqlx::query_as(
                "SELECT id, kind, org_id, attempts, max_attempts, cancel_requested FROM jobs \
                 WHERE state = 'running' AND leased_until < $1 \
                 ORDER BY leased_until LIMIT $2 FOR UPDATE SKIP LOCKED",
            )
            .bind(now)
            .bind(i64::from(limit))
            .fetch_all(&mut *tx)
            .await?;
            let mut ids = Vec::with_capacity(rows.len());
            let mut states = Vec::with_capacity(rows.len());
            let mut run_afters: Vec<Option<OffsetDateTime>> = Vec::with_capacity(rows.len());
            let mut finished: Vec<Option<OffsetDateTime>> = Vec::with_capacity(rows.len());
            let mut dead = Vec::new();
            for (id, kind, org, attempts, max_attempts, cancel_requested) in &rows {
                let (state, run_after) = if *cancel_requested {
                    ("cancelled", None)
                } else if attempts < max_attempts {
                    ("queued", Some(micros(now + reap_delay(&policy_of(kind), to_u16(*attempts)))))
                } else {
                    dead.push((kind.clone(), *id, *org));
                    ("dead", None)
                };
                ids.push(*id);
                states.push(state);
                finished.push((state != "queued").then_some(now));
                run_afters.push(run_after);
            }
            if !ids.is_empty() {
                sqlx::query(
                    "UPDATE jobs j SET state = u.state, run_after = COALESCE(u.run_after, j.run_after), finished_at = u.finished_at, \
                            last_error = $5, leased_until = NULL, leased_by = NULL, updated_at = $6 \
                     FROM UNNEST($1::uuid[], $2::text[], $3::timestamptz[], $4::timestamptz[]) AS u(id, state, run_after, finished_at) \
                     WHERE j.id = u.id",
                )
                .bind(&ids)
                .bind(&states)
                .bind(&run_afters)
                .bind(&finished)
                .bind(LEASE_EXPIRED_ERROR)
                .bind(now)
                .execute(&mut *tx)
                .await?;
            }
            tx.commit().await?;
            for (kind, id, org) in &dead {
                log_dead(kind, *id, *org, Some(LEASE_EXPIRED_ERROR));
            }
            Ok::<_, TxError<JobError>>(u32::try_from(ids.len()).unwrap_or(u32::MAX))
        })
        .await
    }

    async fn get(&self, job: JobId) -> Result<JobRecord, JobError> {
        let row = sqlx::query(&format!("SELECT {RECORD_COLUMNS} FROM jobs WHERE id = $1"))
            .bind(job.as_uuid())
            .fetch_optional(&self.pools.platform)
            .await
            .map_err(DbError::from)?
            .ok_or(JobError::NotFound)?;
        Ok(record_from_row(&row)?)
    }

    async fn list_dead(&self, kind: Option<JobKind>, page: PageRequest) -> Result<Page<JobRecord>, JobError> {
        let after = page.after.as_ref().map(decode_dead).transpose()?;
        let limit = page.effective_limit();
        let rows = sqlx::query(&format!(
            "SELECT {RECORD_COLUMNS} FROM jobs \
             WHERE state = 'dead' AND ($1::text IS NULL OR kind = $1) \
               AND ($2::timestamptz IS NULL OR (finished_at, id) < ($2, $3)) \
             ORDER BY finished_at DESC, id DESC LIMIT $4"
        ))
        .bind(kind.map(JobKind::as_str))
        .bind(after.map(|a| a.0))
        .bind(after.map(|a| a.1))
        .bind(i64::try_from(limit + 1).unwrap_or(i64::MAX))
        .fetch_all(&self.pools.platform)
        .await
        .map_err(DbError::from)?;
        let mut items = rows.iter().map(record_from_row).collect::<Result<Vec<_>, _>>()?;
        let next = if items.len() > limit {
            items.truncate(limit);
            items.last().and_then(|r| r.finished_at.map(|f| encode_dead(f, r.id.as_uuid())))
        } else {
            None
        };
        Ok(Page { items, next })
    }

    async fn retry_dead(&self, job: JobId) -> Result<(), JobError> {
        let now = self.now();
        let updated = sqlx::query(
            "UPDATE jobs SET state = 'queued', attempts = 0, defers = 0, run_after = $2, finished_at = NULL, \
                    cancel_requested = false, updated_at = $2 \
             WHERE id = $1 AND state = 'dead'",
        )
        .bind(job.as_uuid())
        .bind(now)
        .execute(&self.pools.platform)
        .await
        .map_err(DbError::from)?;
        if updated.rows_affected() == 1 {
            return Ok(());
        }
        self.missing_or_conflict(job).await
    }

    async fn discard_dead(&self, job: JobId) -> Result<(), JobError> {
        let now = self.now();
        let updated = sqlx::query("UPDATE jobs SET state = 'cancelled', finished_at = $2, updated_at = $2 WHERE id = $1 AND state = 'dead'")
            .bind(job.as_uuid())
            .bind(now)
            .execute(&self.pools.platform)
            .await
            .map_err(DbError::from)?;
        if updated.rows_affected() == 1 {
            return Ok(());
        }
        self.missing_or_conflict(job).await
    }

    async fn purge_finished(&self, succeeded_before: OffsetDateTime, dead_before: OffsetDateTime, limit: u32) -> Result<u64, JobError> {
        let deleted = sqlx::query(
            "WITH doomed AS ( \
               SELECT id FROM jobs \
               WHERE (state IN ('succeeded','cancelled') AND finished_at < $1) OR (state = 'dead' AND finished_at < $2) \
               ORDER BY finished_at LIMIT $3 FOR UPDATE SKIP LOCKED) \
             DELETE FROM jobs j USING doomed WHERE j.id = doomed.id",
        )
        .bind(succeeded_before)
        .bind(dead_before)
        .bind(i64::from(limit))
        .execute(&self.pools.platform)
        .await
        .map_err(DbError::from)?;
        Ok(deleted.rows_affected())
    }
}

impl PgJobQueue {
    /// After a state-guarded update matched nothing: `NotFound` if the job is absent, else
    /// `Conflict` (wrong state).
    async fn missing_or_conflict(&self, job: JobId) -> Result<(), JobError> {
        let exists: Option<i32> =
            sqlx::query_scalar("SELECT 1 FROM jobs WHERE id = $1").bind(job.as_uuid()).fetch_optional(&self.pools.platform).await.map_err(DbError::from)?;
        Err(if exists.is_some() { JobError::Conflict } else { JobError::NotFound })
    }
}

fn lease_seq(lease: &JobLease) -> i32 {
    i32::try_from(lease.lease_seq).unwrap_or(i32::MAX)
}

/// Wakeups for one lane's worker: a task holding a dedicated `PgListener` on [`JOBS_CHANNEL`]
/// that calls `wake.notify_one()` for every notification naming `lane`. A lost connection is
/// re-established with backoff (notifications in between are covered by the worker's poll) and
/// `wake` is notified after each reconnect. Ends when `shutdown` is cancelled.
pub fn spawn_wakeup_listener(
    pool: sqlx::PgPool,
    lane: JobLane,
    wake: Arc<tokio::sync::Notify>,
    shutdown: tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut backoff = Duration::from_millis(250);
        while !shutdown.is_cancelled() {
            let mut listener = match sqlx::postgres::PgListener::connect_with(&pool).await {
                Ok(listener) => listener,
                Err(e) => {
                    tracing::warn!(error = %e, lane = lane.as_str(), "job wakeup listener cannot connect");
                    tokio::select! {
                        () = shutdown.cancelled() => return,
                        () = tokio::time::sleep(backoff) => {}
                    }
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                    continue;
                }
            };
            if let Err(e) = listener.listen(JOBS_CHANNEL).await {
                tracing::warn!(error = %e, lane = lane.as_str(), "job wakeup listener cannot LISTEN");
                tokio::select! {
                    () = shutdown.cancelled() => return,
                    () = tokio::time::sleep(backoff) => {}
                }
                backoff = (backoff * 2).min(Duration::from_secs(30));
                continue;
            }
            backoff = Duration::from_millis(250);
            wake.notify_one();
            loop {
                tokio::select! {
                    () = shutdown.cancelled() => return,
                    received = listener.try_recv() => match received {
                        Ok(Some(note)) if note.payload() == lane.as_str() => wake.notify_one(),
                        Ok(Some(_)) => {}
                        Ok(None) => {
                            tracing::debug!(lane = lane.as_str(), "job wakeup listener reconnected");
                            wake.notify_one();
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, lane = lane.as_str(), "job wakeup listener lost its connection");
                            break;
                        }
                    },
                }
            }
        }
    })
}

/// The kind→lane check every row satisfies; exposed for workers that build `LeaseRequest`s from
/// configured kind prefixes (`server.jobs.kinds = ["intake.", …]`).
pub fn kinds_with_prefixes(lane: JobLane, prefixes: &[String]) -> Vec<JobKind> {
    JobKind::all().iter().copied().filter(|k| k.lane() == lane && prefixes.iter().any(|p| k.as_str().starts_with(p.as_str()))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dead_cursor_round_trips_at_microsecond_precision() {
        let at = OffsetDateTime::from_unix_timestamp_nanos(1_767_225_600_123_456_000).unwrap_or(OffsetDateTime::UNIX_EPOCH);
        let id = Uuid::from_u128(42);
        assert_eq!(decode_dead(&encode_dead(at, id)), Ok((at, id)));
        assert!(matches!(decode_dead(&Cursor("nope".into())), Err(JobError::Invalid(_))));
    }

    #[test]
    fn kinds_with_prefixes_stay_in_the_lane() {
        let index = kinds_with_prefixes(JobLane::Indexer, &["index.".into()]);
        assert!(!index.is_empty() && index.iter().all(|k| k.as_str().starts_with("index.")));
        assert!(kinds_with_prefixes(JobLane::Server, &["index.".into()]).is_empty(), "index kinds are indexer-lane only");
        let triage = kinds_with_prefixes(JobLane::Server, &["triage.".into()]);
        assert!(triage.contains(&JobKind::TriageTicket));
    }

    #[test]
    fn micros_truncates_like_timestamptz() {
        let t = OffsetDateTime::from_unix_timestamp_nanos(1_000_001_999).unwrap_or(OffsetDateTime::UNIX_EPOCH);
        assert_eq!(micros(t).unix_timestamp_nanos(), 1_000_001_000);
    }
}
