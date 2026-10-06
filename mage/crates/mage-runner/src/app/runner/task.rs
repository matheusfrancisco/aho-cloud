//! One leased run attempt (runs.md "Harness run task", "Verify run task"): shared dependencies
//! and config, the per-run context with its pumps, dispatch per `RunKind`, the Verify task, and
//! the common finish steps.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use mage_core::domain::recipe::RecipeSpec;
use mage_core::domain::{FailStage, RunKind, VerifyOutcome};
use mage_core::ports::{Clock, IdGen};
use mage_core::wire::runner::{
    CommandEnvelope, Download, FinishOutcome, FinishReport, HeartbeatAck, OutboundEventKind, RunAssignment, RunnerPhase, WireRunResult,
};
use mage_core::domain::RunCommand;
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::pump::{EventPump, EventTx, Heartbeats, RunWatch, Scrubber, poll_commands};
use super::scip::ScipStep;
use super::tools::RunToolsConfig;
use super::verify::{VerifyRunner, failed_result, prepare_failure_outcome};
use crate::ports::browser::BrowserRunner;
use crate::ports::executor::{BaseRef, BundleSource, ExecError, Executor, PrepareObserver, PrepareRequest, ReapScope, StepStatus};
use crate::ports::harness::Harness;
use crate::ports::link::{LinkConflict, LinkError, ServerLink};
use crate::ports::run_mcp::RunMcpHost;
use crate::ports::runner_state::{RunPhase, RunnerState};

/// Run-task settings (`[runner.task]`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskConfig {
    /// Runner data dir; each run uses `<data_dir>/runs/<run_id>/outbox/` for files awaiting upload.
    pub data_dir: PathBuf,
    /// How many times a turn that ended without `submit_*` is nudged before `finish(NoResult)`.
    #[serde(default = "defaults::nudge_limit")]
    pub nudge_limit: u32,
    /// Graceful harness close at teardown.
    #[serde(default = "defaults::close_grace_secs")]
    pub close_grace_secs: u64,
    /// After an accepted submission with no gate: teardown at the next `TurnEnded` or after this.
    #[serde(default = "defaults::accepted_linger_secs")]
    pub accepted_linger_secs: u64,
    /// Teardown event flush; the rest is dropped with a log line.
    #[serde(default = "defaults::final_flush_secs")]
    pub final_flush_secs: u64,
    /// `McpServerSpec.tool_timeout` of the Run MCP (`exec`/`playwright_run` run minutes).
    #[serde(default = "defaults::run_mcp_tool_timeout_secs")]
    pub run_mcp_tool_timeout_secs: u64,
    /// `McpServerSpec.tool_timeout` of the Knowledge MCP.
    #[serde(default = "defaults::knowledge_tool_timeout_secs")]
    pub knowledge_tool_timeout_secs: u64,
    /// `HarnessSpec.env_allowlist`.
    #[serde(default)]
    pub harness_env_allowlist: Vec<String>,
    /// Target of the `Vertex` credential mode (internal deployments).
    #[serde(default)]
    pub vertex: Option<VertexTarget>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VertexTarget {
    pub project_id: String,
    pub region: String,
}

mod defaults {
    pub fn nudge_limit() -> u32 {
        1
    }
    pub fn close_grace_secs() -> u64 {
        30
    }
    pub fn accepted_linger_secs() -> u64 {
        60
    }
    pub fn final_flush_secs() -> u64 {
        30
    }
    pub fn run_mcp_tool_timeout_secs() -> u64 {
        960
    }
    pub fn knowledge_tool_timeout_secs() -> u64 {
        120
    }
}

impl TaskConfig {
    pub fn new(data_dir: PathBuf) -> Self {
        TaskConfig {
            data_dir,
            nudge_limit: defaults::nudge_limit(),
            close_grace_secs: defaults::close_grace_secs(),
            accepted_linger_secs: defaults::accepted_linger_secs(),
            final_flush_secs: defaults::final_flush_secs(),
            run_mcp_tool_timeout_secs: defaults::run_mcp_tool_timeout_secs(),
            knowledge_tool_timeout_secs: defaults::knowledge_tool_timeout_secs(),
            harness_env_allowlist: Vec::new(),
            vertex: None,
        }
    }
}

