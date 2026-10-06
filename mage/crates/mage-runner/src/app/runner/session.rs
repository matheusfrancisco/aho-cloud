//! Harness run task for Learn, Triage, Diagnose and Fix (runs.md "Harness run task", "Submit
//! handling"; executor.md UC2, UC5, UC7; harness.md "HarnessSpec per RunKind").
//!
//! Setup: heartbeat(Preparing) → env (`empty_env` for Triage; otherwise `prepare` → `fork`,
//! Diagnose/Fix `start_services`, Fix pinned repro script) → `RunTools` + Run MCP →
//! `HarnessSpec` → `Harness::start` → phase Running. Then a select loop over commands, submit
//! outcomes, turn ends, harness exit, server status and the fence, and finally the teardown.

use std::sync::Arc;
use std::time::Duration;

use mage_core::domain::{FailStage, GateFeedback, GateKind, GateOutcome, RunCommand, RunKind, RunStatus};
use mage_core::domain::event::RunEventKind;
use mage_core::wire::mcp::{KNOWLEDGE_TOOLS, submit_tool};
use mage_core::wire::runner::{CommandEnvelope, DownloadRef, FinishOutcome, OutboundEventKind, RunnerPhase, WireRunResult};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::artifacts::{tar_zst, upload_file, zstd_file};
use super::task::{PrepareError, RunCtx, assignment_recipe, exec_retryable, prepare_base};
use super::tools::{RunTools, RunToolsDeps, RunToolsInit, SubmitOutcome};
use crate::ports::executor::{
    Env, EnvPath, ExecError, ForkOptions, ForkPurpose, EgressProfile, LogQuery, LogSource, RelPath, ServiceSelector, WriteMode,
};
use crate::ports::harness::{
    BudgetHints, BuiltinToolPolicy, CredentialsMode, ExitReason, HarnessError, HarnessEvent, HarnessEventStream, HarnessExit,
    HarnessSession, HarnessSpec, KNOWLEDGE_MCP_SERVER, KillCause, McpServerName, McpServerSpec, ModelId, RUN_MCP_SERVER,
    RunEventMapper, Secret, TurnOutcome,
};
use crate::ports::link::{LinkConflict, LinkError};
use crate::ports::run_mcp::{RunMcpEndpoint, RunMcpHandle};
use mage_core::wire::runner::CredentialModeKind;

/// Env logs artifact cap (newest bytes kept).
const ENV_LOGS_CAP: usize = 20 * 1024 * 1024;
/// Lines requested per log source for the env logs artifact.
const ENV_LOG_TAIL_LINES: u32 = 50_000;

/// How the session ends; decides the teardown and the `finish` outcome.
#[derive(Debug, Clone, PartialEq)]
enum End {
    /// The server status is terminal (or will be: Abort, gate decided, accepted result).
    Done,
    /// The harness ended without `submit_*` after the nudges.
    NoResult,
    Failed { stage: FailStage, reason: String, retryable: bool },
    /// Harness gone while `AwaitingGate`: graceful teardown, `finish(Abandoned)`; the server sets
    /// `session_lost` and the gate continues.
    Abandoned,
    /// Drain grace ended: abort, destroy, `finish(Abandoned)` (requeued through `LeaseLost`).
    Drained,
    /// Lease lost / local deadline passed: hard kill, no uploads, `finish(Abandoned)` best effort.
    Fenced,
}

impl End {
    fn failed(stage: FailStage, reason: impl Into<String>, retryable: bool) -> End {
        End::Failed { stage, reason: reason.into(), retryable }
    }
}

struct Session {
    ctx: RunCtx,
    commands: mpsc::UnboundedReceiver<CommandEnvelope>,
    env: Option<Arc<dyn Env>>,
    tools: Option<Arc<RunTools>>,
    mcp: Option<Box<dyn RunMcpHandle>>,
    harness: Option<Arc<dyn HarnessSession>>,
    /// The last accepted submission (Learn: the recipe the SCIP step prepares).
    accepted: Option<WireRunResult>,
}

/// Entry point of the harness run task.
pub(crate) async fn run_session(ctx: RunCtx, commands: mpsc::UnboundedReceiver<CommandEnvelope>) {
    let mut session = Session { ctx, commands, env: None, tools: None, mcp: None, harness: None, accepted: None };
    let end = session.drive().await;
    session.teardown(end).await;
}

impl Session {
    fn kind(&self) -> RunKind {
        self.ctx.a.kind
    }

    fn emit(&self, kind: OutboundEventKind) {
        self.ctx.emit(kind);
    }

