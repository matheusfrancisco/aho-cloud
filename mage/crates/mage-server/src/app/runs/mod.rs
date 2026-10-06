//! Run manager (runs.md "Server use cases"): every `Run` from creation to its terminal state —
//! scheduling, leases, the event log, commands, results per `RunKind`, gates, budgets,
//! cancellation, timeouts, and applying gate proofs to the domain.
//!
//! Every use case that changes a run follows the same loop: `get`, the pure [`machine::step`],
//! then `commit` with `expected_version`; on `StaleVersion` it reloads and retries up to
//! [`MAX_COMMIT_RETRIES`] times, then returns `AppError::Unavailable`.
//!
//! Entry points (all on [`RunManager`]):
//!
//! | Use case | Method | Caller |
//! |---|---|---|
//! | CreateRun | `create_run` | UI, triage, fix, gates |
//! | RegisterRunner, LeaseRun, Heartbeat | `register_runner`, `lease`, `heartbeat` | `/runner/v1` |
//! | IngestEvents, RecordUsage | `ingest_events`, `record_usage` | `/runner/v1`, model gateway |
//! | PollCommands, SteerRun, CancelRun | `poll_commands`, `steer_run`, `cancel_run` | `/runner/v1`, UI, triage, fix |
//! | RequestUpload, CompleteUpload, RefreshDownloads | `request_upload`, `complete_upload`, `refresh_downloads` | `/runner/v1` |
//! | SubmitResult (+ StartGate) | `submit_result` | `/runner/v1` |
//! | ApplyOutcome (+ EvaluateGate) | `apply_outcome` | job `runs.apply_outcome` |
//! | FinishRun, FailRun | `finish_run`, `fail_run` | `/runner/v1`, model gateway |
//! | ReapLeases, SourceReady | `reap_leases`, `source_ready` | jobs `runs.reap_leases`, `runs.source_ready` |
//! | StreamEvents | `event_stream` | SSE |

pub mod artifacts;
pub mod commands;
pub mod create;
pub mod events;
pub mod fail;
pub mod finish;
pub mod gates;
pub mod hooks;
pub mod lease;
pub mod machine;
pub mod outcome;
pub mod policy;
pub mod prompt;
pub mod query;
pub mod reap;
pub mod result;
pub mod source;
pub mod stream;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use mage_core::domain::{GateEvent, GateStatus, OrgId, RunId, RunKind, RunResult, RunStatus, RunStatusEvent, RunnerId};
use mage_core::ports::blob::{ArtifactStore, BlobError};
use mage_core::ports::clock::Clock;
use mage_core::ports::ids::IdGen;
use mage_core::ports::jobs::{JobError, JobQueue};
use mage_core::ports::repos::{RepoStore, RepoStoreError};
use mage_core::wire::runner::{Rejection, WireError, WireErrorCode};

use crate::error::AppError;
use crate::ports::audit::AuditLog;
use crate::ports::auth::{SignedToken, TokenClaims, TokenScope, TokenSigner};
use crate::ports::org::OrgStore;
use crate::ports::runs::{GateRecord, GateWrite, LeaseFence, RunQuery, RunRecord, RunSignals, RunStore, RunStoreError, RunUpdate, RunWrite};
use crate::ports::ticket::TicketStore;
use crate::ports::tokens::{Runner, TokenStore};

pub use commands::{CancelRun, SteerRun};
pub use create::{CreateRun, RunPrompt};
pub use events::{RecordUsage, UsageVerdict};
pub use fail::FailRun;
pub use hooks::{EvidenceDecision, FixAcceptance, FixGate, KindOutcome, RunHooks, TriageGate};
pub use machine::Initiator;
pub use policy::RunPolicy;
pub use stream::StreamItem;

/// Reload-and-retry bound of the commit loop.
pub const MAX_COMMIT_RETRIES: usize = 5;

/// Everything the run manager talks to.
pub struct RunDeps {
    pub runs: Arc<dyn RunStore>,
    pub signals: Arc<dyn RunSignals>,
    pub tickets: Arc<dyn TicketStore>,
    pub orgs: Arc<dyn OrgStore>,
    pub repos: Arc<dyn RepoStore>,
    pub artifacts: Arc<dyn ArtifactStore>,
    pub jobs: Arc<dyn JobQueue>,
    /// Runner identities, bootstrap tokens, assertion `jti` replay set, token revocations.
    pub token_store: Arc<dyn TokenStore>,
    pub signer: Arc<dyn TokenSigner>,
    pub audit: Arc<dyn AuditLog>,
    pub clock: Arc<dyn Clock>,
    pub ids: Arc<dyn IdGen>,
    pub policy: RunPolicy,
    pub hooks: RunHooks,
    /// Cancelled on SIGTERM: open long-polls and streams return at once.
    pub shutdown: CancellationToken,
}