/// Everything a run task uses; built once by the composition root.
pub struct RunnerDeps {
    pub link: Arc<dyn ServerLink>,
    pub executor: Arc<dyn Executor>,
    pub state: Arc<dyn RunnerState>,
    /// Installed harness adapters, one per `HarnessKind`.
    pub harnesses: Vec<Arc<dyn Harness>>,
    pub mcp_host: Arc<dyn RunMcpHost>,
    pub browser: Arc<dyn BrowserRunner>,
    pub verify: Arc<VerifyRunner>,
    pub scip: Arc<ScipStep>,
    pub clock: Arc<dyn Clock>,
    pub ids: Arc<dyn IdGen>,
    pub tools: RunToolsConfig,
    pub config: TaskConfig,
}

/// Runs one assignment to its end: dispatch per kind, then finish. Never panics on server or
/// executor errors; every path ends with `RunnerState::end_run` and a `finish` attempt.
pub async fn run_assignment(deps: Arc<RunnerDeps>, a: RunAssignment, lease_sent_at: Instant, abandon: CancellationToken) {
    let (ctx, commands) = RunCtx::start(deps, a, lease_sent_at, abandon).await;
    tracing::info!(run = %ctx.a.run_id, attempt = ctx.a.attempt, kind = ?ctx.a.kind, "run task started");
    match ctx.a.kind {
        RunKind::Verify => run_verify(ctx, commands).await,
        _ => super::session::run_session(ctx, commands).await,
    }
}

struct Background {
    stop: CancellationToken,
    pump_close: CancellationToken,
    pump: JoinHandle<()>,
    beats: JoinHandle<()>,
    poller: JoinHandle<()>,
}

/// Per-run context shared by the session and Verify tasks.
pub struct RunCtx {
    pub deps: Arc<RunnerDeps>,
    pub a: RunAssignment,
    /// `<data_dir>/runs/<run_id>/`.
    pub run_dir: PathBuf,
    /// `<run_dir>/outbox/`: exported artifacts awaiting upload.
    pub outbox: PathBuf,
    pub watch: Arc<RunWatch>,
    pub heartbeats: Arc<Heartbeats>,
    pub events: EventTx,
    pub scrubber: Arc<Scrubber>,
    /// Cancelled by the daemon when a drain's grace period ends.
    pub abandon: CancellationToken,
    background: Background,
}

impl RunCtx {
    /// Creates the run dir and starts the event pump, heartbeat ticker and command poller.
    pub async fn start(
        deps: Arc<RunnerDeps>,
        a: RunAssignment,
        lease_sent_at: Instant,
        abandon: CancellationToken,
    ) -> (RunCtx, mpsc::UnboundedReceiver<CommandEnvelope>) {
        let run_dir = deps.config.data_dir.join("runs").join(a.run_id.to_string());
        let outbox = run_dir.join("outbox");
        if let Err(e) = tokio::fs::create_dir_all(&outbox).await {
            tracing::warn!(run = %a.run_id, error = %e, dir = %outbox.display(), "cannot create the run outbox");
        }
        let watch = RunWatch::new(RunnerPhase::Preparing);
        let scrubber = Arc::new(Scrubber::new());
        if let Some(k) = &a.knowledge {
            scrubber.add(&k.knowledge_token);
        }
        if let Some(g) = &a.gateway {
            scrubber.add(&g.gateway_token);
        }
        let (events, pump) = EventPump::new(deps.link.clone(), watch.clone(), scrubber.clone(), deps.clock.clone(), a.run_id, a.attempt);
        let heartbeats = Heartbeats::new(deps.link.clone(), watch.clone(), a.run_id, a.attempt, a.lease, lease_sent_at);
        let stop = CancellationToken::new();
        let pump_close = CancellationToken::new();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let final_flush = Duration::from_secs(deps.config.final_flush_secs);
        let background = Background {
            pump: tokio::spawn(pump.run(pump_close.clone(), final_flush)),
            beats: tokio::spawn(heartbeats.clone().run(stop.clone())),
            poller: tokio::spawn(poll_commands(deps.link.clone(), watch.clone(), a.run_id, a.attempt, cmd_tx, stop.clone())),
            stop,
            pump_close,
        };
        let ctx = RunCtx { deps, a, run_dir, outbox, watch, heartbeats, events, scrubber, abandon, background };
        (ctx, cmd_rx)
    }

