//! `RunStore` and `RunSignals` (runs.md "Ports", "Record types", "Store invariants"), plus the
//! pure record helpers both store implementations share (lease matching, fencing, wall clock).
//!
//! Store semantics every implementation follows (contract suite in `crate::testing::runs`):
//!
//! - `commit` is one transaction; it fails as a whole on `StaleVersion`, `Duplicate`,
//!   `ReplyExists` or `SubjectBusy`. Updates replace every column of the record except the
//!   store-managed counters (`version`, `last_seq`, `last_cmd_seq`, `ingest_cursor`); a lease that
//!   names the same `(runner, instance, attempt)` as the stored one keeps the later `until`, so a
//!   commit never undoes a concurrent `extend_lease`.
//! - Event and command `seq`s are strictly increasing per run with no gaps inside one commit;
//!   ingested runner events get theirs before the commit's server events.
//! - `lease_next` hands one `Queued` run to at most one caller and applies `Lease` atomically
//!   (status, `runner`, `LeaseMeta`, a `run_attempts` row, a `Status(Leased)` event).
//! - Repo slot: at most `repo_limit` `SlotClass::Work` runs with a held lease per `(org, repo)`.
//! - Methods not marked cross-tenant filter by `org`; another org's run is `NotFound`.
//! - Signals: `RunEvents(run)` after events were appended, `RunCommands(run)` after commands,
//!   `Queue` when a run became leasable (inserted or requeued `Queued` with source ready,
//!   `mark_source_ready`) or a lease was released.

use std::time::Duration;

use async_trait::async_trait;
use time::OffsetDateTime;
use uuid::Uuid;

use mage_core::domain::{
    Artifact, ArtifactId, ArtifactKind, CommitSha, Gate, OrgId, RepoId, Run, RunCommand, RunEvent, RunEventKind, RunId, RunKind,
    RunStatus, RunStatusEvent, RunSubject, RunnerId, RunnerLease, TicketId, Usage,
};
use mage_core::ports::jobs::NewJob;
use mage_core::wire::runner::{Capabilities, PreparedBaseKey, SlotClass, SlotCounts};

pub use mage_core::wire::runner::ArtifactState;

use crate::domain::runs::RunSpec;
use crate::error::AppError;
use crate::ports::tokens::RunnerScope;

// ---------------------------------------------------------------------------------------------
// Records
// ---------------------------------------------------------------------------------------------

/// The lease of the current attempt (mirrors `run.runner` plus fencing details).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaseMeta {
    pub runner_id: RunnerId,
    pub instance_id: Uuid,
    pub attempt: u16,
    pub until: OffsetDateTime,
    /// `LeaseRequest.request_id` that obtained the lease (lost-response replay).
    pub request_id: Uuid,
}

impl LeaseMeta {
    pub fn fence(&self) -> LeaseFence {
        LeaseFence { runner_id: self.runner_id, instance_id: self.instance_id, attempt: self.attempt }
    }
}

/// What every per-run runner call must match: token `sub`, token `inst`, request `attempt`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LeaseFence {
    pub runner_id: RunnerId,
    pub instance_id: Uuid,
    pub attempt: u16,
}

/// Wall time spent in `Preparing | Running`: closed segments plus the open one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WallClock {
    pub accumulated: Duration,
    pub segment_start: Option<OffsetDateTime>,
}

impl WallClock {
    pub fn at(&self, now: OffsetDateTime) -> Duration {
        let open = self.segment_start.map_or(Duration::ZERO, |start| positive(now - start));
        self.accumulated.saturating_add(open)
    }

    /// Starts a segment unless one is open.
    pub fn open(&mut self, now: OffsetDateTime) {
        if self.segment_start.is_none() {
            self.segment_start = Some(now);
        }
    }

    /// Folds the open segment into `accumulated`.
    pub fn close(&mut self, now: OffsetDateTime) {
        if let Some(start) = self.segment_start.take() {
            self.accumulated = self.accumulated.saturating_add(positive(now - start));
        }
    }
}