/// The run manager facade (cheap to clone).
#[derive(Clone)]
pub struct RunManager {
    d: Arc<RunDeps>,
    runner_cache: Arc<Mutex<HashMap<RunnerId, (Runner, OffsetDateTime)>>>,
}

/// Runner identity status is cached this long per instance (revocation latency bound).
const RUNNER_CACHE_TTL: time::Duration = time::Duration::seconds(30);

/// Authenticated `/runner/v1` caller: token `sub`, `gen`, `inst`, and the identity record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerCaller {
    pub runner: Runner,
    pub instance_id: Uuid,
}

impl RunnerCaller {
    pub fn id(&self) -> RunnerId {
        self.runner.id
    }

    pub fn fence(&self, attempt: u16) -> LeaseFence {
        LeaseFence { runner_id: self.runner.id, instance_id: self.instance_id, attempt }
    }
}

/// Errors of the runner protocol (runs.md "Errors"); everything else is an [`AppError`].
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("lease lost")]
    LeaseLost,
    #[error("run terminal: {}", .0.name())]
    RunTerminal(RunStatus),
    #[error("invalid state: {}", .0.name())]
    InvalidState(RunStatus),
    #[error("idempotency key reused with a different body")]
    IdempotencyMismatch,
    #[error("too large: {0}")]
    TooLarge(String),
    #[error("rejected: {}", .0.message)]
    Rejected(Rejection),
    #[error("unsupported protocol version")]
    UnsupportedProtocol,
    #[error(transparent)]
    App(#[from] AppError),
}

impl From<RunStoreError> for ProtocolError {
    fn from(err: RunStoreError) -> Self {
        ProtocolError::App(err.into())
    }
}

impl From<mage_core::domain::DomainError> for ProtocolError {
    fn from(err: mage_core::domain::DomainError) -> Self {
        ProtocolError::App(err.into())
    }
}

impl ProtocolError {
    /// The `/runner/v1` error body.
    pub fn to_wire(&self) -> WireError {
        let app = |code: WireErrorCode, e: &AppError| WireError::new(code, e.to_string());
        match self {
            ProtocolError::LeaseLost => WireError::new(WireErrorCode::LeaseLost, "lease lost"),
            ProtocolError::RunTerminal(s) => {
                WireError { status: Some(*s), ..WireError::new(WireErrorCode::RunTerminal, format!("run is {}", s.name())) }
            }
            ProtocolError::InvalidState(s) => {
                WireError { status: Some(*s), ..WireError::new(WireErrorCode::InvalidState, format!("run is {}", s.name())) }
            }
            ProtocolError::IdempotencyMismatch => WireError::new(WireErrorCode::IdempotencyMismatch, self.to_string()),
            ProtocolError::TooLarge(m) => WireError::new(WireErrorCode::TooLarge, m.clone()),
            ProtocolError::Rejected(r) => WireError::rejected(r.clone()),
            ProtocolError::UnsupportedProtocol => WireError::new(WireErrorCode::UnsupportedProtocol, self.to_string()),
            ProtocolError::App(e) => match e {
                AppError::NotFound(_) => app(WireErrorCode::NotFound, e),
                AppError::Conflict(_) => app(WireErrorCode::InvalidState, e),
                AppError::Invalid(_) | AppError::Domain(_) => app(WireErrorCode::Invalid, e),
                AppError::Unauthorized => app(WireErrorCode::Unauthorized, e),
                AppError::Forbidden(_) => app(WireErrorCode::Forbidden, e),
                AppError::RateLimited { retry_after_secs } => WireError {
                    retry_after_ms: retry_after_secs.map(|s| s.saturating_mul(1000)),
                    ..WireError::new(WireErrorCode::RateLimited, e.to_string())
                },
                AppError::Unavailable(_) => app(WireErrorCode::Unavailable, e),
                AppError::Internal(_) => app(WireErrorCode::Internal, e),
            },
        }
    }
}