    pub fn emit(&self, kind: OutboundEventKind) {
        let _ = self.events.send(kind);
    }

    /// Local phase: heartbeats carry it (a change triggers one at once) and `RunnerState` records it.
    pub async fn set_phase(&self, phase: RunnerPhase) {
        self.watch.set_phase(phase);
        if let Err(e) = self.deps.state.set_phase(self.a.run_id, local_phase(phase)).await {
            tracing::warn!(run = %self.a.run_id, error = ?e, "RunnerState.set_phase failed");
        }
    }

    /// Sets the phase and sends its heartbeat inline, so the server applies `EnvPreparing` /
    /// `Started` before the next step (e.g. before the agent can submit).
    pub async fn enter_phase(&self, phase: RunnerPhase) -> Result<HeartbeatAck, LinkError> {
        self.watch.set_phase_quiet(phase);
        if let Err(e) = self.deps.state.set_phase(self.a.run_id, local_phase(phase)).await {
            tracing::warn!(run = %self.a.run_id, error = ?e, "RunnerState.set_phase failed");
        }
        self.heartbeats.beat().await
    }

    /// Steps 5–7 of the teardown order: `end_run`, flush events, `finish`; then stops the
    /// heartbeat ticker and the command poller (the lease stays held until `finish` returns)
    /// and deletes the run dir.
    pub async fn finish(self, outcome: FinishOutcome, message: Option<String>) {
        self.end_run().await;
        self.background.pump_close.cancel();
        let RunCtx { deps, a, run_dir, background, .. } = self;
        let _ = background.pump.await;
        let report = FinishReport { outcome, message };
        match deps.link.finish(a.run_id, a.attempt, &report).await {
            Ok(status) => tracing::info!(run = %a.run_id, attempt = a.attempt, outcome = ?report.outcome, ?status, "run finished"),
            Err(e) => tracing::error!(run = %a.run_id, attempt = a.attempt, outcome = ?report.outcome, error = %e, "finish failed; the reaper clears the lease"),
        }
        background.stop.cancel();
        let _ = background.beats.await;
        let _ = background.poller.await;
        remove_dir(&run_dir).await;
    }

    /// Self-fenced end: no uploads, no event flush; `finish(Abandoned)` best effort (a 409 is
    /// expected and ignored).
    pub async fn finish_fenced(self) {
        let reason = self.watch.fence_reason().unwrap_or_else(|| "lease lost".into());
        tracing::warn!(run = %self.a.run_id, attempt = self.a.attempt, reason, "self-fenced");
        self.end_run().await;
        self.background.stop.cancel();
        self.background.pump_close.cancel();
        let RunCtx { deps, a, run_dir, background, .. } = self;
        let report = FinishReport { outcome: FinishOutcome::Abandoned, message: Some(reason) };
        match deps.link.finish(a.run_id, a.attempt, &report).await {
            Ok(_) | Err(LinkError::Conflict(_)) => {}
            Err(e) => tracing::info!(run = %a.run_id, error = %e, "finish(Abandoned) after self-fence not delivered"),
        }
        let _ = background.pump.await;
        let _ = background.beats.await;
        let _ = background.poller.await;
        remove_dir(&run_dir).await;
    }

    async fn end_run(&self) {
        if let Err(e) = self.deps.state.end_run(self.a.run_id).await {
            tracing::warn!(run = %self.a.run_id, error = ?e, "RunnerState.end_run failed");
        }
    }

    /// Cleans forks a cancelled step left behind (their run is no longer active).
    pub async fn reap_leftovers(&self) {
        if let Err(e) = self.deps.executor.reap(ReapScope::Periodic).await {
            tracing::warn!(run = %self.a.run_id, error = %e, "reap after an interrupted step failed");
        }
    }
}

async fn remove_dir(dir: &std::path::Path) {
    match tokio::fs::remove_dir_all(dir).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(dir = %dir.display(), error = %e, "cannot remove the run dir"),
    }
}

/// Wire phase → local `RunnerState` phase.
pub fn local_phase(phase: RunnerPhase) -> RunPhase {
    match phase {
        RunnerPhase::Preparing => RunPhase::Preparing,
        RunnerPhase::Running => RunPhase::Running,
        RunnerPhase::AwaitingGate => RunPhase::AwaitingGate,
        RunnerPhase::Finishing => RunPhase::Finishing,
    }
}

