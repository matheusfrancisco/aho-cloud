//! `RunStore` over Postgres (runs.md "Adapter internals"; tables in `0004_runs.sql`).
//!
//! - Tenant methods run in `begin_org` on the tenant pool; cross-tenant ones (`locate`,
//!   `lease_next`, `lease_by_request`, `due`, `held_by`, `record_runner_session`,
//!   `runner_session`, `reply`) on the platform pool.
//! - `commit` is one transaction: idempotent reply first, inserts (`ON CONFLICT` on the create
//!   key), version-checked updates (a lease with the same identity keeps the later
//!   `lease_until`), gates, ingest + event seqs from `runs.next_seq`, command seqs from
//!   `runs.next_cmd_seq`, artifact completion, the jobs outbox, org-feed rows for status changes,
//!   and `pg_notify` per topic (delivered at commit).
//! - `lease_next` locks candidates with `FOR UPDATE SKIP LOCKED`, filters capabilities in Rust,
//!   and takes the first whose repo slot is free; a `runs_repo_work_slot` violation (inside a
//!   savepoint) moves on to the next candidate, at most 5 times.
//! - `extend_lease` touches only `lease_until` (never `version`).
//! - `usage_reported` holds `{total, frozen, run}` (reported total, the frozen share of earlier
//!   attempts, and the effective usage shown on the run).

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Postgres, QueryBuilder, Row};
use time::OffsetDateTime;
use uuid::Uuid;

use mage_core::domain::{
    Artifact, ArtifactId, ArtifactKind, Budget, CommitSha, Gate, GateId, GateKind, GateStatus, HarnessKind, OrgId, RepoId, RepoVersionKey, Run,
    RunData, RunError, RunEvent, RunEventKind, RunId, RunKind, RunResult, RunStatus, RunSubject, RunnerId, RunnerLease, TicketId, Usage,
};
use mage_core::ports::blob::BlobKey;
use mage_core::ports::clock::Clock;
use mage_core::ports::ids::IdGen;
use mage_infra::pg::{DbError, PgPools, begin_org, begin_platform, notify};
use mage_infra::pg::jobs::enqueue_all_in;

use crate::adapters::driven::pg::codec::{fixed, opt_sha, to_i64, to_unsigned};
use crate::adapters::driven::pg::feed::append_in;
use crate::adapters::driven::pg::signals::{CHANNEL_QUEUE, CHANNEL_RUN_COMMANDS, CHANNEL_RUN_EVENTS, commands_payload, events_payload};
use crate::domain::runs::RunSpec;
use crate::ports::feed::FeedKind;
use crate::ports::runs::{
    Affinity, ArtifactRecord, ArtifactState, CommandRecord, CommitReceipt, DueItem, DueReason, DueThresholds, GateRecord, GateWrite,
    IngestCursor, LeaseFence, LeaseMeta, LeaseQuery, NewArtifact, NewRun, RunConflict, RunQuery, RunRecord, RunStore, RunStoreError, RunUpdate,
    RunUsage, RunWrite, RunnerSession, StoredReply, WallClock,
};

/// Prefix of `Unavailable` for serialization failures / deadlocks: `commit` reruns those.
const RETRY_MARK: &str = "transaction retry: ";
/// Runs of one `commit` transaction (the first plus two reruns).
const COMMIT_ATTEMPTS: u32 = 3;

/// Candidates locked per `lease_next` pass; slot conflicts tried per call.
const LEASE_CANDIDATES: i64 = 50;
const SLOT_RETRIES: usize = 5;

const RUN_COLUMNS: &str = "org_id, id, kind, ticket_id, cause_id, parent_run_id, repo_id, sha, harness, status, attempt, version, spec, \
    slot_class, priority, source_ready, budget, usage_reported, usage_metered, wall_accum_ms, wall_segment_start, runner_id, lease_until, \
    lease_request_id, runner_instance, session_lost, affinity_runner, affinity_until, next_seq, next_cmd_seq, ingest_attempt, \
    ingest_local_seq, result, error, created_at, started_at, finished_at";

const GATE_COLUMNS: &str = "org_id, id, run_id, kind, attempt, status, reason, feedback, details, verify_run, created_at, decided_at";

const ARTIFACT_COLUMNS: &str = "org_id, id, run_id, attempt, kind, name, key, content_type, bytes, sha256, state, created_at";

impl From<DbError> for RunStoreError {
    fn from(err: DbError) -> Self {
        match err {
            DbError::NotFound => RunStoreError::NotFound,
            DbError::Conflict { constraint } if constraint.as_deref() == Some("runs_one_active_per_subject") => {
                RunStoreError::Conflict(RunConflict::SubjectBusy)
            }
            DbError::Conflict { constraint } => RunStoreError::Invalid(format!("unique violation on {}", constraint.unwrap_or_default())),
            DbError::Invalid(m) => RunStoreError::Invalid(m),
            DbError::Unavailable(m) => RunStoreError::Unavailable(m),
            DbError::Retry(m) => RunStoreError::Unavailable(format!("{RETRY_MARK}{m}")),
            DbError::Timeout => RunStoreError::Unavailable("statement timeout".into()),
        }
    }
}

fn db(e: sqlx::Error) -> RunStoreError {
    DbError::from(e).into()
}

/// `usage_reported` column.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
struct ReportedColumn {
    total: Usage,
    frozen: Usage,
    /// `Run.usage` (effective usage at the last write).
    run: Usage,
}

pub(crate) fn kind_str(kind: RunKind) -> &'static str {
    match kind {
        RunKind::Learn => "learn",
        RunKind::Triage => "triage",
        RunKind::Diagnose => "diagnose",
        RunKind::Verify => "verify",
        RunKind::Fix => "fix",
    }
}

fn parse_kind(s: &str) -> Result<RunKind, RunStoreError> {
    [RunKind::Learn, RunKind::Triage, RunKind::Diagnose, RunKind::Verify, RunKind::Fix]
        .into_iter()
        .find(|k| kind_str(*k) == s)
        .ok_or_else(|| RunStoreError::Invalid(format!("runs.kind: unknown {s:?}")))
}

fn parse_status(s: &str) -> Result<RunStatus, RunStoreError> {
    use RunStatus as S;
    [S::Queued, S::Leased, S::Preparing, S::Running, S::AwaitingGate, S::Succeeded, S::Failed, S::TimedOut, S::Cancelled, S::Errored]
        .into_iter()
        .find(|st| st.name() == s)
        .ok_or_else(|| RunStoreError::Invalid(format!("runs.status: unknown {s:?}")))
}

fn harness_str(h: HarnessKind) -> &'static str {
    match h {
        HarnessKind::Omp => "omp",
        HarnessKind::ClaudeCode => "claude_code",
    }
}

fn parse_harness(s: &str) -> Result<HarnessKind, RunStoreError> {
    match s {
        "omp" => Ok(HarnessKind::Omp),
        "claude_code" => Ok(HarnessKind::ClaudeCode),
        other => Err(RunStoreError::Invalid(format!("runs.harness: unknown {other:?}"))),
    }
}

fn slot_str(s: mage_core::wire::runner::SlotClass) -> &'static str {
    match s {
        mage_core::wire::runner::SlotClass::Light => "light",
        mage_core::wire::runner::SlotClass::Work => "work",
        mage_core::wire::runner::SlotClass::Verify => "verify",
    }
}

fn gate_kind_str(k: GateKind) -> &'static str {
    match k {
        GateKind::Evidence => "evidence",
        GateKind::Boot => "boot",
        GateKind::Repro => "repro",
        GateKind::Fix => "fix",
    }
}

fn parse_gate_kind(s: &str) -> Result<GateKind, RunStoreError> {
    [GateKind::Evidence, GateKind::Boot, GateKind::Repro, GateKind::Fix]
        .into_iter()
        .find(|k| gate_kind_str(*k) == s)
        .ok_or_else(|| RunStoreError::Invalid(format!("gates.kind: unknown {s:?}")))
}

const ARTIFACT_KINDS: [ArtifactKind; 11] = [
    ArtifactKind::Video,
    ArtifactKind::Gif,
    ArtifactKind::Trace,
    ArtifactKind::Screenshot,
    ArtifactKind::Log,
    ArtifactKind::Script,
    ArtifactKind::Patch,
    ArtifactKind::Transcript,
    ArtifactKind::ScipIndex,
    ArtifactKind::SourceBundle,
    ArtifactKind::FixBrief,
];

fn parse_artifact_kind(s: &str) -> Result<ArtifactKind, RunStoreError> {
    ARTIFACT_KINDS.into_iter().find(|k| k.as_str() == s).ok_or_else(|| RunStoreError::Invalid(format!("artifacts.kind: unknown {s:?}")))
}