    /// Awaits `fut` unless the fence, a drain, an `Abort` command or a terminal server status
    /// comes first. Other commands cannot arrive before the harness runs (the server only steers
    /// `Running` runs, which the server sees only after the harness started).
    async fn guard<T>(
        ctx: &RunCtx,
        commands: &mut mpsc::UnboundedReceiver<CommandEnvelope>,
        fut: impl Future<Output = T>,
    ) -> Result<T, End> {
        let mut status = ctx.watch.status_rx();
        if status.borrow_and_update().is_some_and(|s| s.is_terminal()) {
            return Err(End::Done);
        }
        tokio::pin!(fut);
        loop {
            tokio::select! {
                biased;
                _ = ctx.watch.fence().cancelled() => return Err(End::Fenced),
                _ = ctx.abandon.cancelled() => return Err(End::Drained),
                Some(cmd) = commands.recv() => {
                    if let RunCommand::Abort { reason } = cmd.command {
                        ctx.emit(OutboundEventKind::Notice(format!("aborted during setup: {reason:?}")));
                        return Err(End::Done);
                    }
                }
                Ok(()) = status.changed() => {
                    if status.borrow_and_update().is_some_and(|s| s.is_terminal()) {
                        return Err(End::Done);
                    }
                }
                out = &mut fut => return Ok(out),
            }
        }
    }

    async fn drive(&mut self) -> End {
        if self.ctx.enter_phase(RunnerPhase::Preparing).await.is_err() && self.ctx.watch.is_fenced() {
            return End::Fenced;
        }
        let (submit_tx, submit_rx) = mpsc::unbounded_channel();
        let session = match self.setup(submit_tx).await {
            Ok(session) => session,
            Err(end) => return end,
        };
        self.run_loop(session, submit_rx).await
    }

    /// Env, Run MCP and harness; on success the phase is Running. `Err` = how the run ends.
    async fn setup(&mut self, submissions: mpsc::UnboundedSender<SubmitOutcome>) -> Result<Arc<dyn HarnessSession>, End> {
        let fail = |end: End| Err(end);
        let a = &self.ctx.a;
        let Some(assignment) = a.harness.clone() else {
            return fail(End::failed(FailStage::Internal, "harness run without a harness assignment", false));
        };
        let deps = self.ctx.deps.clone();

        // Env.
        let env = if a.kind == RunKind::Triage {
            let opts = ForkOptions { run_id: a.run_id, purpose: ForkPurpose::Triage, egress: EgressProfile::ServerOnly };
            match Self::guard(&self.ctx, &mut self.commands, deps.executor.empty_env(opts)).await? {
                Ok(env) => env,
                Err(e) => return fail(End::failed(FailStage::Fork, e.to_string(), exec_retryable(&e))),
            }
        } else {
            let recipe = match (a.kind, assignment_recipe(a)) {
                (_, Err(reason)) => return fail(End::failed(FailStage::Prepare, reason, false)),
                // Learn prepares a SourceOnly base; the agent writes the recipe.
                (RunKind::Learn, Ok(_)) => None,
                (_, Ok(None)) => return fail(End::failed(FailStage::Prepare, "assignment has no recipe", false)),
                (_, Ok(recipe)) => recipe,
            };
            let base = match Self::guard(&self.ctx, &mut self.commands, prepare_base(&self.ctx, recipe)).await? {
                Ok(base) => base,
                Err(PrepareError::Exec(e)) => return fail(End::failed(FailStage::Prepare, e.to_string(), exec_retryable(&e))),
                Err(e @ PrepareError::Refresh(_)) => return fail(End::failed(FailStage::Download, e.to_string(), true)),
                Err(e @ PrepareError::Assignment(_)) => return fail(End::failed(FailStage::Internal, e.to_string(), false)),
            };
            let opts = ForkOptions { run_id: a.run_id, purpose: ForkPurpose::Session, egress: EgressProfile::Registries };
            match Self::guard(&self.ctx, &mut self.commands, deps.executor.fork(&base, opts)).await? {
                Ok(env) => env,
                Err(e) => return fail(End::failed(FailStage::Fork, e.to_string(), exec_retryable(&e))),
            }
        };
        self.env = Some(env.clone());
        if let Err(e) = deps.state.attach_env(self.ctx.a.run_id, env.id()).await {
            tracing::warn!(run = %self.ctx.a.run_id, error = ?e, "RunnerState.attach_env failed");
        }

        // Diagnose / Fix: the base must boot in the fork.
        if matches!(self.kind(), RunKind::Diagnose | RunKind::Fix) {
            match Self::guard(&self.ctx, &mut self.commands, env.start_services(ServiceSelector::All)).await? {
                Ok(report) if report.all_healthy => {}
                Ok(report) => {
                    let unhealthy: Vec<String> = report
                        .services
                        .iter()
                        .filter(|s| !matches!(s.state, crate::ports::executor::ServiceState::Healthy))
                        .map(|s| format!("{}: {:?}", s.name, s.state))
                        .collect();
                    self.emit(OutboundEventKind::Error(format!("base does not boot in the fork: {}", unhealthy.join("; "))));
                    return fail(End::failed(FailStage::Prepare, "base_does_not_boot", false));
                }
                Err(e @ ExecError::ServiceUnhealthy { .. }) => {
                    self.emit(OutboundEventKind::Error(format!("base does not boot in the fork: {e}")));
                    return fail(End::failed(FailStage::Prepare, "base_does_not_boot", false));
                }
                Err(e) => return fail(End::failed(FailStage::Prepare, e.to_string(), exec_retryable(&e))),
            }
        }
        if self.kind() == RunKind::Fix {
            if let Err(reason) = Self::guard(&self.ctx, &mut self.commands, Self::place_repro_script(&self.ctx, env.as_ref())).await? {
                return fail(End::failed(FailStage::Download, reason, true));
            }
        }

        // Run MCP.
        let a = &self.ctx.a;
        let init = RunToolsInit {
            run_id: a.run_id,
            org_id: a.org_id,
            kind: a.kind,
            attempt: a.attempt,
            repo_id: a.repo.as_ref().map(|r| r.repo_id),
            sha: a.sha,
            env: Some(env.clone()),
            fix: a.fix.clone(),
            outbox: self.ctx.outbox.clone(),
            events: self.ctx.events.clone(),
            submissions,
        };
        let tools_deps =
            RunToolsDeps { browser: deps.browser.clone(), link: deps.link.clone(), clock: deps.clock.clone(), ids: deps.ids.clone() };
        let tools = match RunTools::new(init, tools_deps, deps.tools.clone()) {
            Ok(tools) => Arc::new(tools),
            Err(e) => return fail(End::failed(FailStage::RunMcp, e.to_string(), true)),
        };
        self.tools = Some(tools.clone());
        let mcp = match Self::guard(&self.ctx, &mut self.commands, deps.mcp_host.start(tools.clone())).await? {
            Ok(mcp) => mcp,
            Err(e) => return fail(End::failed(FailStage::RunMcp, format!("{e:?}"), true)),
        };
        self.ctx.scrubber.add(&mcp.endpoint().bearer);
        let spec = self.harness_spec(&assignment, &tools, mcp.endpoint());
        self.mcp = Some(mcp);

        // Harness.
        let spec = match spec {
            Ok(spec) => spec,
            Err(e) => return fail(harness_failure(&e)),
        };
        let Some(harness) = deps.harnesses.iter().find(|h| h.kind() == assignment.kind).cloned() else {
            return fail(End::failed(FailStage::Harness, format!("harness {:?} is not installed on this runner", assignment.kind), true));
        };
        let session: Arc<dyn HarnessSession> = match Self::guard(&self.ctx, &mut self.commands, harness.start(env.clone(), spec)).await? {
            Ok(session) => Arc::from(session),
            Err(e) => {
                self.emit(OutboundEventKind::Error(format!("harness did not start: {e}")));
                return fail(harness_failure(&e));
            }
        };
        self.harness = Some(session.clone());
        let _ = self.ctx.enter_phase(RunnerPhase::Running).await;
        Ok(session)
    }