fn positive(d: time::Duration) -> Duration {
    Duration::try_from(d).unwrap_or(Duration::ZERO)
}

/// Verify runs prefer the parent's runner until `until`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Affinity {
    pub runner_id: RunnerId,
    pub until: OffsetDateTime,
}

/// Runner event dedupe: `(attempt, local_seq)` already ingested.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct IngestCursor {
    pub attempt: u16,
    pub local_seq: u64,
}

/// One run as stored.
#[derive(Debug, Clone, PartialEq)]
pub struct RunRecord {
    pub run: Run,
    pub spec: RunSpec,
    /// Optimistic concurrency; bumped by every `commit` update, `lease_next` and
    /// `mark_source_ready`, never by `extend_lease`.
    pub version: u64,
    pub lease: Option<LeaseMeta>,
    /// The lease was dropped while `AwaitingGate`; the gate continues.
    pub session_lost: bool,
    pub wall: WallClock,
    /// Harness-reported usage: frozen totals of earlier attempts plus the per-field max of the
    /// current attempt's cumulative reports (advisory in Gateway mode).
    pub usage_reported: Usage,
    /// `usage_reported` frozen at the last requeue (the earlier attempts' share).
    pub reported_frozen: Usage,
    /// Model gateway metering (authoritative in Gateway mode).
    pub usage_metered: Usage,
    pub source_ready: bool,
    pub affinity: Option<Affinity>,
    /// Last event `seq` (0 = none).
    pub last_seq: u64,
    /// Last command `seq` (0 = none).
    pub last_cmd_seq: u64,
    pub ingest_cursor: Option<IngestCursor>,
}

impl RunRecord {
    /// A record for a run about to be inserted.
    pub fn new(run: Run, spec: RunSpec, source_ready: bool) -> RunRecord {
        RunRecord {
            run,
            spec,
            version: 0,
            lease: None,
            session_lost: false,
            wall: WallClock::default(),
            usage_reported: Usage::default(),
            reported_frozen: Usage::default(),
            usage_metered: Usage::default(),
            source_ready,
            affinity: None,
            last_seq: 0,
            last_cmd_seq: 0,
            ingest_cursor: None,
        }
    }

    pub fn id(&self) -> RunId {
        self.run.id
    }

    pub fn org(&self) -> OrgId {
        self.run.org_id
    }

    pub fn wall_at(&self, now: OffsetDateTime) -> Duration {
        self.wall.at(now)
    }

    /// `usage_metered` when the assignment had a gateway, else `usage_reported`; `wall` is always
    /// the server-side wall clock.
    pub fn effective_usage(&self, now: OffsetDateTime) -> Usage {
        let base = if self.spec.gateway_metered() { self.usage_metered } else { self.usage_reported };
        Usage { wall: self.wall_at(now), ..base }
    }

    /// The fence matches the held lease and the run's current attempt.
    pub fn holds(&self, fence: &LeaseFence) -> bool {
        self.run.attempt == fence.attempt && self.lease.is_some_and(|l| l.fence() == *fence)
    }

    /// Applies `Lease` for `q` (shared by every `lease_next` implementation): status, `runner`,
    /// `LeaseMeta`, affinity cleared. Returns the `Status(Leased)` event to append.
    pub fn apply_lease(&mut self, q: &LeaseQuery) -> Result<RunEventDraft, mage_core::domain::DomainError> {
        let runner = RunnerLease { runner_id: q.runner_id, until: q.until };
        self.run.apply(RunStatusEvent::Lease { runner }, q.now)?;
        self.lease = Some(LeaseMeta {
            runner_id: q.runner_id,
            instance_id: q.instance_id,
            attempt: self.run.attempt,
            until: q.until,
            request_id: q.request_id,
        });
        self.affinity = None;
        Ok(RunEventDraft { run_id: self.run.id, attempt: self.run.attempt, at: q.now, kind: RunEventKind::Status(RunStatus::Leased) })
    }

    /// `(repo, sha, recipe_hash)` of the base the run needs (warm-base ordering).
    pub fn base_key(&self) -> Option<PreparedBaseKey> {
        Some(PreparedBaseKey { repo_id: self.spec.repo_id?, sha: self.spec.sha?, recipe_hash: self.spec.recipe_hash })
    }