fn json<T: Serialize>(v: &T, column: &str) -> Result<serde_json::Value, RunStoreError> {
    serde_json::to_value(v).map_err(|e| RunStoreError::Invalid(format!("{column}: {e}")))
}

fn from_json<T: serde::de::DeserializeOwned>(v: serde_json::Value, column: &str) -> Result<T, RunStoreError> {
    serde_json::from_value(v).map_err(|e| RunStoreError::Invalid(format!("{column} does not decode: {e}")))
}

fn ms(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

fn get<'r, T: sqlx::Decode<'r, Postgres> + sqlx::Type<Postgres>>(row: &'r PgRow, col: &str) -> Result<T, RunStoreError> {
    row.try_get(col).map_err(db)
}

fn decode_run(row: &PgRow) -> Result<RunRecord, RunStoreError> {
    let org_id = OrgId::from_uuid(get(row, "org_id")?);
    let id = RunId::from_uuid(get(row, "id")?);
    let kind = parse_kind(&get::<String>(row, "kind")?)?;
    let repo_id: Option<RepoId> = get::<Option<Uuid>>(row, "repo_id")?.map(RepoId::from_uuid);
    let sha = opt_sha(get(row, "sha")?, "runs.sha")?;
    let ticket_id: Option<TicketId> = get::<Option<Uuid>>(row, "ticket_id")?.map(TicketId::from_uuid);
    let subject = match kind {
        RunKind::Learn => match (repo_id, sha) {
            (Some(r), Some(s)) => RunSubject::RepoVersion(RepoVersionKey::new(r, s)),
            _ => return Err(RunStoreError::Invalid("learn run without repo/sha".into())),
        },
        RunKind::Triage => RunSubject::Ticket(ticket_id.ok_or_else(|| RunStoreError::Invalid("triage run without ticket".into()))?),
        RunKind::Diagnose | RunKind::Fix => RunSubject::Cause(
            get::<Option<Uuid>>(row, "cause_id")?.map(Into::into).ok_or_else(|| RunStoreError::Invalid("cause run without cause".into()))?,
        ),
        RunKind::Verify => RunSubject::Run(
            get::<Option<Uuid>>(row, "parent_run_id")?.map(RunId::from_uuid).ok_or_else(|| RunStoreError::Invalid("verify run without parent".into()))?,
        ),
    };
    let attempt: u16 = to_unsigned(i64::from(get::<i16>(row, "attempt")?), "runs.attempt")?;
    let runner_id: Option<RunnerId> = get::<Option<Uuid>>(row, "runner_id")?.map(RunnerId::from_uuid);
    let lease_until: Option<OffsetDateTime> = get(row, "lease_until")?;
    let instance: Option<Uuid> = get(row, "runner_instance")?;
    let request_id: Option<Uuid> = get(row, "lease_request_id")?;
    let reported: ReportedColumn = from_json(get(row, "usage_reported")?, "runs.usage_reported")?;
    let budget: Budget = from_json(get(row, "budget")?, "runs.budget")?;
    let spec: RunSpec = from_json(get(row, "spec")?, "runs.spec")?;
    let result: Option<RunResult> = get::<Option<serde_json::Value>>(row, "result")?.map(|v| from_json(v, "runs.result")).transpose()?;
    let error: Option<RunError> = get::<Option<serde_json::Value>>(row, "error")?.map(|v| from_json(v, "runs.error")).transpose()?;
    let runner = match (runner_id, lease_until) {
        (Some(runner_id), Some(until)) => Some(RunnerLease { runner_id, until }),
        _ => None,
    };
    let run = Run::rehydrate(RunData {
        id,
        org_id,
        kind,
        subject,
        harness: get::<Option<String>>(row, "harness")?.map(|h| parse_harness(&h)).transpose()?,
        status: parse_status(&get::<String>(row, "status")?)?,
        attempt,
        budget,
        usage: reported.run,
        parent: get::<Option<Uuid>>(row, "parent_run_id")?.map(RunId::from_uuid),
        runner,
        result,
        error,
        created_at: get(row, "created_at")?,
        started_at: get(row, "started_at")?,
        finished_at: get(row, "finished_at")?,
    });
    let lease = match (runner_id, lease_until, instance) {
        (Some(runner_id), Some(until), Some(instance_id)) => {
            Some(LeaseMeta { runner_id, instance_id, attempt, until, request_id: request_id.unwrap_or_else(Uuid::nil) })
        }
        _ => None,
    };
    let affinity = match (get::<Option<Uuid>>(row, "affinity_runner")?, get::<Option<OffsetDateTime>>(row, "affinity_until")?) {
        (Some(r), Some(until)) => Some(Affinity { runner_id: RunnerId::from_uuid(r), until }),
        _ => None,
    };
    let ingest_cursor = match (get::<Option<i32>>(row, "ingest_attempt")?, get::<Option<i64>>(row, "ingest_local_seq")?) {
        (Some(a), Some(l)) => Some(IngestCursor { attempt: to_unsigned(i64::from(a), "runs.ingest_attempt")?, local_seq: to_unsigned(l, "runs.ingest_local_seq")? }),
        _ => None,
    };
    let next_seq: i64 = get(row, "next_seq")?;
    let next_cmd_seq: i64 = get(row, "next_cmd_seq")?;
    Ok(RunRecord {
        run,
        spec,
        version: to_unsigned(get::<i64>(row, "version")?, "runs.version")?,
        lease,
        session_lost: get(row, "session_lost")?,
        wall: WallClock {
            accumulated: Duration::from_millis(to_unsigned(get::<i64>(row, "wall_accum_ms")?, "runs.wall_accum_ms")?),
            segment_start: get(row, "wall_segment_start")?,
        },
        usage_reported: reported.total,
        reported_frozen: reported.frozen,
        usage_metered: from_json(get(row, "usage_metered")?, "runs.usage_metered")?,
        source_ready: get(row, "source_ready")?,
        affinity,
        last_seq: to_unsigned(next_seq - 1, "runs.next_seq")?,
        last_cmd_seq: to_unsigned(next_cmd_seq - 1, "runs.next_cmd_seq")?,
        ingest_cursor,
    })
}

fn decode_gate(row: &PgRow) -> Result<GateRecord, RunStoreError> {
    let status = match get::<String>(row, "status")?.as_str() {
        "pending" => GateStatus::Pending,
        "passed" => GateStatus::Passed { at: get::<Option<OffsetDateTime>>(row, "decided_at")?.ok_or_else(|| RunStoreError::Invalid("gate without decided_at".into()))? },
        "failed" => GateStatus::Failed {
            reason: get::<Option<String>>(row, "reason")?.unwrap_or_default(),
            at: get::<Option<OffsetDateTime>>(row, "decided_at")?.ok_or_else(|| RunStoreError::Invalid("gate without decided_at".into()))?,
        },
        other => return Err(RunStoreError::Invalid(format!("gates.status: unknown {other:?}"))),
    };
    Ok(GateRecord {
        gate: Gate {
            id: GateId::from_uuid(get(row, "id")?),
            org_id: OrgId::from_uuid(get(row, "org_id")?),
            run_id: RunId::from_uuid(get(row, "run_id")?),
            kind: parse_gate_kind(&get::<String>(row, "kind")?)?,
            attempt: to_unsigned(i64::from(get::<i16>(row, "attempt")?), "gates.attempt")?,
            status,
            verify_run: get::<Option<Uuid>>(row, "verify_run")?.map(RunId::from_uuid),
        },
        feedback: get(row, "feedback")?,
        details: get(row, "details")?,
        created_at: get(row, "created_at")?,
    })
}

fn decode_artifact(row: &PgRow) -> Result<ArtifactRecord, RunStoreError> {
    let state = match get::<String>(row, "state")?.as_str() {
        "pending" => ArtifactState::Pending,
        "complete" => ArtifactState::Complete,
        other => return Err(RunStoreError::Invalid(format!("artifacts.state: unknown {other:?}"))),
    };
    let sha: Option<Vec<u8>> = get(row, "sha256")?;
    Ok(ArtifactRecord {
        artifact: Artifact {
            id: ArtifactId::from_uuid(get(row, "id")?),
            org_id: OrgId::from_uuid(get(row, "org_id")?),
            run_id: RunId::from_uuid(get(row, "run_id")?),
            kind: parse_artifact_kind(&get::<String>(row, "kind")?)?,
            key: get(row, "key")?,
            bytes: to_unsigned(get::<Option<i64>>(row, "bytes")?.unwrap_or(0), "artifacts.bytes")?,
            sha256: match sha {
                Some(b) => fixed::<32>(b, "artifacts.sha256")?,
                None => [0; 32],
            },
            created_at: get(row, "created_at")?,
        },
        attempt: to_unsigned(i64::from(get::<i16>(row, "attempt")?), "artifacts.attempt")?,
        name: get(row, "name")?,
        content_type: get(row, "content_type")?,
        state,
    })
}

/// Columns of a run row written by inserts and updates (counters excluded).
struct RunColumns {
    ticket_id: Option<Uuid>,
    cause_id: Option<Uuid>,
    parent: Option<Uuid>,
    repo_id: Option<Uuid>,
    sha: Option<Vec<u8>>,
    harness: Option<&'static str>,
    status: &'static str,
    attempt: i16,
    spec: serde_json::Value,
    slot: &'static str,
    priority: i16,
    budget: serde_json::Value,
    reported: serde_json::Value,
    metered: serde_json::Value,
    wall_ms: i64,
    segment: Option<OffsetDateTime>,
    runner: Option<Uuid>,
    lease_until: Option<OffsetDateTime>,
    request_id: Option<Uuid>,
    instance: Option<Uuid>,
    affinity_runner: Option<Uuid>,
    affinity_until: Option<OffsetDateTime>,
    result: Option<serde_json::Value>,
    error: Option<serde_json::Value>,
}

impl RunColumns {
    fn of(rec: &RunRecord) -> Result<RunColumns, RunStoreError> {
        let run = &rec.run;
        let (cause_id, parent) = match run.subject {
            RunSubject::Cause(c) => (Some(c.as_uuid()), None),
            RunSubject::Run(p) => (None, Some(p.as_uuid())),
            _ => (None, None),
        };
        let ticket_id = match run.subject {
            RunSubject::Ticket(t) => Some(t),
            _ => rec.spec.ticket_id,
        };
        let (repo_id, sha) = match run.subject {
            RunSubject::RepoVersion(k) => (Some(k.repo_id), Some(k.sha)),
            _ => (rec.spec.repo_id, rec.spec.sha),
        };
        Ok(RunColumns {
            ticket_id: ticket_id.map(|t| t.as_uuid()),
            cause_id,
            parent,
            repo_id: repo_id.map(|r| r.as_uuid()),
            sha: sha.map(|s: CommitSha| s.as_bytes().to_vec()),
            harness: run.harness.map(harness_str),
            status: run.status.name(),
            attempt: i16::try_from(run.attempt).map_err(|_| RunStoreError::Invalid("attempt out of range".into()))?,
            spec: json(&rec.spec, "runs.spec")?,
            slot: slot_str(rec.spec.slot),
            priority: rec.spec.priority,
            budget: json(&run.budget, "runs.budget")?,
            reported: json(&ReportedColumn { total: rec.usage_reported, frozen: rec.reported_frozen, run: run.usage }, "runs.usage_reported")?,
            metered: json(&rec.usage_metered, "runs.usage_metered")?,
            wall_ms: ms(rec.wall.accumulated),
            segment: rec.wall.segment_start,
            runner: rec.lease.map(|l| l.runner_id.as_uuid()),
            lease_until: rec.lease.map(|l| l.until),
            request_id: rec.lease.map(|l| l.request_id),
            instance: rec.lease.map(|l| l.instance_id),
            affinity_runner: rec.affinity.map(|a| a.runner_id.as_uuid()),
            affinity_until: rec.affinity.map(|a| a.until),
            result: run.result.as_ref().map(|r| json(r, "runs.result")).transpose()?,
            error: run.error.as_ref().map(|e| json(e, "runs.error")).transpose()?,
        })
    }
}

#[derive(Default)]
struct Topics {
    events: BTreeSet<(RunId, u64)>,
    commands: BTreeSet<RunId>,
    queue: bool,
    feed: BTreeSet<RunId>,
}

/// Postgres `RunStore`.
pub struct PgRunStore {
    pools: PgPools,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGen>,
    thresholds: DueThresholds,
}

impl PgRunStore {
    pub fn new(pools: PgPools, clock: Arc<dyn Clock>, ids: Arc<dyn IdGen>) -> PgRunStore {
        PgRunStore { pools, clock, ids, thresholds: DueThresholds::default() }
    }

    pub fn with_thresholds(mut self, thresholds: DueThresholds) -> PgRunStore {
        self.thresholds = thresholds;
        self
    }

    async fn exists(conn: &mut PgConnection, org: OrgId, run: RunId) -> Result<bool, RunStoreError> {
        let row = sqlx::query("SELECT 1 FROM runs WHERE org_id = $1 AND id = $2")
            .bind(org.as_uuid())
            .bind(run.as_uuid())
            .fetch_optional(conn)
            .await
            .map_err(db)?;
        Ok(row.is_some())
    }

    async fn insert_run(conn: &mut PgConnection, new: &NewRun) -> Result<RunRecord, RunStoreError> {
        let mut rec = RunRecord::new(new.run.clone(), new.spec.clone(), new.source_ready);
        rec.affinity = new.affinity;
        rec.version = 1;
        let c = RunColumns::of(&rec)?;
        let sql = format!(
            "INSERT INTO runs (org_id, id, kind, ticket_id, cause_id, parent_run_id, repo_id, sha, harness, status, attempt, version, spec, \
             slot_class, priority, source_ready, budget, usage_reported, usage_metered, wall_accum_ms, wall_segment_start, runner_id, \
             lease_until, lease_request_id, runner_instance, session_lost, affinity_runner, affinity_until, result, error, idempotency_key, \
             created_at, started_at, finished_at) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,1,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22,$23,$24,false,$25,$26,$27,$28,$29,$30,$31,$32) \
             ON CONFLICT (org_id, idempotency_key) WHERE idempotency_key IS NOT NULL DO NOTHING RETURNING {RUN_COLUMNS}"
        );
        let row = sqlx::query(&sql)
            .bind(rec.org().as_uuid())
            .bind(rec.id().as_uuid())
            .bind(kind_str(rec.run.kind))
            .bind(c.ticket_id)
            .bind(c.cause_id)
            .bind(c.parent)
            .bind(c.repo_id)
            .bind(c.sha)
            .bind(c.harness)
            .bind(c.status)
            .bind(c.attempt)
            .bind(c.spec)
            .bind(c.slot)
            .bind(c.priority)
            .bind(rec.source_ready)
            .bind(c.budget)
            .bind(c.reported)
            .bind(c.metered)
            .bind(c.wall_ms)
            .bind(c.segment)
            .bind(c.runner)
            .bind(c.lease_until)
            .bind(c.request_id)
            .bind(c.instance)
            .bind(c.affinity_runner)
            .bind(c.affinity_until)
            .bind(c.result)
            .bind(c.error)
            .bind(new.create_key.as_deref())
            .bind(rec.run.created_at)
            .bind(rec.run.started_at)
            .bind(rec.run.finished_at)
            .fetch_optional(&mut *conn)
            .await
            .map_err(|e| match DbError::from(e) {
                DbError::Conflict { constraint } if constraint.as_deref() == Some("runs_pkey") => {
                    RunStoreError::Conflict(RunConflict::Duplicate { run: rec.id() })
                }
                DbError::Invalid(m) if m.starts_with("dangling reference") => RunStoreError::Invalid(m),
                other => other.into(),
            })?;
        match row {
            Some(row) => decode_run(&row),
            None => {
                let existing: Uuid = sqlx::query_scalar("SELECT id FROM runs WHERE org_id = $1 AND idempotency_key = $2")
                    .bind(rec.org().as_uuid())
                    .bind(new.create_key.as_deref())
                    .fetch_one(&mut *conn)
                    .await
                    .map_err(db)?;
                Err(RunStoreError::Conflict(RunConflict::Duplicate { run: RunId::from_uuid(existing) }))
            }
        }
    }

    /// Version-checked full write of `rec`; returns the old `(runner_id, status, attempt,
    /// source_ready)` and the new version.
    async fn update_run(conn: &mut PgConnection, u: &RunUpdate) -> Result<(Option<Uuid>, RunStatus, u16, bool, u64), RunStoreError> {
        let rec = &u.record;
        let c = RunColumns::of(rec)?;
        let row = sqlx::query(
            "WITH old AS (SELECT runner_id, runner_instance, attempt, status, source_ready, lease_until FROM runs \
                          WHERE org_id = $1 AND id = $2 FOR UPDATE) \
             UPDATE runs SET status = $4, attempt = $5, version = runs.version + 1, spec = $6, source_ready = $7, budget = $8, \
               usage_reported = $9, usage_metered = $10, wall_accum_ms = $11, wall_segment_start = $12, runner_id = $13, \
               lease_until = CASE WHEN old.runner_id = $13 AND old.runner_instance = $16 AND old.attempt = $5 AND old.lease_until > $14 \
                                  THEN old.lease_until ELSE $14 END, \
               lease_request_id = $15, runner_instance = $16, session_lost = $17, affinity_runner = $18, affinity_until = $19, \
               result = $20, error = $21, started_at = $22, finished_at = $23, ticket_id = $24, repo_id = $25, sha = $26 \
             FROM old WHERE runs.org_id = $1 AND runs.id = $2 AND runs.version = $3 \
             RETURNING runs.version, old.runner_id AS old_runner, old.status AS old_status, old.attempt AS old_attempt, \
                       old.source_ready AS old_ready",
        )
        .bind(rec.org().as_uuid())
        .bind(rec.id().as_uuid())
        .bind(to_i64(u.expected_version, "runs.version").map_err(RunStoreError::from)?)
        .bind(c.status)
        .bind(c.attempt)
        .bind(c.spec)
        .bind(rec.source_ready)
        .bind(c.budget)
        .bind(c.reported)
        .bind(c.metered)
        .bind(c.wall_ms)
        .bind(c.segment)
        .bind(c.runner)
        .bind(c.lease_until)
        .bind(c.request_id)
        .bind(c.instance)
        .bind(rec.session_lost)
        .bind(c.affinity_runner)
        .bind(c.affinity_until)
        .bind(c.result)
        .bind(c.error)
        .bind(rec.run.started_at)
        .bind(rec.run.finished_at)
        .bind(c.ticket_id)
        .bind(c.repo_id)
        .bind(c.sha)
        .fetch_optional(&mut *conn)
        .await
        .map_err(db)?;
        let Some(row) = row else {
            return Err(if Self::exists(conn, rec.org(), rec.id()).await? {
                RunStoreError::Conflict(RunConflict::StaleVersion { run: rec.id() })
            } else {
                RunStoreError::NotFound
            });
        };
        Ok((
            get(&row, "old_runner")?,
            parse_status(&get::<String>(&row, "old_status")?)?,
            to_unsigned(i64::from(get::<i16>(&row, "old_attempt")?), "runs.attempt")?,
            get(&row, "old_ready")?,
            to_unsigned(get::<i64>(&row, "version")?, "runs.version")?,
        ))
    }

    /// Reserves `n` event seqs of `run`; returns the first.
    async fn take_seqs(conn: &mut PgConnection, org: OrgId, run: RunId, n: usize) -> Result<u64, RunStoreError> {
        let next: i64 = sqlx::query_scalar("UPDATE runs SET next_seq = next_seq + $3 WHERE org_id = $1 AND id = $2 RETURNING next_seq")
            .bind(org.as_uuid())
            .bind(run.as_uuid())
            .bind(i64::try_from(n).unwrap_or(i64::MAX))
            .fetch_optional(&mut *conn)
            .await
            .map_err(db)?
            .ok_or(RunStoreError::NotFound)?;
        Ok(to_unsigned(next - i64::try_from(n).unwrap_or(0), "runs.next_seq")?)
    }

    async fn insert_event(conn: &mut PgConnection, org: OrgId, run: RunId, seq: u64, attempt: u16, at: OffsetDateTime, kind: &RunEventKind) -> Result<(), RunStoreError> {
        sqlx::query("INSERT INTO run_events (org_id, run_id, seq, attempt, at, kind) VALUES ($1, $2, $3, $4, $5, $6)")
            .bind(org.as_uuid())
            .bind(run.as_uuid())
            .bind(to_i64(seq, "run_events.seq").map_err(RunStoreError::from)?)
            .bind(i16::try_from(attempt).unwrap_or(i16::MAX))
            .bind(at)
            .bind(json(kind, "run_events.kind")?)
            .execute(conn)
            .await
            .map_err(db)?;
        Ok(())
    }

    async fn write_gate(conn: &mut PgConnection, org: OrgId, g: &GateWrite) -> Result<(), RunStoreError> {
        let (rec, insert) = match g {
            GateWrite::Insert(r) => (r, true),
            GateWrite::Update(r) => (r, false),
        };
        if rec.gate.org_id != org {
            return Err(RunStoreError::Invalid("gate of another org".into()));
        }
        let (status, reason, decided) = match &rec.gate.status {
            GateStatus::Pending => ("pending", None, None),
            GateStatus::Passed { at } => ("passed", None, Some(*at)),
            GateStatus::Failed { reason, at } => ("failed", Some(reason.clone()), Some(*at)),
        };
        let verify = rec.gate.verify_run.map(|v| v.as_uuid());
        if insert {
            sqlx::query(
                "INSERT INTO gates (org_id, id, run_id, kind, attempt, status, reason, feedback, details, verify_run, created_at, decided_at) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)",
            )
            .bind(org.as_uuid())
            .bind(rec.gate.id.as_uuid())
            .bind(rec.gate.run_id.as_uuid())
            .bind(gate_kind_str(rec.gate.kind))
            .bind(i16::from(rec.gate.attempt))
            .bind(status)
            .bind(reason)
            .bind(rec.feedback.as_deref())
            .bind(rec.details.clone())
            .bind(verify)
            .bind(rec.created_at)
            .bind(decided)
            .execute(conn)
            .await
            .map_err(|e| match DbError::from(e) {
                DbError::Conflict { .. } => RunStoreError::Invalid(format!("gate {:?} attempt {} exists", rec.gate.kind, rec.gate.attempt)),
                other => other.into(),
            })?;
        } else {
            let done = sqlx::query(
                "UPDATE gates SET status = $3, reason = $4, feedback = $5, details = $6, verify_run = $7, decided_at = $8 \
                 WHERE org_id = $1 AND id = $2",
            )
            .bind(org.as_uuid())
            .bind(rec.gate.id.as_uuid())
            .bind(status)
            .bind(reason)
            .bind(rec.feedback.as_deref())
            .bind(rec.details.clone())
            .bind(verify)
            .bind(decided)
            .execute(conn)
            .await
            .map_err(db)?;
            if done.rows_affected() == 0 {
                return Err(RunStoreError::NotFound);
            }
        }
        Ok(())
    }

    async fn commit_tx(&self, org: OrgId, w: RunWrite) -> Result<CommitReceipt, RunStoreError> {
        let now = self.clock.now();
        let mut tx = begin_org(&self.pools.tenant, org).await?;
        let mut receipt = CommitReceipt::default();
        let mut topics = Topics::default();

        if let Some(reply) = &w.reply {
            let body: serde_json::Value =
                serde_json::from_slice(&reply.body).map_err(|e| RunStoreError::Invalid(format!("reply body is not JSON: {e}")))?;
            let inserted = sqlx::query(
                "INSERT INTO idempotency_replies (scope, key, org_id, request_hash, response, created_at) VALUES ($1,$2,$3,$4,$5,$6) \
                 ON CONFLICT (scope, key) DO NOTHING",
            )
            .bind(&reply.scope)
            .bind(&reply.key)
            .bind(org.as_uuid())
            .bind(reply.request_hash.to_vec())
            .bind(body)
            .bind(now)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
            if inserted.rows_affected() == 0 {
                let stored = Self::stored_reply(&mut tx, &reply.scope, &reply.key).await?.ok_or(RunStoreError::NotFound)?;
                return Err(RunStoreError::Conflict(RunConflict::ReplyExists(stored)));
            }
        }

        for new in &w.inserts {
            if new.run.org_id != org {
                return Err(RunStoreError::Invalid("run of another org".into()));
            }
            let rec = Self::insert_run(&mut tx, new).await?;
            receipt.versions.push((rec.id(), 1));
            topics.feed.insert(rec.id());
            if rec.leasable() {
                topics.queue = true;
            }
        }

        for u in &w.updates {
            if u.record.org() != org {
                return Err(RunStoreError::Invalid("run of another org".into()));
            }
            let (old_runner, old_status, old_attempt, old_ready, version) = Self::update_run(&mut tx, u).await?;
            let rec = &u.record;
            receipt.versions.push((rec.id(), version));
            if old_runner.is_some() && rec.lease.is_none() {
                topics.queue = true;
                sqlx::query("UPDATE run_attempts SET ended_at = $4, end_reason = $5 WHERE org_id = $1 AND run_id = $2 AND attempt = $3 AND ended_at IS NULL")
                    .bind(org.as_uuid())
                    .bind(rec.id().as_uuid())
                    .bind(i16::try_from(old_attempt).unwrap_or(i16::MAX))
                    .bind(now)
                    .bind(rec.run.status.name())
                    .execute(&mut *tx)
                    .await
                    .map_err(db)?;
            }
            let was_leasable = old_status == RunStatus::Queued && old_ready;
            if rec.leasable() && (!was_leasable || old_status != RunStatus::Queued) {
                topics.queue = true;
            }
            if old_status != rec.run.status {
                topics.feed.insert(rec.id());
            }
        }

        for g in &w.gates {
            Self::write_gate(&mut tx, org, g).await?;
        }

        if let Some(ingest) = &w.ingest {
            let row = sqlx::query("SELECT ingest_attempt, ingest_local_seq FROM runs WHERE org_id = $1 AND id = $2 FOR UPDATE")
                .bind(org.as_uuid())
                .bind(ingest.run_id.as_uuid())
                .fetch_optional(&mut *tx)
                .await
                .map_err(db)?
                .ok_or(RunStoreError::NotFound)?;
            let cursor = match (get::<Option<i32>>(&row, "ingest_attempt")?, get::<Option<i64>>(&row, "ingest_local_seq")?) {
                (Some(a), Some(l)) => Some(IngestCursor { attempt: to_unsigned(i64::from(a), "ingest_attempt")?, local_seq: to_unsigned(l, "ingest_local_seq")? }),
                _ => None,
            };
            let fresh: Vec<_> = ingest
                .events
                .iter()
                .filter(|(local, _)| cursor.is_none_or(|c| IngestCursor { attempt: ingest.attempt, local_seq: *local } > c))
                .collect();
            let mut acked = cursor.filter(|c| c.attempt == ingest.attempt).map_or(0, |c| c.local_seq);
            let mut newest = cursor;
            if !fresh.is_empty() {
                let first = Self::take_seqs(&mut tx, org, ingest.run_id, fresh.len()).await?;
                for (i, (local, ev)) in fresh.iter().enumerate() {
                    Self::insert_event(&mut tx, org, ingest.run_id, first + i as u64, ev.attempt, ev.at, &ev.kind).await?;
                    acked = acked.max(*local);
                    let key = IngestCursor { attempt: ingest.attempt, local_seq: *local };
                    if newest.is_none_or(|n| key > n) {
                        newest = Some(key);
                    }
                }
                let last = first + fresh.len() as u64 - 1;
                receipt.seqs.push((ingest.run_id, first, last));
                topics.events.insert((ingest.run_id, last));
                if let Some(n) = newest {
                    sqlx::query("UPDATE runs SET ingest_attempt = $3, ingest_local_seq = $4 WHERE org_id = $1 AND id = $2")
                        .bind(org.as_uuid())
                        .bind(ingest.run_id.as_uuid())
                        .bind(i32::from(n.attempt))
                        .bind(to_i64(n.local_seq, "ingest_local_seq").map_err(RunStoreError::from)?)
                        .execute(&mut *tx)
                        .await
                        .map_err(db)?;
                }
            }
            let last: i64 = sqlx::query_scalar("SELECT next_seq - 1 FROM runs WHERE org_id = $1 AND id = $2")
                .bind(org.as_uuid())
                .bind(ingest.run_id.as_uuid())
                .fetch_one(&mut *tx)
                .await
                .map_err(db)?;
            receipt.ingested = Some((acked, to_unsigned(last, "runs.next_seq")?));
        }

        let mut by_run: Vec<(RunId, Vec<&crate::ports::runs::RunEventDraft>)> = Vec::new();
        for ev in &w.events {
            match by_run.iter_mut().find(|(r, _)| *r == ev.run_id) {
                Some((_, list)) => list.push(ev),
                None => by_run.push((ev.run_id, vec![ev])),
            }
        }
        for (run, list) in by_run {
            let first = Self::take_seqs(&mut tx, org, run, list.len()).await?;
            for (i, ev) in list.iter().enumerate() {
                Self::insert_event(&mut tx, org, run, first + i as u64, ev.attempt, ev.at, &ev.kind).await?;
            }
            let last = first + list.len() as u64 - 1;
            receipt.seqs.push((run, first, last));
            topics.events.insert((run, last));
        }

        for cmd in &w.commands {
            let next: i64 = sqlx::query_scalar("UPDATE runs SET next_cmd_seq = next_cmd_seq + 1 WHERE org_id = $1 AND id = $2 RETURNING next_cmd_seq")
                .bind(org.as_uuid())
                .bind(cmd.run_id.as_uuid())
                .fetch_optional(&mut *tx)
                .await
                .map_err(db)?
                .ok_or(RunStoreError::NotFound)?;
            let seq: u64 = to_unsigned(next - 1, "runs.next_cmd_seq")?;
            sqlx::query("INSERT INTO run_commands (org_id, run_id, seq, attempt, issued_at, command) VALUES ($1,$2,$3,$4,$5,$6)")
                .bind(org.as_uuid())
                .bind(cmd.run_id.as_uuid())
                .bind(next - 1)
                .bind(i16::try_from(cmd.attempt).unwrap_or(i16::MAX))
                .bind(now)
                .bind(json(&cmd.command, "run_commands.command")?)
                .execute(&mut *tx)
                .await
                .map_err(db)?;
            receipt.command_seqs.push((cmd.run_id, seq));
            topics.commands.insert(cmd.run_id);
        }

        for (id, bytes) in &w.complete_artifacts {
            let done = sqlx::query(
                "UPDATE artifacts SET state = 'complete', bytes = $3, completed_at = $4 WHERE org_id = $1 AND id = $2 AND sha256 IS NOT NULL",
            )
            .bind(org.as_uuid())
            .bind(id.as_uuid())
            .bind(to_i64(*bytes, "artifacts.bytes").map_err(RunStoreError::from)?)
            .bind(now)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
            if done.rows_affected() == 0 {
                return Err(RunStoreError::NotFound);
            }
        }

        if !w.jobs.is_empty() {
            enqueue_all_in(&mut tx, &w.jobs, now, &*self.ids).await?;
        }
        for run in &topics.feed {
            append_in(&mut tx, org, FeedKind::Run, &run.to_string(), now).await?;
        }
        for (run, seq) in &topics.events {
            notify(&mut tx, CHANNEL_RUN_EVENTS, &events_payload(org, *run, *seq)).await?;
        }
        for run in &topics.commands {
            notify(&mut tx, CHANNEL_RUN_COMMANDS, &commands_payload(org, *run)).await?;
        }
        if topics.queue {
            notify(&mut tx, CHANNEL_QUEUE, "").await?;
        }
        tx.commit().await.map_err(db)?;
        Ok(receipt)
    }

    async fn stored_reply(conn: &mut PgConnection, scope: &str, key: &str) -> Result<Option<StoredReply>, RunStoreError> {
        let row = sqlx::query("SELECT request_hash, response FROM idempotency_replies WHERE scope = $1 AND key = $2")
            .bind(scope)
            .bind(key)
            .fetch_optional(conn)
            .await
            .map_err(db)?;
        row.map(|row| {
            let body: serde_json::Value = get(&row, "response")?;
            Ok(StoredReply {
                request_hash: fixed::<32>(get(&row, "request_hash")?, "idempotency_replies.request_hash")?,
                body: serde_json::to_vec(&body).map_err(|e| RunStoreError::Invalid(e.to_string()))?,
            })
        })
        .transpose()
    }

    async fn fetch_run(conn: &mut PgConnection, org: OrgId, id: RunId) -> Result<RunRecord, RunStoreError> {
        let row = sqlx::query(&format!("SELECT {RUN_COLUMNS} FROM runs WHERE org_id = $1 AND id = $2"))
            .bind(org.as_uuid())
            .bind(id.as_uuid())
            .fetch_optional(conn)
            .await
            .map_err(db)?
            .ok_or(RunStoreError::NotFound)?;
        decode_run(&row)
    }
}

#[async_trait]
impl RunStore for PgRunStore {
    async fn get(&self, org: OrgId, id: RunId) -> Result<RunRecord, RunStoreError> {
        let mut tx = begin_org(&self.pools.tenant, org).await?;
        let rec = Self::fetch_run(&mut tx, org, id).await?;
        tx.commit().await.map_err(db)?;
        Ok(rec)
    }

    async fn list(&self, org: OrgId, q: &RunQuery) -> Result<Vec<RunRecord>, RunStoreError> {
        let mut tx = begin_org(&self.pools.tenant, org).await?;
        let mut b: QueryBuilder<Postgres> = QueryBuilder::new(format!("SELECT {RUN_COLUMNS} FROM runs WHERE org_id = "));
        b.push_bind(org.as_uuid());
        if let Some(kind) = q.kind {
            b.push(" AND kind = ").push_bind(kind_str(kind));
        }
        match q.subject {
            Some(RunSubject::RepoVersion(k)) => {
                b.push(" AND kind = 'learn' AND repo_id = ").push_bind(k.repo_id.as_uuid()).push(" AND sha = ").push_bind(k.sha.as_bytes().to_vec());
            }
            Some(RunSubject::Ticket(t)) => {
                b.push(" AND kind = 'triage' AND ticket_id = ").push_bind(t.as_uuid());
            }
            Some(RunSubject::Cause(c)) => {
                b.push(" AND cause_id = ").push_bind(c.as_uuid());
            }
            Some(RunSubject::Run(p)) => {
                b.push(" AND parent_run_id = ").push_bind(p.as_uuid());
            }
            None => {}
        }
        if let Some(p) = q.parent {
            b.push(" AND parent_run_id = ").push_bind(p.as_uuid());
        }
        if let Some(t) = q.ticket {
            b.push(" AND ticket_id = ").push_bind(t.as_uuid());
        }
        if let Some(r) = q.repo {
            b.push(" AND repo_id = ").push_bind(r.as_uuid());
        }
        if let Some(s) = q.sha {
            b.push(" AND sha = ").push_bind(s.as_bytes().to_vec());
        }
        if !q.statuses.is_empty() {
            let names: Vec<&'static str> = q.statuses.iter().map(|s| s.name()).collect();
            b.push(" AND status = ANY(").push_bind(names).push(")");
        }
        let (cmp, order) = if q.newest_first { ("<", "DESC") } else { (">", "ASC") };
        if let Some(cursor) = q.cursor {
            let at: Option<OffsetDateTime> = sqlx::query_scalar("SELECT created_at FROM runs WHERE org_id = $1 AND id = $2")
                .bind(org.as_uuid())
                .bind(cursor.as_uuid())
                .fetch_optional(&mut *tx)
                .await
                .map_err(db)?;
            let at = at.ok_or_else(|| RunStoreError::Invalid("unknown cursor".into()))?;
            b.push(format!(" AND (created_at, id) {cmp} (")).push_bind(at).push(", ").push_bind(cursor.as_uuid()).push(")");
        }
        b.push(format!(" ORDER BY created_at {order}, id {order} LIMIT ")).push_bind(i64::from(q.effective_limit()));
        let rows = b.build().fetch_all(&mut *tx).await.map_err(db)?;
        tx.commit().await.map_err(db)?;
        rows.iter().map(decode_run).collect()
    }

    async fn commit(&self, org: OrgId, w: RunWrite) -> Result<CommitReceipt, RunStoreError> {
        let mut attempts = 0;
        loop {
            attempts += 1;
            match self.commit_tx(org, w.clone()).await {
                Err(RunStoreError::Unavailable(m)) if attempts < COMMIT_ATTEMPTS && m.starts_with(RETRY_MARK) => continue,
                other => return other,
            }
        }
    }

    async fn locate(&self, id: RunId) -> Result<OrgId, RunStoreError> {
        let org: Option<Uuid> = sqlx::query_scalar("SELECT org_id FROM runs WHERE id = $1 LIMIT 1")
            .bind(id.as_uuid())
            .fetch_optional(&self.pools.platform)
            .await
            .map_err(db)?;
        org.map(OrgId::from_uuid).ok_or(RunStoreError::NotFound)
    }

    async fn lease_next(&self, q: &LeaseQuery) -> Result<Option<RunRecord>, RunStoreError> {
        let classes: Vec<&'static str> = [
            (mage_core::wire::runner::SlotClass::Light, "light"),
            (mage_core::wire::runner::SlotClass::Work, "work"),
            (mage_core::wire::runner::SlotClass::Verify, "verify"),
        ]
        .into_iter()
        .filter(|(c, _)| q.free.get(*c) > 0)
        .map(|(_, s)| s)
        .collect();
        if classes.is_empty() {
            return Ok(None);
        }
        let (shared, orgs) = match &q.scope {
            crate::ports::tokens::RunnerScope::Shared => (true, Vec::new()),
            crate::ports::tokens::RunnerScope::Orgs(o) => (false, o.iter().map(|o| o.as_uuid()).collect::<Vec<_>>()),
        };
        let mut tx = begin_platform(&self.pools.platform).await?;
        let rows = sqlx::query(&format!(
            "SELECT {RUN_COLUMNS} FROM runs \
             WHERE status = 'queued' AND source_ready AND slot_class = ANY($1) AND ($2 OR org_id = ANY($3)) \
               AND (affinity_runner IS NULL OR affinity_runner = $4 OR affinity_until <= $5) \
             ORDER BY (affinity_runner IS NOT DISTINCT FROM $4 AND affinity_until > $5) DESC, priority, created_at, id \
             LIMIT $6 FOR UPDATE SKIP LOCKED"
        ))
        .bind(classes)
        .bind(shared)
        .bind(orgs)
        .bind(q.runner_id.as_uuid())
        .bind(q.now)
        .bind(LEASE_CANDIDATES)
        .fetch_all(&mut *tx)
        .await
        .map_err(db)?;
        let mut candidates: Vec<RunRecord> = rows.iter().map(decode_run).collect::<Result<_, _>>()?;
        candidates.retain(|r| q.admits(r));
        candidates.sort_by_key(|r| q.order_key(r));
        let mut slot_conflicts = 0;
        for mut rec in candidates {
            if let Some((org, repo)) = rec.work_slot() {
                let held: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM runs WHERE org_id = $1 AND repo_id = $2 AND runner_id IS NOT NULL AND slot_class = 'work'",
                )
                .bind(org.as_uuid())
                .bind(repo.as_uuid())
                .fetch_one(&mut *tx)
                .await
                .map_err(db)?;
                if held >= i64::from(q.repo_limit) {
                    continue;
                }
            }
            let draft = rec.apply_lease(q).map_err(|e| RunStoreError::Invalid(e.to_string()))?;
            let mut sp = sqlx::Acquire::begin(&mut tx).await.map_err(db)?;
            let update = RunUpdate::of(rec.clone());
            match Self::update_run(&mut sp, &update).await {
                Ok((_, _, _, _, version)) => {
                    rec.version = version;
                    let org = rec.org();
                    let seq = Self::take_seqs(&mut sp, org, rec.id(), 1).await?;
                    Self::insert_event(&mut sp, org, rec.id(), seq, draft.attempt, draft.at, &draft.kind).await?;
                    sqlx::query("INSERT INTO run_attempts (org_id, run_id, attempt, runner_id, leased_at) VALUES ($1,$2,$3,$4,$5)")
                        .bind(org.as_uuid())
                        .bind(rec.id().as_uuid())
                        .bind(i16::try_from(rec.run.attempt).unwrap_or(i16::MAX))
                        .bind(q.runner_id.as_uuid())
                        .bind(q.now)
                        .execute(&mut *sp)
                        .await
                        .map_err(db)?;
                    sp.commit().await.map_err(db)?;
                    append_in(&mut tx, org, FeedKind::Run, &rec.id().to_string(), q.now).await?;
                    notify(&mut tx, CHANNEL_RUN_EVENTS, &events_payload(org, rec.id(), seq)).await?;
                    tx.commit().await.map_err(db)?;
                    rec.last_seq = seq;
                    return Ok(Some(rec));
                }
                Err(RunStoreError::Invalid(m)) if m.contains("runs_repo_work_slot") => {
                    sp.rollback().await.map_err(db)?;
                    slot_conflicts += 1;
                    if slot_conflicts >= SLOT_RETRIES {
                        break;
                    }
                }
                Err(e) => return Err(e),
            }
        }
        tx.rollback().await.map_err(db)?;
        Ok(None)
    }

    async fn lease_by_request(&self, runner: RunnerId, request_id: Uuid) -> Result<Option<RunRecord>, RunStoreError> {
        let row = sqlx::query(&format!(
            "SELECT {RUN_COLUMNS} FROM runs WHERE runner_id = $1 AND lease_request_id = $2 AND status = 'leased' LIMIT 1"
        ))
        .bind(runner.as_uuid())
        .bind(request_id)
        .fetch_optional(&self.pools.platform)
        .await
        .map_err(db)?;
        row.as_ref().map(decode_run).transpose()
    }

    async fn extend_lease(&self, org: OrgId, id: RunId, fence: LeaseFence, until: OffsetDateTime) -> Result<RunRecord, RunStoreError> {
        let mut tx = begin_org(&self.pools.tenant, org).await?;
        let row = sqlx::query(&format!(
            "UPDATE runs SET lease_until = $6 WHERE org_id = $1 AND id = $2 AND runner_id = $3 AND runner_instance = $4 AND attempt = $5 \
             RETURNING {RUN_COLUMNS}"
        ))
        .bind(org.as_uuid())
        .bind(id.as_uuid())
        .bind(fence.runner_id.as_uuid())
        .bind(fence.instance_id)
        .bind(i16::try_from(fence.attempt).unwrap_or(i16::MAX))
        .bind(until)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db)?;
        let rec = match row {
            Some(row) => decode_run(&row)?,
            None if Self::exists(&mut tx, org, id).await? => return Err(RunStoreError::Conflict(RunConflict::LeaseLost)),
            None => return Err(RunStoreError::NotFound),
        };
        tx.commit().await.map_err(db)?;
        Ok(rec)
    }

    async fn due(&self, now: OffsetDateTime, limit: u32) -> Result<Vec<DueItem>, RunStoreError> {
        let gate_before = now - crate::app::runs::to_time(self.thresholds.gate_wait_max);
        let queue_before = now - crate::app::runs::to_time(self.thresholds.queue_max_wait);
        let rows = sqlx::query(
            "SELECT org_id, id, reason FROM ( \
               SELECT r.org_id, r.id, CASE \
                 WHEN r.status IN ('preparing','running') AND \
                      r.wall_accum_ms + COALESCE(EXTRACT(EPOCH FROM ($1 - r.wall_segment_start)) * 1000, 0) > (r.budget->>'max_wall')::numeric THEN 'wall' \
                 WHEN r.runner_id IS NOT NULL AND r.lease_until < $1 THEN 'lease' \
                 WHEN r.status = 'awaiting_gate' AND EXISTS (SELECT 1 FROM gates g WHERE g.org_id = r.org_id AND g.run_id = r.id \
                      AND g.status = 'pending' AND g.created_at < $2) THEN 'gate' \
                 WHEN r.status = 'queued' AND COALESCE((SELECT max(a.ended_at) FROM run_attempts a WHERE a.org_id = r.org_id AND a.run_id = r.id), \
                      r.created_at) < $3 THEN 'queue' \
               END AS reason \
               FROM runs r \
               WHERE r.status IN ('queued','leased','preparing','running','awaiting_gate') OR r.runner_id IS NOT NULL \
             ) d WHERE reason IS NOT NULL LIMIT $4",
        )
        .bind(now)
        .bind(gate_before)
        .bind(queue_before)
        .bind(i64::from(limit))
        .fetch_all(&self.pools.platform)
        .await
        .map_err(db)?;
        rows.iter()
            .map(|row| {
                let reason = match get::<String>(row, "reason")?.as_str() {
                    "wall" => DueReason::WallExceeded,
                    "lease" => DueReason::LeaseExpired,
                    "gate" => DueReason::GateWait,
                    _ => DueReason::QueueWait,
                };
                Ok(DueItem { org_id: OrgId::from_uuid(get(row, "org_id")?), run_id: RunId::from_uuid(get(row, "id")?), reason })
            })
            .collect()
    }

    async fn held_by(&self, runner: RunnerId) -> Result<Vec<(OrgId, RunId)>, RunStoreError> {
        let rows = sqlx::query("SELECT org_id, id FROM runs WHERE runner_id = $1 ORDER BY created_at")
            .bind(runner.as_uuid())
            .fetch_all(&self.pools.platform)
            .await
            .map_err(db)?;
        rows.iter().map(|r| Ok((OrgId::from_uuid(get(r, "org_id")?), RunId::from_uuid(get(r, "id")?)))).collect()
    }

    async fn mark_source_ready(&self, org: OrgId, repo: RepoId, sha: CommitSha) -> Result<u32, RunStoreError> {
        let now = self.clock.now();
        let mut tx = begin_org(&self.pools.tenant, org).await?;
        let ids: Vec<Uuid> = sqlx::query_scalar(
            "UPDATE runs SET source_ready = true, version = version + 1 \
             WHERE org_id = $1 AND repo_id = $2 AND sha = $3 AND status = 'queued' AND NOT source_ready RETURNING id",
        )
        .bind(org.as_uuid())
        .bind(repo.as_uuid())
        .bind(sha.as_bytes().to_vec())
        .fetch_all(&mut *tx)
        .await
        .map_err(db)?;
        if !ids.is_empty() {
            for id in &ids {
                append_in(&mut tx, org, FeedKind::Run, &RunId::from_uuid(*id).to_string(), now).await?;
            }
            notify(&mut tx, CHANNEL_QUEUE, "").await?;
        }
        tx.commit().await.map_err(db)?;
        Ok(u32::try_from(ids.len()).unwrap_or(u32::MAX))
    }

    async fn record_runner_session(&self, s: &RunnerSession) -> Result<Option<Uuid>, RunStoreError> {
        let mut tx = begin_platform(&self.pools.platform).await?;
        let previous: Option<Option<Uuid>> = sqlx::query_scalar("SELECT instance_id FROM runners WHERE id = $1 FOR UPDATE")
            .bind(s.runner_id.as_uuid())
            .fetch_optional(&mut *tx)
            .await
            .map_err(db)?;
        let Some(previous) = previous else { return Err(RunStoreError::NotFound) };
        sqlx::query("UPDATE runners SET instance_id = $2, capabilities = $3, version = $4, last_seen_at = $5 WHERE id = $1")
            .bind(s.runner_id.as_uuid())
            .bind(s.instance_id)
            .bind(json(&s.capabilities, "runners.capabilities")?)
            .bind(&s.version)
            .bind(s.at)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        tx.commit().await.map_err(db)?;
        Ok(previous)
    }

    async fn runner_session(&self, runner: RunnerId) -> Result<Option<RunnerSession>, RunStoreError> {
        let row = sqlx::query("SELECT instance_id, capabilities, version, last_seen_at FROM runners WHERE id = $1")
            .bind(runner.as_uuid())
            .fetch_optional(&self.pools.platform)
            .await
            .map_err(db)?;
        let Some(row) = row else { return Ok(None) };
        let (Some(instance_id), Some(version), Some(at)) =
            (get::<Option<Uuid>>(&row, "instance_id")?, get::<Option<String>>(&row, "version")?, get::<Option<OffsetDateTime>>(&row, "last_seen_at")?)
        else {
            return Ok(None);
        };
        Ok(Some(RunnerSession { runner_id: runner, instance_id, version, capabilities: from_json(get(&row, "capabilities")?, "runners.capabilities")?, at }))
    }

    async fn events_after(&self, org: OrgId, run: RunId, after: u64, limit: u32) -> Result<Vec<RunEvent>, RunStoreError> {
        let mut tx = begin_org(&self.pools.tenant, org).await?;
        if !Self::exists(&mut tx, org, run).await? {
            return Err(RunStoreError::NotFound);
        }
        let rows = sqlx::query("SELECT seq, at, kind FROM run_events WHERE org_id = $1 AND run_id = $2 AND seq > $3 ORDER BY seq LIMIT $4")
            .bind(org.as_uuid())
            .bind(run.as_uuid())
            .bind(to_i64(after, "after").map_err(RunStoreError::from)?)
            .bind(i64::from(limit))
            .fetch_all(&mut *tx)
            .await
            .map_err(db)?;
        tx.commit().await.map_err(db)?;
        rows.iter()
            .map(|r| {
                Ok(RunEvent {
                    run_id: run,
                    seq: to_unsigned(get::<i64>(r, "seq")?, "run_events.seq")?,
                    at: get(r, "at")?,
                    kind: from_json(get(r, "kind")?, "run_events.kind")?,
                })
            })
            .collect()
    }

    async fn commands_after(&self, org: OrgId, run: RunId, attempt: u16, after: u64, limit: u32) -> Result<Vec<CommandRecord>, RunStoreError> {
        let mut tx = begin_org(&self.pools.tenant, org).await?;
        if !Self::exists(&mut tx, org, run).await? {
            return Err(RunStoreError::NotFound);
        }
        let rows = sqlx::query(
            "SELECT seq, attempt, issued_at, command FROM run_commands WHERE org_id = $1 AND run_id = $2 AND attempt = $3 AND seq > $4 \
             ORDER BY seq LIMIT $5",
        )
        .bind(org.as_uuid())
        .bind(run.as_uuid())
        .bind(i16::try_from(attempt).unwrap_or(i16::MAX))
        .bind(to_i64(after, "after").map_err(RunStoreError::from)?)
        .bind(i64::from(limit))
        .fetch_all(&mut *tx)
        .await
        .map_err(db)?;
        tx.commit().await.map_err(db)?;
        rows.iter()
            .map(|r| {
                Ok(CommandRecord {
                    seq: to_unsigned(get::<i64>(r, "seq")?, "run_commands.seq")?,
                    attempt: to_unsigned(i64::from(get::<i16>(r, "attempt")?), "run_commands.attempt")?,
                    issued_at: get(r, "issued_at")?,
                    command: from_json(get(r, "command")?, "run_commands.command")?,
                })
            })
            .collect()
    }

    async fn gates(&self, org: OrgId, run: RunId) -> Result<Vec<GateRecord>, RunStoreError> {
        let mut tx = begin_org(&self.pools.tenant, org).await?;
        if !Self::exists(&mut tx, org, run).await? {
            return Err(RunStoreError::NotFound);
        }
        let rows = sqlx::query(&format!("SELECT {GATE_COLUMNS} FROM gates WHERE org_id = $1 AND run_id = $2 ORDER BY created_at, kind, attempt"))
            .bind(org.as_uuid())
            .bind(run.as_uuid())
            .fetch_all(&mut *tx)
            .await
            .map_err(db)?;
        tx.commit().await.map_err(db)?;
        rows.iter().map(decode_gate).collect()
    }

    async fn gate_for_verify(&self, org: OrgId, verify_run: RunId) -> Result<Option<GateRecord>, RunStoreError> {
        let mut tx = begin_org(&self.pools.tenant, org).await?;
        let row = sqlx::query(&format!("SELECT {GATE_COLUMNS} FROM gates WHERE org_id = $1 AND verify_run = $2 LIMIT 1"))
            .bind(org.as_uuid())
            .bind(verify_run.as_uuid())
            .fetch_optional(&mut *tx)
            .await
            .map_err(db)?;
        tx.commit().await.map_err(db)?;
        row.as_ref().map(decode_gate).transpose()
    }

    async fn reserve_artifact(&self, org: OrgId, a: NewArtifact) -> Result<ArtifactRecord, RunStoreError> {
        let key = BlobKey::run_artifact(org, a.run_id, a.kind, &a.name).map_err(|e| RunStoreError::Invalid(e.to_string()))?;
        let now = self.clock.now();
        let id: ArtifactId = self.ids.new_id();
        let mut tx = begin_org(&self.pools.tenant, org).await?;
        if !Self::exists(&mut tx, org, a.run_id).await? {
            return Err(RunStoreError::NotFound);
        }
        let attempt = i16::try_from(a.attempt).unwrap_or(i16::MAX);
        let inserted = sqlx::query(&format!(
            "INSERT INTO artifacts (org_id, id, run_id, attempt, kind, name, key, content_type, bytes, sha256, state, created_at) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,'pending',$11) \
             ON CONFLICT (org_id, run_id, attempt, kind, name) DO NOTHING RETURNING {ARTIFACT_COLUMNS}"
        ))
        .bind(org.as_uuid())
        .bind(id.as_uuid())
        .bind(a.run_id.as_uuid())
        .bind(attempt)
        .bind(a.kind.as_str())
        .bind(&a.name)
        .bind(key.as_str())
        .bind(&a.content_type)
        .bind(to_i64(a.bytes, "artifacts.bytes").map_err(RunStoreError::from)?)
        .bind(a.sha256.to_vec())
        .bind(now)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| match DbError::from(e) {
            DbError::Conflict { .. } => RunStoreError::Invalid(format!("artifact key {key} is taken by another attempt")),
            other => other.into(),
        })?;
        let row = match inserted {
            Some(row) => row,
            None => sqlx::query(&format!(
                "SELECT {ARTIFACT_COLUMNS} FROM artifacts WHERE org_id = $1 AND run_id = $2 AND attempt = $3 AND kind = $4 AND name = $5"
            ))
            .bind(org.as_uuid())
            .bind(a.run_id.as_uuid())
            .bind(attempt)
            .bind(a.kind.as_str())
            .bind(&a.name)
            .fetch_one(&mut *tx)
            .await
            .map_err(db)?,
        };
        let rec = decode_artifact(&row)?;
        tx.commit().await.map_err(db)?;
        Ok(rec)
    }

    async fn artifact(&self, org: OrgId, id: ArtifactId) -> Result<ArtifactRecord, RunStoreError> {
        let mut tx = begin_org(&self.pools.tenant, org).await?;
        let row = sqlx::query(&format!("SELECT {ARTIFACT_COLUMNS} FROM artifacts WHERE org_id = $1 AND id = $2"))
            .bind(org.as_uuid())
            .bind(id.as_uuid())
            .fetch_optional(&mut *tx)
            .await
            .map_err(db)?
            .ok_or(RunStoreError::NotFound)?;
        tx.commit().await.map_err(db)?;
        decode_artifact(&row)
    }

    async fn artifacts(&self, org: OrgId, run: RunId) -> Result<Vec<ArtifactRecord>, RunStoreError> {
        let mut tx = begin_org(&self.pools.tenant, org).await?;
        if !Self::exists(&mut tx, org, run).await? {
            return Err(RunStoreError::NotFound);
        }
        let rows = sqlx::query(&format!("SELECT {ARTIFACT_COLUMNS} FROM artifacts WHERE org_id = $1 AND run_id = $2 ORDER BY created_at, id"))
            .bind(org.as_uuid())
            .bind(run.as_uuid())
            .fetch_all(&mut *tx)
            .await
            .map_err(db)?;
        tx.commit().await.map_err(db)?;
        rows.iter().map(decode_artifact).collect()
    }

    async fn reply(&self, scope: &str, key: &str) -> Result<Option<StoredReply>, RunStoreError> {
        let mut conn = self.pools.platform.acquire().await.map_err(db)?;
        Self::stored_reply(&mut conn, scope, key).await
    }

    async fn usage(&self, org: OrgId, from: OffsetDateTime, to: OffsetDateTime) -> Result<Vec<RunUsage>, RunStoreError> {
        let mut tx = begin_org(&self.pools.tenant, org).await?;
        let rows = sqlx::query(&format!(
            "SELECT {RUN_COLUMNS} FROM runs WHERE org_id = $1 AND created_at >= $2 AND created_at < $3 ORDER BY created_at, id"
        ))
        .bind(org.as_uuid())
        .bind(from)
        .bind(to)
        .fetch_all(&mut *tx)
        .await
        .map_err(db)?;
        tx.commit().await.map_err(db)?;
        rows.iter().map(|r| decode_run(r).map(|rec| RunUsage::of(&rec))).collect()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::Arc;

    use async_trait::async_trait;
    use mage_core::domain::RunnerId;
    use mage_core::testing::SeqIdGen;
    use sqlx::PgPool;
    use tokio_util::sync::CancellationToken;

    use super::PgRunStore;
    use crate::adapters::driven::pg::signals::PgRunSignals;
    use crate::testing::pg::with_pg;
    use crate::testing::runs_contract::{RunStoreFixture, RunnerSeed, contract_run_store};

    struct PgRunnerSeed(PgPool);

    #[async_trait]
    impl RunnerSeed for PgRunnerSeed {
        async fn runner(&self, runner: RunnerId) {
            sqlx::query(
                "INSERT INTO runners (id, name, scope, status, public_key, generation, registered_at) \
                 VALUES ($1, $2, '{\"type\":\"shared\"}', 'active', $3, 1, now()) ON CONFLICT DO NOTHING",
            )
            .bind(runner.as_uuid())
            .bind(format!("runner-{}", runner.as_uuid().simple()))
            .bind(vec![7u8; 32])
            .execute(&self.0)
            .await
            .expect("seed runner");
        }
    }

    /// `DATABASE_URL=postgres://… cargo test -p mage-server --features testing pg_run_store_contract`
    #[tokio::test]
    async fn pg_run_store_contract() {
        with_pg("pg_run_store_contract", |env| async move {
            let shutdown = CancellationToken::new();
            let previous: Arc<parking_lot::Mutex<Option<CancellationToken>>> = Arc::default();
            contract_run_store(|clock| {
                let env = env.clone();
                let shutdown = shutdown.clone();
                let previous = previous.clone();
                async move {
                    // The previous scenario's database is dropped by `fresh`: stop its listener.
                    if let Some(old) = previous.lock().take() {
                        old.cancel();
                    }
                    let db = env.fresh().await;
                    let token = shutdown.child_token();
                    *previous.lock() = Some(token.clone());
                    let store = PgRunStore::new(db.pools.clone(), clock.clone(), Arc::new(SeqIdGen::new()));
                    let signals = PgRunSignals::start(db.admin.clone(), token);
                    RunStoreFixture {
                        store: Arc::new(store),
                        signals: Arc::new(signals),
                        seed: db.seed(clock),
                        runners: Arc::new(PgRunnerSeed(db.admin.clone())),
                    }
                }
            })
            .await;
            shutdown.cancel();
        })
        .await;
    }
}