    /// Fix: the pinned repro script (sha256 checked) at `.mage/repro/<name>` in the fork; the
    /// fix exclusion globs (`.mage/**`) keep it out of the patch.
    async fn place_repro_script(ctx: &RunCtx, env: &dyn Env) -> Result<(), String> {
        let a = &ctx.a;
        let Some(fix) = &a.fix else {
            return Err("Fix run without a fix spec".into());
        };
        let Some(dl) = a.downloads.iter().find(|d| d.what == DownloadRef::Artifact(fix.script)) else {
            return Err("assignment lacks the repro script download".into());
        };
        if dl.sha256 != fix.script_sha256 {
            return Err("repro script download does not match the pinned sha256".into());
        }
        let name = dl.name.clone().unwrap_or_else(|| fix.script.to_string());
        if name.is_empty() || name.contains('/') || name == "." || name == ".." {
            return Err(format!("repro script name {name:?} is not a file name"));
        }
        let local = ctx.outbox.join(format!("repro-{name}"));
        ctx.deps.link.download(a.run_id, a.attempt, dl, &local).await.map_err(|e| format!("repro script download failed: {e}"))?;
        let bytes = tokio::fs::read(&local).await.map_err(|e| format!("read {}: {e}", local.display()))?;
        let _ = tokio::fs::remove_file(&local).await;
        let digest: [u8; 32] = Sha256::digest(&bytes).into();
        if digest != fix.script_sha256 {
            return Err("repro script bytes do not match the pinned sha256".into());
        }
        let path = EnvPath::ws(&format!(".mage/repro/{name}")).map_err(|e| e.to_string())?;
        env.write(&path, &bytes, WriteMode::Overwrite).await.map_err(|e| format!("write {name} into the fork: {e}"))?;
        Ok(())
    }