// ---------------------------------------------------------------------------------------------
// Prepare
// ---------------------------------------------------------------------------------------------

/// `PrepareObserver` callbacks become `Notice` events (build progress in the UI). Build output
/// lines stay in the env logs artifact; only step transitions are reported.
pub struct NoticeObserver {
    pub events: EventTx,
}

impl PrepareObserver for NoticeObserver {
    fn step(&self, step: &str, status: StepStatus) {
        let text = match status {
            StepStatus::Started => format!("prepare: {step} started"),
            StepStatus::Ok { secs } => format!("prepare: {step} ok ({secs} s)"),
            StepStatus::Failed { reason, .. } => format!("prepare: {step} failed: {reason}"),
        };
        let _ = self.events.send(OutboundEventKind::Notice(text));
    }

    fn line(&self, _step: &str, _line: &str) {}
}

#[derive(Debug)]
pub enum PrepareError {
    /// The assignment lacks repo, sha or source bundle.
    Assignment(String),
    Exec(ExecError),
    /// `refresh_downloads` after an expired bundle URL failed.
    Refresh(LinkError),
}

impl std::fmt::Display for PrepareError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PrepareError::Assignment(m) => write!(f, "{m}"),
            PrepareError::Exec(e) => write!(f, "{e}"),
            PrepareError::Refresh(e) => write!(f, "refreshing the source bundle URL failed: {e}"),
        }
    }
}

/// Transient prepare failures (network, disk pressure) hand the lease back for a requeue; a
/// recipe that does not build or boot does not.
pub fn exec_retryable(e: &ExecError) -> bool {
    matches!(e, ExecError::Unavailable(_) | ExecError::Io(_) | ExecError::QuotaExceeded(_) | ExecError::SourceUrlExpired(_))
}

/// `Executor::prepare` for the assignment's `(repo, sha)` with `recipe` (cached by the executor).
/// An expired bundle URL is refreshed through `refresh_downloads` and retried once.
pub async fn prepare_base(ctx: &RunCtx, recipe: Option<RecipeSpec>) -> Result<BaseRef, PrepareError> {
    let a = &ctx.a;
    let (Some(repo), Some(sha), Some(source)) = (&a.repo, a.sha, &a.source) else {
        return Err(PrepareError::Assignment("assignment without repo, sha or source bundle".into()));
    };
    let observer = NoticeObserver { events: ctx.events.clone() };
    let request = |dl: &Download| PrepareRequest {
        org_id: a.org_id,
        repo_id: repo.repo_id,
        sha,
        recipe: recipe.clone(),
        source_bundle: BundleSource { url: dl.url.clone(), sha256: dl.sha256, bytes: dl.bytes },
    };
    match ctx.deps.executor.prepare(request(source), &observer).await {
        Err(ExecError::SourceUrlExpired(_)) => {
            let fresh = ctx.deps.link.refresh_downloads(a.run_id, a.attempt, &[source.what]).await.map_err(PrepareError::Refresh)?;
            let Some(dl) = fresh.into_iter().find(|d| d.what == source.what) else {
                return Err(PrepareError::Refresh(LinkError::Invalid("refresh_downloads omitted the source bundle".into())));
            };
            ctx.deps.executor.prepare(request(&dl), &observer).await.map_err(PrepareError::Exec)
        }
        other => other.map_err(PrepareError::Exec),
    }
}

/// Parses the assignment's recipe TOML.
pub fn assignment_recipe(a: &RunAssignment) -> Result<Option<RecipeSpec>, String> {
    a.recipe.as_ref().map(|r| RecipeSpec::parse(&r.toml).map_err(|e| format!("recipe {} does not parse: {e}", r.hash))).transpose()
}

// ---------------------------------------------------------------------------------------------
// Verify task
// ---------------------------------------------------------------------------------------------

enum Stop {
    Fenced,
    Drained,
    /// The server already ended the run (Abort command or terminal status).
    ServerDone,
}