pub(crate) fn repo_err(err: RepoStoreError) -> AppError {
    match err {
        RepoStoreError::NotFound => AppError::NotFound("repo or version".into()),
        RepoStoreError::Conflict => AppError::Conflict("repo store conflict".into()),
        RepoStoreError::Invalid(m) => AppError::Invalid(m),
        RepoStoreError::Unavailable(m) => AppError::Unavailable(m),
    }
}

pub(crate) fn blob_err(err: BlobError) -> AppError {
    match err {
        BlobError::NotFound => AppError::NotFound("object".into()),
        BlobError::Conflict => AppError::Conflict("object conflict".into()),
        BlobError::Invalid(m) => AppError::Invalid(m),
        BlobError::Unavailable(m) => AppError::Unavailable(m),
        BlobError::TooLarge { limit } => AppError::Invalid(format!("object larger than {limit} bytes")),
    }
}

pub(crate) fn job_err(err: JobError) -> AppError {
    match err {
        JobError::NotFound => AppError::NotFound("job".into()),
        JobError::Conflict | JobError::LeaseLost => AppError::Conflict("job conflict".into()),
        JobError::Invalid(m) => AppError::Invalid(m),
        JobError::Unavailable(m) => AppError::Unavailable(m),
    }
}

pub(crate) fn is_stale(err: &RunStoreError) -> bool {
    matches!(err, RunStoreError::Conflict(crate::ports::runs::RunConflict::StaleVersion { .. }))
}

pub(crate) fn contention() -> AppError {
    AppError::Unavailable("run changed concurrently; retry".into())
}