    /// `HarnessSpec` per kind (harness.md table): Triage has no built-in tools and no workspace;
    /// the others get `WorkspaceRw` on the repository root.
    fn harness_spec(
        &self,
        h: &mage_core::wire::runner::HarnessAssignment,
        tools: &RunTools,
        run_mcp: &RunMcpEndpoint,
    ) -> Result<HarnessSpec, HarnessError> {
        let a = &self.ctx.a;
        let config = &self.ctx.deps.config;
        let mut mcp_servers = Vec::with_capacity(2);
        if let Some(k) = &a.knowledge {
            mcp_servers.push(McpServerSpec {
                name: McpServerName::new(KNOWLEDGE_MCP_SERVER)?,
                url: k.url.clone(),
                bearer: Secret::new(k.knowledge_token.clone()),
                tool_timeout: Duration::from_secs(config.knowledge_tool_timeout_secs),
                expected_tools: KNOWLEDGE_TOOLS.iter().map(|t| (*t).to_owned()).collect(),
            });
        }
        mcp_servers.push(McpServerSpec {
            name: McpServerName::new(RUN_MCP_SERVER)?,
            url: run_mcp.url.clone(),
            bearer: Secret::new(run_mcp.bearer.clone()),
            tool_timeout: Duration::from_secs(config.run_mcp_tool_timeout_secs),
            expected_tools: tools.tool_names().into_iter().map(str::to_owned).collect(),
        });
        let triage = a.kind == RunKind::Triage;
        let credentials = match h.credential_mode {
            CredentialModeKind::LocalLogin => CredentialsMode::LocalLogin,
            CredentialModeKind::Gateway => {
                let g = a.gateway.as_ref().ok_or_else(|| HarnessError::Invalid("gateway credential mode without gateway access".into()))?;
                CredentialsMode::Gateway { base_url: g.base_url.clone(), token: Secret::new(g.gateway_token.clone()) }
            }
            CredentialModeKind::Vertex => {
                let v = config.vertex.as_ref().ok_or_else(|| HarnessError::Unsupported("no [runner.task.vertex] target configured".into()))?;
                CredentialsMode::Vertex { project_id: v.project_id.clone(), region: v.region.clone() }
            }
        };
        Ok(HarnessSpec {
            run_id: a.run_id,
            org_id: a.org_id,
            kind: a.kind,
            prompt: render_prompt(&h.prompt, &a.prior_feedback),
            system_append: h.system_append.clone(),
            model: ModelId::new(h.model.clone())?,
            mcp_servers,
            builtin_tools: if triage { BuiltinToolPolicy::None } else { BuiltinToolPolicy::WorkspaceRw },
            cwd: if triage { None } else { Some(RelPath::new(".").map_err(HarnessError::Env)?) },
            env_allowlist: config.harness_env_allowlist.clone(),
            credentials,
            budget: BudgetHints {
                max_wall: a.budget.max_wall,
                max_cost_micros: Some(a.budget.max_cost_micros),
                max_turns: Some(a.budget.max_turns),
            },
            session_dir: RelPath::new("harness").map_err(HarnessError::Env)?,
        })
    }

    async fn run_loop(&mut self, session: Arc<dyn HarnessSession>, mut submits: mpsc::UnboundedReceiver<SubmitOutcome>) -> End {
        let stream = match session.events() {
            Ok(stream) => stream,
            Err(e) => return harness_failure(&e),
        };
        let (turn_tx, mut turns) = mpsc::unbounded_channel();
        let mode = self.ctx.a.harness.as_ref().map_or(CredentialModeKind::LocalLogin, |h| h.credential_mode);
        tokio::spawn(pump_harness(stream, RunEventMapper::new(mode), self.ctx.events.clone(), turn_tx));
        let exited = {
            let session = session.clone();
            async move { session.wait().await }
        };
        tokio::pin!(exited);
        let linger = Duration::from_secs(self.ctx.deps.config.accepted_linger_secs);
        let mut loop_state = LoopState { nudges: 0, gate_pending: false, accepted_at: None, terminal_since: None };
        let mut status = self.ctx.watch.status_rx();
        let watch = self.ctx.watch.clone();
        let abandon = self.ctx.abandon.clone();
        if status.borrow_and_update().is_some_and(|s| s.is_terminal()) {
            return End::Done;
        }
        loop {
            let deadline = [loop_state.accepted_at, loop_state.terminal_since].into_iter().flatten().min().map(|t| t + linger);
            tokio::select! {
                biased;
                _ = watch.fence().cancelled() => return End::Fenced,
                _ = abandon.cancelled() => return End::Drained,
                Some(cmd) = self.commands.recv() => {
                    if let Some(end) = self.on_command(cmd.command, &session, &mut loop_state).await {
                        return end;
                    }
                }
                Some(outcome) = submits.recv() => {
                    if let Some(end) = self.on_submit(outcome, &mut loop_state).await {
                        return end;
                    }
                }
                Some(outcome) = turns.recv() => {
                    if let Some(end) = self.on_turn_end(outcome, &session, &mut loop_state).await {
                        return end;
                    }
                }
                Ok(()) = status.changed() => {
                    let terminal = status.borrow_and_update().is_some_and(|s| s.is_terminal());
                    if terminal {
                        // A decided gate or an accepted result is followed by its command / turn
                        // end; anything else ends the session now.
                        if loop_state.gate_pending || loop_state.accepted_at.is_some() {
                            loop_state.terminal_since.get_or_insert_with(Instant::now);
                        } else {
                            return End::Done;
                        }
                    }
                }
                exit = &mut exited => return self.on_harness_exit(exit, &loop_state),
                _ = tokio::time::sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => return End::Done,
            }
        }
    }