    /// The repo slot this run takes while leased (Work class only).
    pub fn work_slot(&self) -> Option<(OrgId, RepoId)> {
        (self.spec.slot == SlotClass::Work).then_some(()).and(self.spec.repo_id).map(|repo| (self.run.org_id, repo))
    }

    /// Queued with its source ready: `lease_next` may hand it out.
    pub fn leasable(&self) -> bool {
        self.run.status == RunStatus::Queued && self.source_ready
    }
}

/// One gate attempt with its rendered feedback and module-owned details.
#[derive(Debug, Clone, PartialEq)]
pub struct GateRecord {
    pub gate: Gate,
    /// Rendered `GateFeedback` details; reused as `prior_feedback`.
    pub feedback: Option<String>,
    /// Opaque, owned by the gate's module (e.g. triage's `EvidenceGateDetails`, fix's change set).
    pub details: Option<serde_json::Value>,
    /// Set by the inserting use case; kept on updates (gate wait and infra-retry accounting).
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewRun {
    pub run: Run,
    pub spec: RunSpec,
    /// Creation idempotency key (unique per org).
    pub create_key: Option<String>,
    pub source_ready: bool,
    /// Initial affinity (Verify runs: the parent's runner).
    pub affinity: Option<Affinity>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunUpdate {
    pub record: RunRecord,
    pub expected_version: u64,
}

impl RunUpdate {
    pub fn of(record: RunRecord) -> RunUpdate {
        let expected_version = record.version;
        RunUpdate { record, expected_version }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum GateWrite {
    Insert(GateRecord),
    /// Status, feedback, details and `verify_run` of an existing gate.
    Update(GateRecord),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunEventDraft {
    pub run_id: RunId,
    pub attempt: u16,
    pub at: OffsetDateTime,
    pub kind: RunEventKind,
}

/// Runner events of one batch; entries at or below the run's ingest cursor are dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ingest {
    pub run_id: RunId,
    pub attempt: u16,
    pub events: Vec<(u64, RunEventDraft)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandDraft {
    pub run_id: RunId,
    pub attempt: u16,
    pub command: RunCommand,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRecord {
    pub seq: u64,
    pub attempt: u16,
    pub issued_at: OffsetDateTime,
    pub command: RunCommand,
}

/// Artifact metadata; `artifact.bytes`/`sha256` are the runner's declaration until complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactRecord {
    pub artifact: Artifact,
    pub attempt: u16,
    pub name: String,
    pub content_type: String,
    pub state: ArtifactState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewArtifact {
    pub run_id: RunId,
    pub attempt: u16,
    pub kind: ArtifactKind,
    pub name: String,
    pub bytes: u64,
    pub sha256: [u8; 32],
    pub content_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdempotentReply {
    pub scope: String,
    pub key: String,
    pub request_hash: [u8; 32],
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredReply {
    pub request_hash: [u8; 32],
    pub body: Vec<u8>,
}

/// Everything one transaction writes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunWrite {
    pub inserts: Vec<NewRun>,
    pub updates: Vec<RunUpdate>,
    pub gates: Vec<GateWrite>,
    /// Server-generated events; `seq` assigned in the transaction.
    pub events: Vec<RunEventDraft>,
    /// Runner events; entries with `(attempt, local_seq) <= cursor` are dropped.
    pub ingest: Option<Ingest>,
    /// Command `seq` assigned in the transaction.
    pub commands: Vec<CommandDraft>,
    /// `(artifact, bytes observed)` → `complete`.
    pub complete_artifacts: Vec<(ArtifactId, u64)>,
    /// `JobQueue` outbox.
    pub jobs: Vec<NewJob>,
    pub reply: Option<IdempotentReply>,
}

impl RunWrite {
    pub fn is_empty(&self) -> bool {
        self.inserts.is_empty()
            && self.updates.is_empty()
            && self.gates.is_empty()
            && self.events.is_empty()
            && self.ingest.is_none()
            && self.commands.is_empty()
            && self.complete_artifacts.is_empty()
            && self.jobs.is_empty()
            && self.reply.is_none()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommitReceipt {
    /// New version of every inserted or updated run.
    pub versions: Vec<(RunId, u64)>,
    /// `(run, first, last)` event seqs appended per run (ingest included).
    pub seqs: Vec<(RunId, u64, u64)>,
    /// `(acked_local_seq, last_seq)` of the ingest batch.
    pub ingested: Option<(u64, u64)>,
    /// `(run, seq)` of every appended command, in input order.
    pub command_seqs: Vec<(RunId, u64)>,
}

impl CommitReceipt {
    pub fn version_of(&self, run: RunId) -> Option<u64> {
        self.versions.iter().find(|(id, _)| *id == run).map(|(_, v)| *v)
    }
}

/// `list` filter. `limit` 0 means 100.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunQuery {
    pub kind: Option<RunKind>,
    pub subject: Option<RunSubject>,
    pub parent: Option<RunId>,
    pub ticket: Option<TicketId>,
    pub repo: Option<RepoId>,
    pub sha: Option<CommitSha>,
    /// Empty = every status.
    pub statuses: Vec<RunStatus>,
    pub newest_first: bool,
    pub limit: u32,
    /// Continue after this run (in the chosen order).
    pub cursor: Option<RunId>,
}

impl RunQuery {
    pub const DEFAULT_LIMIT: u32 = 100;
    pub const MAX_LIMIT: u32 = 500;

    pub fn effective_limit(&self) -> u32 {
        if self.limit == 0 { Self::DEFAULT_LIMIT } else { self.limit.min(Self::MAX_LIMIT) }
    }

    /// The filter part of the query (cursor and limit are positional).
    pub fn matches(&self, rec: &RunRecord) -> bool {
        let run = &rec.run;
        self.kind.is_none_or(|k| run.kind == k)
            && self.subject.is_none_or(|s| run.subject == s)
            && self.parent.is_none_or(|p| run.parent == Some(p))
            && self.ticket.is_none_or(|t| rec.spec.ticket_id == Some(t))
            && self.repo.is_none_or(|r| rec.spec.repo_id == Some(r))
            && self.sha.is_none_or(|s| rec.spec.sha == Some(s))
            && (self.statuses.is_empty() || self.statuses.contains(&run.status))
    }
}

/// One `lease_next` request (cross-tenant).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseQuery {
    pub runner_id: RunnerId,
    pub instance_id: Uuid,
    pub scope: RunnerScope,
    pub capabilities: Capabilities,
    pub free: SlotCounts,
    pub prepared: Vec<PreparedBaseKey>,
    pub request_id: Uuid,
    /// Lease expiry to set (`now + lease_ttl`).
    pub until: OffsetDateTime,
    pub now: OffsetDateTime,
    /// Held Work-class leases allowed per `(org, repo)` (v1: 1).
    pub repo_limit: u16,
}

impl LeaseQuery {
    pub fn in_scope(&self, org: OrgId) -> bool {
        match &self.scope {
            RunnerScope::Shared => true,
            RunnerScope::Orgs(orgs) => orgs.contains(&org),
        }
    }

    /// Every filter except the repo slot (store-specific): leasable, scope, capabilities, a free
    /// slot of the run's class, and Verify affinity held by another runner.
    pub fn admits(&self, rec: &RunRecord) -> bool {
        rec.leasable()
            && self.in_scope(rec.org())
            && rec.spec.requirements.satisfied_by(&self.capabilities)
            && self.free.get(rec.spec.slot) > 0
            && rec.affinity.is_none_or(|a| a.runner_id == self.runner_id || a.until <= self.now)
    }

    /// Candidate order (ascending): own affinity first, then a warm base, then priority, then age.
    pub fn order_key(&self, rec: &RunRecord) -> (bool, bool, i16, OffsetDateTime, RunId) {
        let mine = rec.affinity.is_some_and(|a| a.runner_id == self.runner_id && a.until > self.now);
        let warm = rec.base_key().is_some_and(|k| self.prepared.contains(&k));
        (!mine, !warm, rec.spec.priority, rec.run.created_at, rec.run.id)
    }
}

/// The session columns of a runner identity (`runners` row).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerSession {
    pub runner_id: RunnerId,
    pub instance_id: Uuid,
    pub version: String,
    pub capabilities: Capabilities,
    pub at: OffsetDateTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DueItem {
    pub org_id: OrgId,
    pub run_id: RunId,
    pub reason: DueReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DueReason {
    LeaseExpired,
    WallExceeded,
    QueueWait,
    GateWait,
}

/// Usage of one run for the org usage report (`RunStore::usage`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunUsage {
    pub run_id: RunId,
    pub kind: RunKind,
    pub ticket_id: Option<TicketId>,
    /// `RunSpec.model` (empty for Verify).
    pub model: String,
    /// Metered if the run used the gateway, else harness-reported; `wall` is zero.
    pub usage: Usage,
    pub created_at: OffsetDateTime,
}

impl RunUsage {
    pub fn of(rec: &RunRecord) -> RunUsage {
        let usage = if rec.spec.gateway_metered() { rec.usage_metered } else { rec.usage_reported };
        RunUsage {
            run_id: rec.id(),
            kind: rec.run.kind,
            ticket_id: rec.spec.ticket_id,
            model: rec.spec.model.clone(),
            usage: Usage { wall: Duration::ZERO, ..usage },
            created_at: rec.run.created_at,
        }
    }
}

/// Wait limits `due` applies (store construction config, from `RunPolicy`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DueThresholds {
    /// `Queued` (since creation or the last requeue) for longer → `QueueWait`.
    pub queue_max_wait: Duration,
    /// A `Pending` gate older than this → `GateWait`.
    pub gate_wait_max: Duration,
}

impl Default for DueThresholds {
    fn default() -> Self {
        DueThresholds { queue_max_wait: Duration::from_secs(2 * 3600), gate_wait_max: Duration::from_secs(30 * 60) }
    }
}

// ---------------------------------------------------------------------------------------------
// RunStore
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RunStoreError {
    #[error("not found")]
    NotFound,
    #[error("conflict: {0:?}")]
    Conflict(RunConflict),
    #[error("invalid: {0}")]
    Invalid(String),
    #[error("unavailable: {0}")]
    Unavailable(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunConflict {
    /// `expected_version` mismatch: reload and retry.
    StaleVersion { run: RunId },
    /// `create_key` already used: return that run.
    Duplicate { run: RunId },
    /// Idempotency key already recorded.
    ReplyExists(StoredReply),
    /// Fence does not match the current lease.
    LeaseLost,
    /// Another non-terminal Diagnose/Fix run exists for the same `(kind, cause)`.
    SubjectBusy,
}

impl From<RunStoreError> for AppError {
    fn from(err: RunStoreError) -> Self {
        match err {
            RunStoreError::NotFound => AppError::NotFound("run".into()),
            RunStoreError::Conflict(RunConflict::SubjectBusy) => AppError::Conflict("already running".into()),
            RunStoreError::Conflict(c) => AppError::Conflict(format!("{c:?}")),
            RunStoreError::Invalid(msg) => AppError::Invalid(msg),
            RunStoreError::Unavailable(msg) => AppError::Unavailable(msg),
        }
    }
}

#[async_trait]
pub trait RunStore: Send + Sync {
    // ---- runs ----
    async fn get(&self, org: OrgId, id: RunId) -> Result<RunRecord, RunStoreError>;
    async fn list(&self, org: OrgId, q: &RunQuery) -> Result<Vec<RunRecord>, RunStoreError>;
    /// One transaction (module docs).
    async fn commit(&self, org: OrgId, w: RunWrite) -> Result<CommitReceipt, RunStoreError>;

    // ---- leasing (cross-tenant by design: runners serve a scope, not one org) ----
    async fn locate(&self, id: RunId) -> Result<OrgId, RunStoreError>;
    async fn lease_next(&self, q: &LeaseQuery) -> Result<Option<RunRecord>, RunStoreError>;
    /// The run still `Leased` to `runner` under `request_id`, if any.
    async fn lease_by_request(&self, runner: RunnerId, request_id: Uuid) -> Result<Option<RunRecord>, RunStoreError>;
    /// Conditional on `fence`; does not bump `version`.
    async fn extend_lease(&self, org: OrgId, id: RunId, fence: LeaseFence, until: OffsetDateTime) -> Result<RunRecord, RunStoreError>;
    /// Cross-tenant sweep: at most one item per run (wall > lease > gate > queue).
    async fn due(&self, now: OffsetDateTime, limit: u32) -> Result<Vec<DueItem>, RunStoreError>;
    /// Cross-tenant: runs whose lease `runner` holds.
    async fn held_by(&self, runner: RunnerId) -> Result<Vec<(OrgId, RunId)>, RunStoreError>;
    /// Sets `source_ready` on the matching `Queued` runs; returns how many changed.
    async fn mark_source_ready(&self, org: OrgId, repo: RepoId, sha: CommitSha) -> Result<u32, RunStoreError>;
    /// Writes the runner's session columns; returns the previous `instance_id`, if any.
    async fn record_runner_session(&self, s: &RunnerSession) -> Result<Option<Uuid>, RunStoreError>;
    /// Cross-tenant: the runner's last recorded session (lease matching reads its capabilities).
    async fn runner_session(&self, runner: RunnerId) -> Result<Option<RunnerSession>, RunStoreError>;

    // ---- events and commands ----
    async fn events_after(&self, org: OrgId, run: RunId, after: u64, limit: u32) -> Result<Vec<RunEvent>, RunStoreError>;
    async fn commands_after(&self, org: OrgId, run: RunId, attempt: u16, after: u64, limit: u32) -> Result<Vec<CommandRecord>, RunStoreError>;

    // ---- gates ----
    /// Oldest first.
    async fn gates(&self, org: OrgId, run: RunId) -> Result<Vec<GateRecord>, RunStoreError>;
    async fn gate_for_verify(&self, org: OrgId, verify_run: RunId) -> Result<Option<GateRecord>, RunStoreError>;

    // ---- artifact metadata ----
    /// Idempotent on `(run_id, attempt, kind, name)`: returns the existing record if present.
    async fn reserve_artifact(&self, org: OrgId, a: NewArtifact) -> Result<ArtifactRecord, RunStoreError>;
    async fn artifact(&self, org: OrgId, id: ArtifactId) -> Result<ArtifactRecord, RunStoreError>;
    /// Oldest first.
    async fn artifacts(&self, org: OrgId, run: RunId) -> Result<Vec<ArtifactRecord>, RunStoreError>;

    // ---- idempotent replies (scope encodes the caller, e.g. "runner:{id}:result:{run}") ----
    async fn reply(&self, scope: &str, key: &str) -> Result<Option<StoredReply>, RunStoreError>;

    // ---- reporting ----
    /// Runs created in `[from, to)`, oldest first.
    async fn usage(&self, org: OrgId, from: OffsetDateTime, to: OffsetDateTime) -> Result<Vec<RunUsage>, RunStoreError>;
}

// ---------------------------------------------------------------------------------------------
// RunSignals
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SignalTopic {
    RunEvents(RunId),
    RunCommands(RunId),
    Queue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Woke {
    Signal,
    Timeout,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SignalError {
    #[error("signals unavailable: {0}")]
    Unavailable(String),
}

/// Wakeups for long-polls and SSE. Hints only: at-most-once, coalesced, ids only. Waiters
/// re-read `RunStore` after every wake and at least every 2 s anyway.
#[async_trait]
pub trait RunSignals: Send + Sync {
    async fn subscribe(&self, topic: SignalTopic) -> Result<Box<dyn Subscription>, SignalError>;
}

#[async_trait]
pub trait Subscription: Send {
    /// Returns after >= 1 notification since the previous call (coalesced) or after `timeout`.
    async fn wait(&mut self, timeout: Duration) -> Woke;
}