/// Verify run (runs.md "Verify run task"): heartbeat(Preparing), prepare (recipe failures →
/// `BootFailed`, infrastructure → `Errored`, via `verify::prepare_failure_outcome`),
/// heartbeat(Running), run the `VerifySpec`, `submit_result(Verify)`, `finish(Done)`.
async fn run_verify(ctx: RunCtx, mut commands: mpsc::UnboundedReceiver<CommandEnvelope>) {
    if ctx.enter_phase(RunnerPhase::Preparing).await.is_err() && ctx.watch.is_fenced() {
        return ctx.finish_fenced().await;
    }
    let mut status = ctx.watch.status_rx();
    let work = async {
        let recipe = match assignment_recipe(&ctx.a) {
            Ok(recipe) => recipe,
            Err(reason) => return failed_result(VerifyOutcome::BootFailed { reason }),
        };
        let base = match prepare_base(&ctx, recipe).await {
            Ok(base) => base,
            Err(PrepareError::Exec(e)) => return failed_result(prepare_failure_outcome(&e)),
            Err(e) => return failed_result(VerifyOutcome::Errored { reason: e.to_string() }),
        };
        // "verify started" (Preparing → Running on the server) before the spec runs.
        let _ = ctx.enter_phase(RunnerPhase::Running).await;
        ctx.deps.verify.run(&ctx.a, &base, &ctx.outbox).await
    };
    let stopped = {
        tokio::pin!(work);
        loop {
            tokio::select! {
                biased;
                _ = ctx.watch.fence().cancelled() => break Err(Stop::Fenced),
                _ = ctx.abandon.cancelled() => break Err(Stop::Drained),
                Some(cmd) = commands.recv() => {
                    if let RunCommand::Abort { reason } = cmd.command {
                        ctx.emit(OutboundEventKind::Notice(format!("verify aborted: {reason:?}")));
                        break Err(Stop::ServerDone);
                    }
                }
                Ok(()) = status.changed() => {
                    if status.borrow_and_update().is_some_and(|s| s.is_terminal()) {
                        break Err(Stop::ServerDone);
                    }
                }
                result = &mut work => break Ok(result),
            }
        }
    };
    let result = match stopped {
        Ok(result) => result,
        Err(stop) => {
            ctx.emit(OutboundEventKind::Notice("verify interrupted".into()));
            ctx.set_phase(RunnerPhase::Finishing).await;
            ctx.reap_leftovers().await;
            return match stop {
                Stop::Fenced => ctx.finish_fenced().await,
                Stop::Drained => ctx.finish(FinishOutcome::Abandoned, Some("runner draining".into())).await,
                Stop::ServerDone => ctx.finish(FinishOutcome::Done, None).await,
            };
        }
    };
    ctx.set_phase(RunnerPhase::Finishing).await;
    let submission_id = ctx.deps.ids.uuid_v4();
    let wire = WireRunResult::Verify(result);
    let outcome = loop {
        let submitted = tokio::select! {
            biased;
            _ = ctx.watch.fence().cancelled() => return ctx.finish_fenced().await,
            _ = ctx.abandon.cancelled() => break FinishOutcome::Abandoned,
            r = ctx.deps.link.submit_result(ctx.a.run_id, ctx.a.attempt, submission_id, &wire) => r,
        };
        match submitted {
            Ok(ack) => {
                ctx.watch.observe_status(ack.status);
                break FinishOutcome::Done;
            }
            Err(LinkError::Unavailable(m)) => {
                tracing::warn!(run = %ctx.a.run_id, reason = %m, "verify result not delivered yet; retrying with the same submission id");
            }
            Err(LinkError::Conflict(LinkConflict::RunTerminal(_) | LinkConflict::InvalidState(_))) => break FinishOutcome::Done,
            Err(e) if e.is_lease_lost() || matches!(e, LinkError::NotFound | LinkError::Forbidden) => {
                ctx.watch.observe_error(&e);
                return ctx.finish_fenced().await;
            }
            Err(LinkError::Rejected(r)) => {
                ctx.emit(OutboundEventKind::Error(format!("verify result rejected: {}", r.message)));
                break FinishOutcome::Failed { stage: FailStage::Verify, reason: r.message, retryable: false };
            }
            Err(e) => {
                ctx.emit(OutboundEventKind::Error(format!("verify result not accepted: {e}")));
                break FinishOutcome::Failed { stage: FailStage::Internal, reason: e.to_string(), retryable: false };
            }
        }
    };
    ctx.finish(outcome, None).await;
}

#[cfg(test)]
mod scenarios;