    async fn on_command(&mut self, command: RunCommand, session: &Arc<dyn HarnessSession>, st: &mut LoopState) -> Option<End> {
        match command {
            RunCommand::Steer { text, .. } => {
                if self.ctx.watch.phase() == RunnerPhase::Running {
                    if let Err(e) = session.steer(text).await {
                        tracing::warn!(run = %self.ctx.a.run_id, error = %e, "steer failed");
                    }
                } else {
                    tracing::info!(run = %self.ctx.a.run_id, phase = ?self.ctx.watch.phase(), "steer ignored outside Running");
                }
                None
            }
            RunCommand::Abort { reason } => {
                self.emit(OutboundEventKind::Notice(format!("run aborted by the server: {reason:?}")));
                if let Err(e) = session.abort().await {
                    tracing::info!(run = %self.ctx.a.run_id, error = %e, "abort on the harness failed");
                }
                Some(End::Done)
            }
            RunCommand::GateFeedback(feedback) => {
                if let Some(tools) = &self.tools {
                    tools.gate_feedback_received();
                }
                st.gate_pending = false;
                match &feedback.outcome {
                    GateOutcome::Passed => {
                        if self.kind() == RunKind::Learn {
                            if let Some(end) = self.scip_step().await {
                                return Some(end);
                            }
                        }
                        Some(End::Done)
                    }
                    GateOutcome::Failed { attempts_left: 0, .. } => Some(End::Done),
                    GateOutcome::Failed { .. } => {
                        st.terminal_since = None;
                        self.ctx.set_phase(RunnerPhase::Running).await;
                        if let Err(e) = session.steer(render_feedback(&feedback, self.kind())).await {
                            tracing::warn!(run = %self.ctx.a.run_id, error = %e, "gate feedback steer failed");
                        }
                        None
                    }
                }
            }
        }
    }

    async fn on_submit(&mut self, outcome: SubmitOutcome, st: &mut LoopState) -> Option<End> {
        match outcome {
            SubmitOutcome::Accepted { ack, result } => {
                self.accepted = Some(result);
                self.ctx.watch.observe_status(ack.status);
                match ack.status {
                    RunStatus::AwaitingGate => {
                        st.gate_pending = true;
                        self.ctx.set_phase(RunnerPhase::AwaitingGate).await;
                        None
                    }
                    RunStatus::Succeeded => {
                        st.accepted_at = Some(Instant::now());
                        None
                    }
                    // Fix patch-policy rejection: the gate failed in the same commit; its
                    // GateFeedback follows.
                    RunStatus::Running if ack.gate.is_some() => {
                        st.gate_pending = true;
                        None
                    }
                    s if s.is_terminal() => Some(End::Done),
                    _ => None,
                }
            }
            SubmitOutcome::Rejected(_) => None,
            SubmitOutcome::Conflict(LinkConflict::LeaseLost) => {
                self.ctx.watch.trip("lease lost (409 lease_lost on a tool call)");
                Some(End::Fenced)
            }
            SubmitOutcome::Conflict(conflict) => {
                self.ctx.watch.observe_error(&LinkError::Conflict(conflict));
                None
            }
        }
    }

    async fn on_turn_end(&mut self, outcome: TurnOutcome, session: &Arc<dyn HarnessSession>, st: &mut LoopState) -> Option<End> {
        if st.accepted_at.is_some() {
            return Some(End::Done);
        }
        if st.gate_pending || self.ctx.watch.phase() == RunnerPhase::AwaitingGate {
            return None;
        }
        if self.ctx.watch.status().is_some_and(|s| s.is_terminal()) {
            return Some(End::Done);
        }
        if let TurnOutcome::Failed { message, .. } = &outcome {
            tracing::info!(run = %self.ctx.a.run_id, message, "turn failed");
        }
        if st.nudges < self.ctx.deps.config.nudge_limit {
            st.nudges += 1;
            if let Err(e) = session.steer(nudge_text(self.kind())).await {
                tracing::warn!(run = %self.ctx.a.run_id, error = %e, "nudge steer failed");
            }
            return None;
        }
        Some(End::NoResult)
    }

