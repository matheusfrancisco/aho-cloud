//! Run task orchestration over fakes (runs.md "Testing", runner tasks): `FakeServerLink`,
//! `FakeExecutor`, `MemRunnerState`, scripted `FakeHarness`, `FakeRunMcpHost` (whose recorded
//! `RunTools` the tests call like the agent would), real `VerifyRunner`/`ScipStep`
//! (`testing::runner::FakeRunner`).

use std::sync::Arc;
use std::time::Duration;

use mage_core::domain::{
    AbortReason, GateFeedback, GateKind, GateOutcome, HarnessKind, RunCommand, RunId, RunKind, RunStatus, VerifyOutcome,
};
use mage_core::ports::{Clock, SystemClock};
use mage_core::wire::mcp::KNOWLEDGE_TOOLS;
use mage_core::wire::runner::{DownloadRef, FinishOutcome, GateTicket, RunAssignment, RunnerPhase, SubmitAck, WireRunResult};
use serde_json::json;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::app::runner::tools::RunTools;
use crate::ports::executor::{Env, EgressProfile, ExecError, ForkPurpose};
use crate::ports::harness::{BuiltinToolPolicy, ExitReason, HarnessEvent, HarnessExit, SessionInfo, TurnOutcome, UsageDelta};
use crate::ports::runner_state::RunnerState;
use crate::testing::harness::{FakeCall, Step, SteerMatch};
use crate::testing::link::{HeartbeatScript, LinkCall};
use crate::testing::runner::FakeRunner;

const WAIT: Duration = Duration::from_secs(10);

struct World {
    fake: FakeRunner,
    _dir: tempfile::TempDir,
}

impl std::ops::Deref for World {
    type Target = FakeRunner;

    fn deref(&self) -> &FakeRunner {
        &self.fake
    }
}

fn world(gated: bool) -> World {
    let dir = tempfile::tempdir().unwrap();
    let fake = if gated { FakeRunner::gated(dir.path()) } else { FakeRunner::new(dir.path()) };
    World { fake, _dir: dir }
}

fn assignment(world: &World, kind: RunKind) -> RunAssignment {
    world.assignment(kind)
}

fn ready() -> Step {
    Step::Emit(HarnessEvent::Ready(SessionInfo {
        harness: HarnessKind::Omp,
        version: "18.3.1".into(),
        native_session_id: "sess-1".into(),
        model: "claude-sonnet-4-5".into(),
        tools: Vec::new(),
        pid: 4242,
        pgid: 4242,
    }))
}

fn exit(reason: ExitReason) -> HarnessExit {
    HarnessExit {
        reason,
        exit_code: Some(1),
        signal: None,
        stderr_tail: "boom".into(),
        stderr_log: None,
        transcript: None,
        usage: UsageDelta::default(),
        wall: Duration::from_secs(1),
    }
}

fn spawn(world: &World, a: RunAssignment) -> tokio::task::JoinHandle<()> {
    tokio::spawn(run_assignment(world.deps.clone(), a, Instant::now(), CancellationToken::new()))
}