/// SHA-256 of a request body (idempotency replay check).
pub(crate) fn request_hash(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

pub(crate) fn to_time(d: Duration) -> time::Duration {
    time::Duration::try_from(d).unwrap_or(time::Duration::MAX)
}

impl RunManager {
    pub fn new(deps: RunDeps) -> RunManager {
        RunManager { d: Arc::new(deps), runner_cache: Arc::new(Mutex::new(HashMap::new())) }
    }

    pub fn policy(&self) -> &RunPolicy {
        &self.d.policy
    }

    pub fn store(&self) -> &Arc<dyn RunStore> {
        &self.d.runs
    }

    pub fn signals(&self) -> &Arc<dyn RunSignals> {
        &self.d.signals
    }

    pub fn artifact_store(&self) -> &Arc<dyn ArtifactStore> {
        &self.d.artifacts
    }

    pub fn shutdown(&self) -> &CancellationToken {
        &self.d.shutdown
    }

    pub(crate) fn now(&self) -> OffsetDateTime {
        self.d.clock.now()
    }

    /// Signs a run-bound or runner token valid for `ttl` from now.
    pub(crate) fn mint(&self, scope: TokenScope, ttl: Duration) -> Result<(SignedToken, TokenClaims), AppError> {
        let iat = self.now();
        let claims = TokenClaims { scope, iat, exp: iat + to_time(ttl), jti: self.d.ids.uuid_v7() };
        let token = self.d.signer.sign(&claims)?;
        Ok((token, claims))
    }

    /// The runner identity (cached [`RUNNER_CACHE_TTL`]).
    pub(crate) async fn runner_identity(&self, id: RunnerId) -> Result<Runner, AppError> {
        let now = self.now();
        if let Some((runner, at)) = self.runner_cache.lock().get(&id).cloned()
            && now - at < RUNNER_CACHE_TTL
        {
            return Ok(runner);
        }
        let runner = match self.d.token_store.runner(id).await {
            Ok(r) => r,
            Err(crate::ports::tokens::TokenError::NotFound) => return Err(AppError::Unauthorized),
            Err(e) => return Err(e.into()),
        };
        self.runner_cache.lock().insert(id, (runner.clone(), now));
        Ok(runner)
    }

    pub(crate) fn forget_runner(&self, id: RunnerId) {
        self.runner_cache.lock().remove(&id);
    }

    /// Loads the run of a per-run runner call and checks scope and the lease fence.
    pub(crate) async fn fenced(&self, caller: &RunnerCaller, run: RunId, attempt: u16) -> Result<RunRecord, ProtocolError> {
        let rec = self.runner_run(caller, run).await?;
        if !rec.holds(&caller.fence(attempt)) {
            return Err(ProtocolError::LeaseLost);
        }
        Ok(rec)
    }

    /// Loads a run for a runner call (scope checked, no fence).
    pub(crate) async fn runner_run(&self, caller: &RunnerCaller, run: RunId) -> Result<RunRecord, ProtocolError> {
        let org = self.d.runs.locate(run).await?;
        let in_scope = match &caller.runner.scope {
            crate::ports::tokens::RunnerScope::Shared => true,
            crate::ports::tokens::RunnerScope::Orgs(orgs) => orgs.contains(&org),
        };
        if !in_scope {
            return Err(AppError::Forbidden("run outside the runner's scope".into()).into());
        }
        Ok(self.d.runs.get(org, run).await?)
    }

    /// Cancels the non-terminal Verify children of a parent that just went terminal and fails
    /// its still-`Pending` gates with "parent terminal" (gates already written in `w` are left
    /// alone).
    pub(crate) async fn cascade(&self, parent: &RunRecord, now: OffsetDateTime, w: &mut RunWrite) -> Result<(), AppError> {
        let org = parent.org();
        let open: Vec<RunStatus> = [RunStatus::Queued, RunStatus::Leased, RunStatus::Preparing, RunStatus::Running, RunStatus::AwaitingGate].into();
        let children = self
            .d
            .runs
            .list(org, &RunQuery { parent: Some(parent.id()), kind: Some(RunKind::Verify), statuses: open, limit: RunQuery::MAX_LIMIT, ..RunQuery::default() })
            .await?;
        for mut child in children {
            if w.updates.iter().any(|u| u.record.id() == child.id()) {
                continue;
            }
            machine::step(&mut child, RunStatusEvent::Cancel, Initiator::Server, now, w)?;
            w.updates.push(RunUpdate::of(child));
        }
        let written: Vec<_> = w
            .gates
            .iter()
            .map(|g| match g {
                GateWrite::Insert(r) | GateWrite::Update(r) => r.gate.id,
            })
            .collect();
        for mut g in self.d.runs.gates(org, parent.id()).await? {
            if g.gate.status == GateStatus::Pending && !written.contains(&g.gate.id) {
                g.gate.status = g.gate.status.apply(GateEvent::Fail { reason: "parent terminal".into(), at: now })?;
                w.gates.push(GateWrite::Update(g));
            }
        }
        Ok(())
    }

    /// Steps `rec` (cascading on terminal) and queues its update.
    pub(crate) async fn step_and_queue(
        &self,
        mut rec: RunRecord,
        event: RunStatusEvent,
        by: Initiator,
        now: OffsetDateTime,
        w: &mut RunWrite,
    ) -> Result<RunRecord, AppError> {
        let stepped = machine::step(&mut rec, event, by, now, w)?;
        if stepped.cascade {
            self.cascade(&rec, now, w).await?;
        }
        w.updates.push(RunUpdate::of(rec.clone()));
        Ok(rec)
    }

    /// Triage salvage hook (runs.md "Triage salvage"): before `BudgetExceeded` or `InfraError`
    /// for `NoResult` on a `Running` Triage run, a salvaged result ends the run `Succeeded`.
    /// Returns `true` when salvage was applied to `rec` (events and steps queued in `w`).
    pub(crate) async fn try_salvage(&self, rec: &mut RunRecord, now: OffsetDateTime, w: &mut RunWrite) -> Result<bool, AppError> {
        if rec.run.kind != RunKind::Triage || rec.run.status != RunStatus::Running {
            return Ok(false);
        }
        let gates = self.d.runs.gates(rec.org(), rec.id()).await?;
        let Some(result) = self.d.hooks.triage_gate.salvage(rec, &gates) else { return Ok(false) };
        let attempt = gates.iter().filter(|g| g.gate.kind == mage_core::domain::GateKind::Evidence).map(|g| g.gate.attempt).max().unwrap_or(0);
        rec.run.set_result(RunResult::Triage(result))?;
        w.events.push(crate::ports::runs::RunEventDraft {
            run_id: rec.id(),
            attempt: rec.run.attempt,
            at: now,
            kind: mage_core::domain::RunEventKind::Notice(format!("salvaged from evidence attempt {attempt}")),
        });
        machine::step(rec, RunStatusEvent::ResultSubmitted { needs_gate: false }, Initiator::Server, now, w)?;
        Ok(true)
    }

    /// Newest-first gate rows of `kind` on `run`.
    pub(crate) async fn gates_of(&self, org: OrgId, run: RunId, kind: mage_core::domain::GateKind) -> Result<Vec<GateRecord>, AppError> {
        let mut gates: Vec<GateRecord> = self.d.runs.gates(org, run).await?.into_iter().filter(|g| g.gate.kind == kind).collect();
        gates.sort_by_key(|g| std::cmp::Reverse(g.gate.attempt));
        Ok(gates)
    }
}