    fn on_harness_exit(&self, exit: HarnessExit, st: &LoopState) -> End {
        if st.accepted_at.is_some() || self.ctx.watch.status().is_some_and(|s| s.is_terminal()) {
            return End::Done;
        }
        if st.gate_pending || self.ctx.watch.phase() == RunnerPhase::AwaitingGate {
            self.emit(OutboundEventKind::Notice("harness exited while the gate runs; session lost".into()));
            return End::Abandoned;
        }
        let tail = tail_chars(&exit.stderr_tail, 1024);
        let reason = format!("harness exited unexpectedly: {:?}, code {:?}, signal {:?}: {tail}", exit.reason, exit.exit_code, exit.signal);
        self.emit(OutboundEventKind::Error(reason.clone()));
        End::failed(FailStage::Harness, reason, exit_retryable(&exit.reason))
    }

    /// Learn post-gate SCIP step (runs.md; executor.md UC5): prepare with the accepted recipe
    /// (normally a warm hit from the Boot verify), run the indexers, upload `ScipIndex`
    /// artifacts. Failures are `Notice`s and never change the run status.
    async fn scip_step(&mut self) -> Option<End> {
        self.ctx.set_phase(RunnerPhase::Finishing).await;
        let recipe = match &self.accepted {
            Some(WireRunResult::Learn(proposal)) => match mage_core::domain::recipe::RecipeSpec::parse(&proposal.toml) {
                Ok(recipe) => recipe,
                Err(e) => {
                    self.emit(OutboundEventKind::Notice(format!("SCIP step skipped: accepted recipe does not parse: {e}")));
                    return None;
                }
            },
            _ => {
                self.emit(OutboundEventKind::Notice("SCIP step skipped: no accepted recipe".into()));
                return None;
            }
        };
        let ctx = &self.ctx;
        let step = async {
            match prepare_base(ctx, Some(recipe)).await {
                Ok(base) => {
                    let report = ctx.deps.scip.run(ctx.a.run_id, ctx.a.attempt, &base, &ctx.outbox, &ctx.events).await;
                    tracing::info!(run = %ctx.a.run_id, ?report, "SCIP step done");
                }
                Err(e) => ctx.emit(OutboundEventKind::Notice(format!("SCIP step skipped: prepare failed: {e}"))),
            }
        };
        tokio::select! {
            biased;
            _ = ctx.watch.fence().cancelled() => Some(End::Fenced),
            _ = ctx.abandon.cancelled() => Some(End::Drained),
            () = step => None,
        }
    }

    /// Teardown (runs.md order): close the harness, upload transcript and logs while the lease
    /// is held, shut the Run MCP down, destroy the env, then `end_run`, flush, `finish`.
    async fn teardown(mut self, end: End) {
        tracing::info!(run = %self.ctx.a.run_id, attempt = self.ctx.a.attempt, ?end, "session ending");
        let fenced = end == End::Fenced || self.ctx.watch.is_fenced();
        if !fenced {
            self.ctx.set_phase(RunnerPhase::Finishing).await;
        }
        let grace = match &end {
            _ if fenced => Duration::ZERO,
            End::Drained => Duration::ZERO,
            _ => Duration::from_secs(self.ctx.deps.config.close_grace_secs),
        };
        let mut exit = None;
        if let Some(session) = self.harness.take() {
            if end == End::Drained {
                let _ = session.abort().await;
            }
            if let Err(e) = session.close(grace).await {
                tracing::info!(run = %self.ctx.a.run_id, error = %e, "harness close");
            }
            exit = Some(session.wait().await);
        }
        if !fenced && end != End::Drained {
            if let Some(exit) = &exit {
                self.upload_harness_files(exit).await;
            }
            if let Some(env) = &self.env {
                self.upload_env_logs(env.as_ref()).await;
            }
        }
        if let Some(mcp) = self.mcp.take() {
            mcp.shutdown().await;
        } else if let Some(tools) = &self.tools {
            tools.shutdown();
        }
        if let Some(env) = self.env.take() {
            if let Err(e) = env.destroy().await {
                tracing::warn!(run = %self.ctx.a.run_id, error = %e, "env destroy failed; the periodic reaper retries");
            }
        }
        let message = match &end {
            End::Failed { reason, .. } => Some(reason.clone()),
            End::Drained => Some("runner draining".into()),
            End::Abandoned => Some("harness exited while awaiting the gate".into()),
            _ => None,
        };
        let outcome = match end {
            End::Done => FinishOutcome::Done,
            End::NoResult => FinishOutcome::NoResult,
            End::Failed { stage, reason, retryable } => FinishOutcome::Failed { stage, reason, retryable },
            End::Abandoned | End::Drained => FinishOutcome::Abandoned,
            End::Fenced => return self.ctx.finish_fenced().await,
        };
        if fenced || self.ctx.watch.is_fenced() {
            return self.ctx.finish_fenced().await;
        }
        self.ctx.finish(outcome, message).await;
    }