async fn eventually(what: &str, cond: impl Fn() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn tools(world: &World) -> Arc<RunTools> {
    eventually("Run MCP started", || !world.mcp.started().is_empty()).await;
    world.mcp.started()[0].clone()
}

fn heartbeat_phase(run: RunId, phase: RunnerPhase) -> impl Fn(&[LinkCall]) -> bool {
    move |calls| calls.iter().any(|c| matches!(c, LinkCall::Heartbeat { run: r, phase: p, .. } if *r == run && *p == phase))
}

async fn finished(world: &World, handle: tokio::task::JoinHandle<()>) -> FinishOutcome {
    match tokio::time::timeout(WAIT, handle).await {
        Ok(joined) => joined.expect("run task does not panic"),
        Err(_) => panic!("run task did not end; link calls: {:#?}\nharness calls: {:#?}", world.link.calls(), world.harness.calls()),
    }
    let finishes = world.link.finishes();
    assert_eq!(finishes.len(), 1, "exactly one finish: {finishes:?}");
    assert!(world.state.active_runs().await.unwrap().is_empty(), "end_run was called");
    finishes[0].2.outcome.clone()
}

fn gate_ack(kind: GateKind, attempt: u8) -> SubmitAck {
    SubmitAck {
        status: RunStatus::AwaitingGate,
        gate: Some(GateTicket { gate_id: uuid::Uuid::now_v7().into(), kind, attempt }),
        message: format!("Submitted. {kind:?} gate attempt {attempt} is running in a clean fork. End your turn now."),
    }
}

fn gate_feedback(kind: GateKind, attempt: u8, outcome: GateOutcome) -> RunCommand {
    RunCommand::GateFeedback(GateFeedback { gate_id: uuid::Uuid::now_v7().into(), kind, attempt, outcome })
}

fn inconclusive() -> serde_json::Value {
    json!({"verdict": "inconclusive", "summary": "The checkout page 500s.", "reason": "seed data missing"})
}

async fn begin(world: &World, a: &RunAssignment) {
    world
        .state
        .begin_run(crate::ports::runner_state::ActiveRun {
            run_id: a.run_id,
            org_id: a.org_id,
            kind: a.kind,
            attempt: a.attempt,
            phase: crate::ports::runner_state::RunPhase::Leased,
            env: None,
            started_at: SystemClock.now(),
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn diagnose_happy_path_through_a_passing_repro_gate() {
    let w = world(false);
    let a = assignment(&w, RunKind::Diagnose);
    begin(&w, &a).await;
    w.harness.push_script(vec![
        ready(),
        Step::Emit(HarnessEvent::TurnStarted { turn: 1 }),
        Step::Emit(HarnessEvent::AssistantText { turn: 1, text: "Reading mgk_knowledge-token-0123456789 aloud".into() }),
    ]);
    let handle = spawn(&w, a.clone());
    let tools = tools(&w).await;
    w.link.wait_for(WAIT, heartbeat_phase(a.run_id, RunnerPhase::Running)).await;

    w.link.script_submit(Ok(gate_ack(GateKind::Repro, 1)));
    let response = tools.call("submit_diagnosis", inconclusive(), CancellationToken::new()).await.unwrap();
    assert!(response.ok, "{response:?}");
    assert!(response.text.contains("End your turn now"), "the ack message is the tool result: {}", response.text);
    w.link.wait_for(WAIT, heartbeat_phase(a.run_id, RunnerPhase::AwaitingGate)).await;

    w.link.set_status(a.run_id, RunStatus::Succeeded);
    w.link.push_command(a.run_id, 1, gate_feedback(GateKind::Repro, 1, GateOutcome::Passed));
    assert_eq!(finished(&w, handle).await, FinishOutcome::Done);

    let spec = &w.harness.started_specs()[0];
    assert_eq!(spec.builtin_tools, BuiltinToolPolicy::WorkspaceRw);
    assert_eq!(spec.prompt, "Investigate the cause.");
    let servers: Vec<(&str, Vec<String>)> = spec.mcp_servers.iter().map(|s| (s.name.as_str(), s.expected_tools.clone())).collect();
    assert_eq!(servers[0], ("mage_knowledge", KNOWLEDGE_TOOLS.iter().map(|t| (*t).to_owned()).collect()));
    assert_eq!(servers[1], ("mage_run", tools.tool_names().into_iter().map(str::to_owned).collect()));
    assert_eq!(w.harness.calls().last(), Some(&FakeCall::Close(Duration::from_secs(5))), "graceful close, no steer");
    assert!(!w.harness.calls().iter().any(|c| matches!(c, FakeCall::Steer(_))));
    assert_eq!(w.executor.forks().iter().map(|f| f.purpose).collect::<Vec<_>>(), vec![ForkPurpose::Session]);
    assert!(w.executor.envs().iter().all(|e| e.destroyed()), "env destroyed");
    assert_eq!(w.mcp.shutdowns(), 1);

    let phases: Vec<RunnerPhase> = w
        .link
        .calls()
        .iter()
        .filter_map(|c| match c {
            LinkCall::Heartbeat { phase, .. } => Some(*phase),
            _ => None,
        })
        .collect();
    let mut seen = phases;
    seen.dedup();
    assert_eq!(&seen[..3], &[RunnerPhase::Preparing, RunnerPhase::Running, RunnerPhase::AwaitingGate], "{seen:?}");
    assert!(seen[3..].iter().all(|p| *p == RunnerPhase::Finishing), "only Finishing after the gate: {seen:?}");
    let events = w.link.events(a.run_id);
    assert!(events.iter().any(|e| matches!(e, OutboundEventKind::Notice(t) if t.starts_with("harness ready"))), "{events:?}");
    assert!(
        events.iter().any(|e| matches!(e, OutboundEventKind::AssistantText(t) if t == "Reading [redacted] aloud")),
        "tokens are scrubbed: {events:?}"
    );
}

#[tokio::test]
async fn failed_gate_steers_the_feedback_and_the_resubmission_passes() {
    let w = world(false);
    let a = assignment(&w, RunKind::Diagnose);
    begin(&w, &a).await;
    w.harness.push_script(vec![
        ready(),
        Step::Emit(HarnessEvent::TurnStarted { turn: 1 }),
        Step::AwaitSteer(SteerMatch::Contains("signature mismatch".into())),
        Step::Emit(HarnessEvent::TurnStarted { turn: 2 }),
    ]);
    let handle = spawn(&w, a.clone());
    let tools = tools(&w).await;
    w.link.wait_for(WAIT, heartbeat_phase(a.run_id, RunnerPhase::Running)).await;

    w.link.script_submit(Ok(gate_ack(GateKind::Repro, 1)));
    assert!(tools.call("submit_diagnosis", inconclusive(), CancellationToken::new()).await.unwrap().ok);
    w.link.wait_for(WAIT, heartbeat_phase(a.run_id, RunnerPhase::AwaitingGate)).await;
    let refused = tools.call("submit_diagnosis", inconclusive(), CancellationToken::new()).await.unwrap();
    assert!(!refused.ok && refused.text.contains("gate in progress"), "{refused:?}");

    w.link.set_status(a.run_id, RunStatus::Running);
    let failed = GateOutcome::Failed { reason: "signature mismatch".into(), details: "expected 500, observed 200".into(), attempts_left: 1 };
    w.link.push_command(a.run_id, 1, gate_feedback(GateKind::Repro, 1, failed));
    eventually("feedback steered", || w.harness.calls().iter().any(|c| matches!(c, FakeCall::Steer(_)))).await;
    let steer = w.harness.calls().into_iter().find_map(|c| if let FakeCall::Steer(t) = c { Some(t) } else { None }).unwrap();
    assert!(steer.contains("signature mismatch") && steer.contains("expected 500, observed 200"), "{steer}");
    assert!(steer.contains("Gate attempts left: 1") && steer.contains("submit_diagnosis"), "{steer}");

    w.link.script_submit(Ok(gate_ack(GateKind::Repro, 2)));
    assert!(tools.call("submit_diagnosis", inconclusive(), CancellationToken::new()).await.unwrap().ok, "submits work again");
    w.link.set_status(a.run_id, RunStatus::Succeeded);
    w.link.push_command(a.run_id, 1, gate_feedback(GateKind::Repro, 2, GateOutcome::Passed));
    assert_eq!(finished(&w, handle).await, FinishOutcome::Done);

    let submissions: Vec<uuid::Uuid> = w
        .link
        .calls()
        .iter()
        .filter_map(|c| match c {
            LinkCall::SubmitResult { submission_id, .. } => Some(*submission_id),
            _ => None,
        })
        .collect();
    assert_eq!(submissions.len(), 2, "the refused submit never reached the server");
    assert_ne!(submissions[0], submissions[1]);
}

#[tokio::test]
async fn abort_command_aborts_closes_destroys_then_finishes_done() {
    let w = world(false);
    let a = assignment(&w, RunKind::Diagnose);
    begin(&w, &a).await;
    w.harness.push_script(vec![
        ready(),
        Step::Emit(HarnessEvent::TurnStarted { turn: 1 }),
        Step::AwaitAbort,
        Step::Emit(HarnessEvent::TurnEnded { turn: 1, outcome: TurnOutcome::Aborted }),
        Step::Emit(HarnessEvent::Idle),
    ]);
    let handle = spawn(&w, a.clone());
    w.link.wait_for(WAIT, heartbeat_phase(a.run_id, RunnerPhase::Running)).await;
    w.link.set_status(a.run_id, RunStatus::Cancelled);
    w.link.push_command(a.run_id, 1, RunCommand::Abort { reason: AbortReason::Cancelled { by: None } });
    assert_eq!(finished(&w, handle).await, FinishOutcome::Done);
    let calls: Vec<FakeCall> = w.harness.calls().into_iter().filter(|c| !matches!(c, FakeCall::Start(_))).collect();
    assert_eq!(calls, vec![FakeCall::Abort, FakeCall::Close(Duration::from_secs(5))]);
    assert!(w.executor.envs()[0].destroyed());
    assert!(matches!(w.link.calls().last(), Some(LinkCall::Finish { .. })), "finish is the last call");
}

#[tokio::test]
async fn fix_session_places_the_pinned_script_and_ends_when_the_budget_is_exceeded() {
    let w = world(false);
    let a = assignment(&w, RunKind::Fix);
    begin(&w, &a).await;
    w.harness.push_script(vec![ready(), Step::Emit(HarnessEvent::TurnStarted { turn: 1 })]);
    let mut a = a;
    let script = w.link.add_blob(DownloadRef::Artifact(uuid::Uuid::now_v7().into()), Some("repro.spec.ts"), b"test('x', () => {})".to_vec());
    let DownloadRef::Artifact(script_id) = script.what else { unreachable!() };
    a.downloads.push(script.clone());
    a.fix = Some(mage_core::wire::runner::FixSpec {
        brief: uuid::Uuid::now_v7().into(),
        script: script_id,
        script_sha256: script.sha256,
        signature: mage_core::domain::FailureSignature {
            kind: mage_core::domain::SignatureKind::AssertionText,
            value: "boom".into(),
            matcher: mage_core::domain::Matcher::Contains,
        },
        policy: mage_core::domain::fix::PatchPolicy {
            allow_workflow_edits: false,
            lockfiles: mage_core::domain::fix::LockfilePolicy::Exclude,
            generated: Vec::new(),
            limits: mage_core::domain::fix::PatchLimits::default(),
        },
        regression: mage_core::domain::fix::RegressionPlacement::Off,
    });
    let handle = spawn(&w, a.clone());
    w.link.wait_for(WAIT, heartbeat_phase(a.run_id, RunnerPhase::Running)).await;
    let env = w.executor.envs()[0].clone();
    let placed = crate::ports::executor::EnvPath::ws(".mage/repro/repro.spec.ts").unwrap();
    assert_eq!(env.file(&placed), Some(b"test('x', () => {})".to_vec()), "pinned repro script placed in the fork");

    // The server applied BudgetExceeded(Cost); the next heartbeat reports it.
    w.link.set_status(a.run_id, RunStatus::TimedOut);
    assert_eq!(finished(&w, handle).await, FinishOutcome::Done);
    assert!(env.destroyed());
}

#[tokio::test]
async fn lease_lost_self_fences_without_uploads() {
    let w = world(false);
    let a = assignment(&w, RunKind::Diagnose);
    begin(&w, &a).await;
    w.harness.push_script(vec![ready(), Step::Emit(HarnessEvent::TurnStarted { turn: 1 })]);
    let handle = spawn(&w, a.clone());
    w.link.wait_for(WAIT, heartbeat_phase(a.run_id, RunnerPhase::Running)).await;
    w.link.script_heartbeat(HeartbeatScript::Fail(LinkError::Conflict(LinkConflict::LeaseLost)));
    assert_eq!(finished(&w, handle).await, FinishOutcome::Abandoned);
    assert_eq!(w.harness.calls().last(), Some(&FakeCall::Close(Duration::ZERO)), "hard kill");
    assert!(w.executor.envs()[0].destroyed());
    assert!(!w.link.calls().iter().any(|c| matches!(c, LinkCall::RequestUpload { .. })), "no uploads after self-fence");
}

#[tokio::test]
async fn harness_crash_while_awaiting_the_gate_finishes_abandoned() {
    let w = world(true);
    let a = assignment(&w, RunKind::Diagnose);
    begin(&w, &a).await;
    w.harness.push_script(vec![
        ready(),
        Step::Emit(HarnessEvent::TurnStarted { turn: 1 }),
        Step::Emit(HarnessEvent::TurnEnded { turn: 1, outcome: TurnOutcome::Completed }),
        Step::Emit(HarnessEvent::Idle),
        Step::Exit(exit(ExitReason::Crashed)),
    ]);
    let handle = spawn(&w, a.clone());
    let tools = tools(&w).await;
    w.link.script_submit(Ok(gate_ack(GateKind::Repro, 1)));
    assert!(tools.call("submit_diagnosis", inconclusive(), CancellationToken::new()).await.unwrap().ok);
    w.start_gate.add_permits(1);
    let outcome = finished(&w, handle).await;
    assert_eq!(outcome, FinishOutcome::Abandoned, "the server keeps the gate running (session_lost)");
    assert!(!w.harness.calls().iter().any(|c| matches!(c, FakeCall::Steer(_))), "no nudge while a gate is pending");
    assert!(w.executor.envs()[0].destroyed());
}

#[tokio::test]
async fn turn_without_submit_is_nudged_once_then_finishes_no_result() {
    let w = world(false);
    let a = assignment(&w, RunKind::Diagnose);
    begin(&w, &a).await;
    w.harness.push_script(vec![
        ready(),
        Step::Emit(HarnessEvent::TurnStarted { turn: 1 }),
        Step::Emit(HarnessEvent::TurnEnded { turn: 1, outcome: TurnOutcome::Completed }),
        Step::Emit(HarnessEvent::Idle),
        Step::AwaitSteer(SteerMatch::Contains("submit_diagnosis".into())),
        Step::Emit(HarnessEvent::TurnStarted { turn: 2 }),
        Step::Emit(HarnessEvent::TurnEnded { turn: 2, outcome: TurnOutcome::Completed }),
        Step::Emit(HarnessEvent::Idle),
    ]);
    let handle = spawn(&w, a.clone());
    assert_eq!(finished(&w, handle).await, FinishOutcome::NoResult);
    let steers: Vec<FakeCall> = w.harness.calls().into_iter().filter(|c| matches!(c, FakeCall::Steer(_))).collect();
    assert_eq!(steers.len(), 1, "nudge_limit = 1");
}

#[tokio::test]
async fn triage_runs_in_an_empty_env_and_ends_after_the_accepted_submission() {
    let w = world(false);
    let a = assignment(&w, RunKind::Triage);
    begin(&w, &a).await;
    w.harness.push_script(vec![
        ready(),
        Step::Emit(HarnessEvent::TurnStarted { turn: 1 }),
        Step::AwaitSteer(SteerMatch::Contains("wrap up".into())),
        Step::Emit(HarnessEvent::TurnEnded { turn: 1, outcome: TurnOutcome::Completed }),
        Step::Emit(HarnessEvent::Idle),
    ]);
    let handle = spawn(&w, a.clone());
    let tools = tools(&w).await;
    w.link.wait_for(WAIT, heartbeat_phase(a.run_id, RunnerPhase::Running)).await;
    let submission = json!({"summary": "Checkout fails for EU customers.", "causes": [], "missing_info": ["Which browser?"]});
    let response = tools.call("submit_triage", submission, CancellationToken::new()).await.unwrap();
    assert!(response.ok, "{response:?}");
    // The agent closes its turn.
    w.link.push_command(a.run_id, 1, RunCommand::Steer { text: "wrap up".into(), by: uuid::Uuid::now_v7().into() });
    assert_eq!(finished(&w, handle).await, FinishOutcome::Done);

    assert_eq!(w.executor.prepare_count(), 0);
    let forks = w.executor.forks();
    assert_eq!(forks.len(), 1);
    assert_eq!((forks[0].purpose, forks[0].egress), (ForkPurpose::Triage, EgressProfile::ServerOnly));
    let spec = &w.harness.started_specs()[0];
    assert_eq!(spec.builtin_tools, BuiltinToolPolicy::None);
    assert!(spec.cwd.is_none());
    assert_eq!(spec.mcp_servers[1].expected_tools, vec!["submit_triage".to_owned()]);
    assert!(w.link.calls().iter().any(|c| matches!(c, LinkCall::SubmitResult { result: WireRunResult::Triage(_), .. })));
}

#[tokio::test]
async fn verify_boot_runs_without_a_harness_and_submits_its_result() {
    let w = world(false);
    let a = assignment(&w, RunKind::Verify);
    begin(&w, &a).await;
    let handle = spawn(&w, a.clone());
    assert_eq!(finished(&w, handle).await, FinishOutcome::Done);
    assert!(w.harness.calls().is_empty(), "no harness for Verify");
    assert!(w.mcp.started().is_empty(), "no Run MCP for Verify");
    let calls = w.link.calls();
    let preparing = calls.iter().position(|c| matches!(c, LinkCall::Heartbeat { phase: RunnerPhase::Preparing, .. })).unwrap();
    let running = calls.iter().position(|c| matches!(c, LinkCall::Heartbeat { phase: RunnerPhase::Running, .. })).unwrap();
    let submit = calls
        .iter()
        .position(|c| matches!(c, LinkCall::SubmitResult { result: WireRunResult::Verify(r), .. } if r.outcome == VerifyOutcome::Completed))
        .expect("Verify result submitted");
    assert!(preparing < running && running < submit);
    assert!(w.executor.envs().iter().all(|e| e.destroyed()));
}

#[tokio::test]
async fn verify_prepare_failure_from_the_recipe_is_boot_failed() {
    let w = world(false);
    let a = assignment(&w, RunKind::Verify);
    begin(&w, &a).await;
    w.executor.fail_next_prepare(ExecError::PrepareFailed { step: "migrate".into(), exit: None, log_tail: "relation missing".into() });
    let handle = spawn(&w, a.clone());
    assert_eq!(finished(&w, handle).await, FinishOutcome::Done);
    let outcome = w.link.calls().into_iter().find_map(|c| match c {
        LinkCall::SubmitResult { result: WireRunResult::Verify(r), .. } => Some(r.outcome),
        _ => None,
    });
    assert!(matches!(outcome, Some(VerifyOutcome::BootFailed { .. })), "{outcome:?}");
}

#[tokio::test]
async fn expired_bundle_url_is_refreshed_once_before_prepare_is_retried() {
    let w = world(false);
    let a = assignment(&w, RunKind::Verify);
    begin(&w, &a).await;
    w.executor.fail_next_prepare(ExecError::SourceUrlExpired("403".into()));
    let handle = spawn(&w, a.clone());
    assert_eq!(finished(&w, handle).await, FinishOutcome::Done);
    let refreshes: Vec<LinkCall> = w.link.calls().into_iter().filter(|c| matches!(c, LinkCall::RefreshDownloads { .. })).collect();
    assert_eq!(refreshes.len(), 1);
}

#[tokio::test]
async fn startup_recovery_reports_held_runs_then_ends_them() {
    let w = world(false);
    let held = [assignment(&w, RunKind::Diagnose), assignment(&w, RunKind::Triage)];
    for a in &held {
        begin(&w, a).await;
    }
    let base = w
        .executor
        .prepare(
            crate::ports::executor::PrepareRequest {
                org_id: held[0].org_id,
                repo_id: held[0].repo.as_ref().unwrap().repo_id,
                sha: held[0].sha.unwrap(),
                recipe: None,
                source_bundle: crate::ports::executor::BundleSource { url: "fake://bundle".into(), sha256: [0; 32], bytes: 0 },
            },
            &crate::ports::executor::NullObserver,
        )
        .await
        .unwrap();
    let leftover = w
        .executor
        .fork(&base, crate::ports::executor::ForkOptions { run_id: held[0].run_id, purpose: ForkPurpose::Session, egress: EgressProfile::Registries })
        .await
        .unwrap();
    let leftover_dir = w.deps.config.data_dir.join("runs").join(held[1].run_id.to_string()).join("outbox");
    std::fs::create_dir_all(&leftover_dir).unwrap();

    let daemon = crate::app::runner::daemon::Daemon::new(w.deps.clone(), crate::app::runner::daemon::DaemonConfig::default());
    daemon.start().await.unwrap();

    let registered = w.link.calls().into_iter().find_map(|c| match c {
        LinkCall::Register { held, .. } => Some(held),
        _ => None,
    });
    let mut reported: Vec<RunId> = registered.expect("registered").iter().map(|h| h.run_id).collect();
    reported.sort();
    let mut expected: Vec<RunId> = held.iter().map(|a| a.run_id).collect();
    expected.sort();
    assert_eq!(reported, expected);
    assert!(w.state.active_runs().await.unwrap().is_empty(), "every held run ended");
    assert!(w.executor.envs().iter().any(|e| e.id() == leftover.id() && e.destroyed()), "leftover fork destroyed");
    assert!(!leftover_dir.exists(), "run dir deleted");
}

#[tokio::test]
async fn drain_abandons_runs_still_going_after_the_grace_period() {
    let w = world(false);
    w.harness.push_script(vec![ready(), Step::Emit(HarnessEvent::TurnStarted { turn: 1 })]);
    let daemon = crate::app::runner::daemon::Daemon::new(w.deps.clone(), crate::app::runner::daemon::DaemonConfig::default());
    let a = assignment(&w, RunKind::Diagnose);
    daemon.accept(a.clone(), Instant::now());
    w.link.wait_for(WAIT, heartbeat_phase(a.run_id, RunnerPhase::Running)).await;
    tokio::time::timeout(WAIT, daemon.drain(Duration::from_millis(100))).await.expect("drain ends");
    assert_eq!(daemon.running(), 0);
    let finishes = w.link.finishes();
    assert_eq!(finishes.len(), 1);
    assert_eq!(finishes[0].2.outcome, FinishOutcome::Abandoned, "requeued through LeaseLost");
    let calls: Vec<FakeCall> = w.harness.calls().into_iter().filter(|c| !matches!(c, FakeCall::Start(_))).collect();
    assert_eq!(calls, vec![FakeCall::Abort, FakeCall::Close(Duration::ZERO)]);
    assert!(w.executor.envs()[0].destroyed());
    assert!(w.state.active_runs().await.unwrap().is_empty());
}