    /// One teardown artifact; a failure is a `Notice` (409 `lease_lost` also self-fences).
    async fn upload(&self, kind: mage_core::domain::ArtifactKind, name: &str, content_type: &str, path: &std::path::Path) -> bool {
        if self.ctx.watch.is_fenced() {
            return false;
        }
        let a = &self.ctx.a;
        match upload_file(self.ctx.deps.link.as_ref(), a.run_id, a.attempt, kind, name, content_type, path).await {
            Ok(_) => true,
            Err(e) => {
                self.ctx.watch.observe_error(&e);
                self.emit(OutboundEventKind::Notice(format!("artifact {name} not uploaded: {e}")));
                false
            }
        }
    }

    /// Transcript (zstd) and harness stderr log; local copies are deleted once uploaded.
    async fn upload_harness_files(&self, exit: &HarnessExit) {
        use mage_core::domain::ArtifactKind;
        if let Some(transcript) = &exit.transcript {
            let packed = self.ctx.outbox.join("transcript.jsonl.zst");
            match zstd_file(transcript.clone(), packed.clone()).await {
                Ok(()) => {
                    if self.upload(ArtifactKind::Transcript, "transcript.jsonl.zst", "application/zstd", &packed).await {
                        let _ = tokio::fs::remove_file(transcript).await;
                    }
                }
                Err(e) => self.emit(OutboundEventKind::Notice(format!("artifact transcript.jsonl.zst not uploaded: compress: {e}"))),
            }
            let _ = tokio::fs::remove_file(&packed).await;
        }
        if let Some(log) = &exit.stderr_log {
            if self.upload(ArtifactKind::Log, "harness-stderr.log", "text/plain", log).await {
                let _ = tokio::fs::remove_file(log).await;
            }
        }
    }

    /// Prepare, egress, service and Run MCP (`mcp.jsonl`) logs as one `env-logs.tar.zst` (20 MiB
    /// cap, newest bytes kept).
    async fn upload_env_logs(&self, env: &dyn Env) {
        let mut sources = vec![
            ("prepare.log".to_owned(), LogSource::Prepare),
            ("egress.log".to_owned(), LogSource::Egress),
            ("mcp.jsonl".to_owned(), LogSource::Mcp),
        ];
        if let Some(recipe) = env.recipe() {
            sources.extend(recipe.services.keys().map(|name| (format!("svc/{name}.log"), LogSource::Service(name.clone()))));
        }
        let mut entries = Vec::new();
        for (name, source) in sources {
            if let Ok(slice) = env.logs(LogQuery { source, tail: ENV_LOG_TAIL_LINES, grep: None, since: None }).await {
                if !slice.lines.is_empty() {
                    let mut text = slice.lines.join("\n");
                    text.push('\n');
                    entries.push((name, text.into_bytes()));
                }
            }
        }
        if entries.is_empty() {
            return;
        }
        let packed = self.ctx.outbox.join("env-logs.tar.zst");
        match tar_zst(entries, ENV_LOGS_CAP, packed.clone()).await {
            Ok(()) => {
                self.upload(mage_core::domain::ArtifactKind::Log, "env-logs.tar.zst", "application/zstd", &packed).await;
            }
            Err(e) => self.emit(OutboundEventKind::Notice(format!("artifact env-logs.tar.zst not uploaded: pack: {e}"))),
        }
        let _ = tokio::fs::remove_file(&packed).await;
    }
}

struct LoopState {
    nudges: u32,
    /// A gate decision is pending (`AwaitingGate`, or a gate failed in the submit commit).
    gate_pending: bool,
    /// An accepted submission with no gate: teardown at the next turn end or after the linger.
    accepted_at: Option<Instant>,
    /// The server went terminal while a gate decision or turn end was still expected.
    terminal_since: Option<Instant>,
}

/// Drains the harness event stream: mapped events go to the event pump, turn ends to the task.
async fn pump_harness(
    mut stream: Box<dyn HarnessEventStream>,
    mut mapper: RunEventMapper,
    events: super::pump::EventTx,
    turns: mpsc::UnboundedSender<TurnOutcome>,
) {
    while let Some(event) = stream.next().await {
        if let Some(kind) = mapper.map(&event).and_then(outbound) {
            let _ = events.send(kind);
        }
        if let HarnessEvent::TurnEnded { outcome, .. } = event {
            let _ = turns.send(outcome);
        }
    }
}

/// The runner-sendable subset of `RunEventKind`.
fn outbound(kind: RunEventKind) -> Option<OutboundEventKind> {
    Some(match kind {
        RunEventKind::AssistantText(t) => OutboundEventKind::AssistantText(t),
        RunEventKind::ToolCall { name, args_preview } => OutboundEventKind::ToolCall { name, args_preview },
        RunEventKind::ToolResult { name, ok, preview } => OutboundEventKind::ToolResult { name, ok, preview },
        RunEventKind::Usage(u) => OutboundEventKind::Usage(u),
        RunEventKind::Notice(t) => OutboundEventKind::Notice(t),
        RunEventKind::Error(t) => OutboundEventKind::Error(t),
        RunEventKind::Status(_) | RunEventKind::GateStarted(_) | RunEventKind::GateResult(..) | RunEventKind::Artifact(_) => return None,
    })
}

/// Start failures: `Invalid`/`Unsupported`/`IsolationViolation`/`NotFound` are permanent;
/// `Unavailable`/`Protocol`/`Env` hand the lease back (harness.md "Error handling").
fn harness_failure(e: &HarnessError) -> End {
    End::failed(FailStage::Harness, e.to_string(), e.is_infrastructure())
}

/// Unexpected exits: crashes, protocol errors and unanswered aborts are infrastructure (retryable);
/// isolation violations, stalls and runaway output are not.
fn exit_retryable(reason: &ExitReason) -> bool {
    match reason {
        ExitReason::Closed | ExitReason::Crashed | ExitReason::ProtocolError(_) => true,
        ExitReason::Killed(KillCause::AbortUnanswered | KillCause::CloseGraceExpired) => true,
        ExitReason::Killed(KillCause::Stalled | KillCause::OutputLimit) | ExitReason::IsolationViolation(_) => false,
    }
}

fn tail_chars(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

/// First user message: the server-rendered prompt plus gate feedback from earlier attempts.
pub fn render_prompt(prompt: &str, prior_feedback: &[String]) -> String {
    if prior_feedback.is_empty() {
        return prompt.to_owned();
    }
    let mut out = String::with_capacity(prompt.len() + prior_feedback.iter().map(String::len).sum::<usize>() + 128);
    out.push_str(prompt);
    out.push_str("\n\n## Gate feedback from earlier attempts of this run\n");
    for (i, feedback) in prior_feedback.iter().enumerate() {
        out.push_str(&format!("\n### Earlier attempt {}\n\n{feedback}\n", i + 1));
    }
    out
}

fn gate_name(kind: GateKind) -> &'static str {
    match kind {
        GateKind::Evidence => "evidence",
        GateKind::Boot => "boot",
        GateKind::Repro => "repro",
        GateKind::Fix => "fix",
    }
}

/// Steer text for a failed gate with attempts left; `details` is the server-rendered feedback.
pub fn render_feedback(feedback: &GateFeedback, kind: RunKind) -> String {
    let GateOutcome::Failed { reason, details, attempts_left } = &feedback.outcome else {
        return format!("The {} gate (attempt {}) passed.", gate_name(feedback.kind), feedback.attempt);
    };
    let tool = submit_tool(kind).map_or("your submit tool", |t| t.name);
    format!(
        "The {} gate (attempt {}) failed: {reason}\n\n{details}\n\nGate attempts left: {attempts_left}. Fix the problem, then call {tool} again.",
        gate_name(feedback.kind),
        feedback.attempt,
    )
}

/// Nudge after a turn that ended without `submit_*`.
pub fn nudge_text(kind: RunKind) -> String {
    let tool = submit_tool(kind).map_or("submit", |t| t.name);
    format!("You ended without calling {tool}. Call it now with your best result.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_carries_prior_feedback_in_order() {
        assert_eq!(render_prompt("Do it.", &[]), "Do it.");
        let p = render_prompt("Do it.", &["first".into(), "second".into()]);
        let first = p.find("first").unwrap();
        let second = p.find("second").unwrap();
        assert!(p.starts_with("Do it.\n\n## Gate feedback"));
        assert!(first < second);
    }

    #[test]
    fn nudge_names_the_kind_submit_tool() {
        assert_eq!(nudge_text(RunKind::Diagnose), "You ended without calling submit_diagnosis. Call it now with your best result.");
        assert!(nudge_text(RunKind::Triage).contains("submit_triage"));
    }

    #[test]
    fn exits_classified_for_requeue() {
        assert!(exit_retryable(&ExitReason::Crashed));
        assert!(exit_retryable(&ExitReason::ProtocolError("x".into())));
        assert!(!exit_retryable(&ExitReason::IsolationViolation("x".into())));
        assert!(!exit_retryable(&ExitReason::Killed(KillCause::Stalled)));
    }
}
