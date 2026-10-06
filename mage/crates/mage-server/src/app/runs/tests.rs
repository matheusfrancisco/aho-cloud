//! Run manager use cases over fakes (runs.md "Testing").

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use uuid::Uuid;

use mage_core::domain::{
    AbortReason, ArtifactId, ArtifactKind, AttemptKind, Budget, BudgetLimit, CauseStatus, DiagnoseResult, FailureSignature, GateKind,
    GateOutcome, GateStatus, JobKind, JobPayload, Matcher, Notice, OrgId, RunCommand, RunErrorCode, RunEventKind, RunId, RunKind, RunStatus,
    RunSubject, SignatureKind, TicketStatus, TriageResult, Usage, UserId, Verdict, VerifyAttempt, VerifyOutcome, VerifyResult, VerifySpec,
};
use mage_core::ports::jobs::{JobQueue, LeaseRequest as JobLeaseRequest};
use mage_core::wire::runner::{
    ArtifactSpec, CommandsQuery, EventsRequest, FinishOutcome, FinishReport, FinishRequest, HeartbeatRequest, LeaseRequest, OutboundEvent,
    OutboundEventKind, Rejection, RejectionItem, RunAssignment, RunnerPhase, SlotCounts, SubmitResultRequest, TriageSubmission, UploadRequest,
    WireRunResult,
};

use crate::app::runs::{CancelRun, CreateRun, EvidenceDecision, FailRun, ProtocolError, RecordUsage, RunnerCaller, SteerRun, UsageVerdict};
use crate::error::AppError;
use crate::ports::runs::{RunQuery, RunRecord, RunStore};
use crate::ports::ticket::TicketStore;
use crate::ports::tokens::RunnerScope;
use crate::testing::runs_harness::{RunsHarness, SeededRepo, SeededTicket};

const FREE: SlotCounts = SlotCounts { light: 1, work: 1, verify: 1 };
const VERIFY_ONLY: SlotCounts = SlotCounts { light: 0, work: 0, verify: 1 };

struct World {
    h: RunsHarness,
    org: OrgId,
    repo: SeededRepo,
    ticket: SeededTicket,
}

async fn world() -> World {
    let h = RunsHarness::new();
    let org = h.org();
    let repo = h.repo(org).await;
    let ticket = h.ticket(org, &repo, 2).await;
    World { h, org, repo, ticket }
}

fn sig() -> FailureSignature {
    FailureSignature { kind: SignatureKind::AssertionText, value: "expected 10 got 20".into(), matcher: Matcher::Contains }
}

impl World {
    async fn diagnose(&self, cause: usize) -> RunRecord {
        self.h.manager.start_diagnose(self.org, self.ticket.ticket, self.ticket.causes[cause], None, UserId::from_uuid(Uuid::nil())).await.unwrap()
    }

    async fn runner(&self, name: &str) -> RunnerCaller {
        self.h.runner(name, RunnerScope::Shared).await.0
    }

    async fn lease(&self, caller: &RunnerCaller, free: SlotCounts) -> Option<RunAssignment> {
        let req = LeaseRequest { request_id: Uuid::now_v7(), wait: Duration::ZERO, free, prepared: Vec::new() };
        self.h.manager.lease(caller, req).await.unwrap()
    }

    async fn start(&self, caller: &RunnerCaller, run: RunId, attempt: u16) {
        for phase in [RunnerPhase::Preparing, RunnerPhase::Running] {
            self.h.manager.heartbeat(caller, run, HeartbeatRequest { attempt, phase }).await.unwrap();
        }
    }

    async fn upload(&self, caller: &RunnerCaller, run: RunId, attempt: u16, kind: ArtifactKind, name: &str, body: &[u8]) -> ArtifactId {
        let spec = ArtifactSpec { kind, name: name.into(), bytes: body.len() as u64, sha256: crate::app::runs::request_hash(body), content_type: "text/plain".into() };
        let grant = self.h.manager.request_upload(caller, run, UploadRequest { attempt, spec }).await.unwrap();
        self.h.artifacts.upload_signed(&grant.url, &grant.headers, body.to_vec()).unwrap();
        self.h.manager.complete_upload(caller, run, grant.artifact_id, attempt).await.unwrap();
        grant.artifact_id
    }

    async fn submit(&self, caller: &RunnerCaller, run: RunId, attempt: u16, result: WireRunResult) -> Result<mage_core::wire::runner::SubmitAck, ProtocolError> {
        self.submit_as(caller, run, attempt, Uuid::now_v7(), result).await
    }

    async fn submit_as(
        &self,
        caller: &RunnerCaller,
        run: RunId,
        attempt: u16,
        submission_id: Uuid,
        result: WireRunResult,
    ) -> Result<mage_core::wire::runner::SubmitAck, ProtocolError> {
        self.h.manager.submit_result(caller, run, SubmitResultRequest { attempt, submission_id, result }).await
    }

    async fn get(&self, run: RunId) -> RunRecord {
        self.h.runs.get(self.org, run).await.unwrap()
    }

    async fn verify_child(&self, parent: RunId) -> RunRecord {
        let mut kids = self.h.runs.list(self.org, &RunQuery { parent: Some(parent), newest_first: true, ..RunQuery::default() }).await.unwrap();
        kids.remove(0)
    }

    /// Runs every ready `runs.apply_outcome` job like the worker does; returns the errors.
    async fn drain(&self) -> Vec<AppError> {
        let mut errors = Vec::new();
        loop {
            let req = JobLeaseRequest {
                lane: mage_core::domain::JobLane::Server,
                kinds: Some(vec![JobKind::RunsApplyOutcome]),
                worker: "test".into(),
                max: 50,
            };
            let leased = self.h.jobs.lease(&req).await.unwrap();
            if leased.is_empty() {
                return errors;
            }
            for job in leased {
                let JobPayload::RunsApplyOutcome { org_id, run_id } = job.payload else { unreachable!() };
                match self.h.manager.apply_outcome(org_id, run_id).await {
                    Ok(()) => self.h.jobs.complete(&job.lease, &[]).await.unwrap(),
                    Err(e) => {
                        errors.push(e);
                        self.h.jobs.fail(&job.lease, mage_core::ports::jobs::JobFailure::Permanent { error: "test".into() }).await.unwrap();
                    }
                }
            }
        }
    }

    /// A reproduced diagnosis with a freshly uploaded script.
    async fn reproduced(&self, caller: &RunnerCaller, run: RunId, attempt: u16, name: &str) -> WireRunResult {
        let script = self.upload(caller, run, attempt, ArtifactKind::Script, name, b"test('x', () => {})").await;
        WireRunResult::Diagnose(DiagnoseResult {
            verdict: Verdict::Reproduced,
            script: Some(script),
            signature: Some(sig()),
            summary: "reproduced".into(),
            suggested_fix: None,
            fix_sites: Vec::new(),
        })
    }

    /// Runs the Verify child of `parent` on `caller` with `observed` per repetition.
    async fn verify(&self, caller: &RunnerCaller, parent: RunId, observed: bool) -> RunId {
        let a = self.lease(caller, VERIFY_ONLY).await.expect("verify run leased");
        assert_eq!(a.kind, RunKind::Verify);
        assert_eq!(self.get(a.run_id).await.run.parent, Some(parent));
        self.start(caller, a.run_id, a.attempt).await;
        let attempts = (1..=2)
            .map(|i| VerifyAttempt {
                index: i,
                kind: AttemptKind::Script,
                script_exit: Some(1),
                script_passed: false,
                status: Some("failed".into()),
                observed: if observed { vec![FailureSignature { matcher: Matcher::Exact, ..sig() }] } else { Vec::new() },
                health_ok: true,
                duration_ms: 10,
            })
            .collect();
        let result = VerifyResult { outcome: VerifyOutcome::Completed, observed: None, attempts, artifacts: Vec::new(), fix_checks: None, rebased_patch: None };
        let ack = self.submit(caller, a.run_id, a.attempt, WireRunResult::Verify(result)).await.unwrap();
        assert_eq!(ack.status, RunStatus::Succeeded);
        self.finish(caller, a.run_id, a.attempt, FinishOutcome::Done).await.unwrap();
        a.run_id
    }

    async fn finish(&self, caller: &RunnerCaller, run: RunId, attempt: u16, outcome: FinishOutcome) -> Result<RunStatus, ProtocolError> {
        let req = FinishRequest { attempt, report: FinishReport { outcome, message: None } };
        self.h.manager.finish_run(caller, run, req).await.map(|a| a.status)
    }

    async fn commands(&self, caller: &RunnerCaller, run: RunId, attempt: u16) -> Vec<RunCommand> {
        let q = CommandsQuery { attempt, after: 0, wait: Duration::ZERO };
        self.h.manager.poll_commands(caller, run, q).await.unwrap().commands.into_iter().map(|c| c.command).collect()
    }

    async fn cause(&self, i: usize) -> CauseStatus {
        self.h.tickets.get_cause(self.org, self.ticket.causes[i]).await.unwrap().status.clone()
    }

    async fn ticket_status(&self) -> TicketStatus {
        self.h.tickets.get_ticket(self.org, self.ticket.ticket).await.unwrap().value.status
    }

    fn jobs_of(&self, kind: JobKind) -> Vec<JobPayload> {
        self.h.jobs.jobs().into_iter().filter(|j| j.kind == kind).map(|j| j.payload).collect()
    }
}

// ---------------------------------------------------------------------------------------------
// Repro gate paths
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn diagnose_happy_path_through_the_repro_gate() {
    let w = world().await;
    let run = w.diagnose(0).await;
    assert_eq!(run.run.status, RunStatus::Queued);
    assert_eq!(run.spec.recipe_hash, Some(w.repo.recipe));
    assert!(run.source_ready);
    w.drain().await;
    assert_eq!(w.cause(0).await, CauseStatus::Running(run.id()));
    assert_eq!(w.ticket_status().await, TicketStatus::Diagnosing);

    let r = w.runner("r1").await;
    let a = w.lease(&r, FREE).await.unwrap();
    assert_eq!((a.run_id, a.attempt, a.kind), (run.id(), 1, RunKind::Diagnose));
    assert!(a.source.is_some() && a.recipe.is_some() && a.knowledge.is_some() && a.gateway.is_some());
    assert!(a.prior_feedback.is_empty());
    w.start(&r, a.run_id, 1).await;
    assert_eq!(w.get(run.id()).await.run.status, RunStatus::Running);

    let result = w.reproduced(&r, run.id(), 1, "repro.spec.ts").await;
    let ack = w.submit(&r, run.id(), 1, result).await.unwrap();
    assert_eq!(ack.status, RunStatus::AwaitingGate);
    let gate = ack.gate.unwrap();
    assert_eq!((gate.kind, gate.attempt), (GateKind::Repro, 1));
    let child = w.verify_child(run.id()).await;
    assert_eq!(child.spec.verify.as_ref().map(|v| matches!(v, VerifySpec::Repro { repetitions: 2, .. })), Some(true));
    assert!(child.spec.requirements.browser, "a Playwright script needs a browser");
    assert_eq!(child.affinity.map(|a| a.runner_id), Some(r.id()));

    w.verify(&r, run.id(), true).await;
    w.drain().await;
    let done = w.get(run.id()).await;
    assert_eq!(done.run.status, RunStatus::Succeeded);
    let cmds = w.commands(&r, run.id(), 1).await;
    assert!(cmds.iter().any(|c| matches!(c, RunCommand::GateFeedback(f) if f.outcome == GateOutcome::Passed)));
    w.finish(&r, run.id(), 1, FinishOutcome::Done).await.unwrap();
    w.drain().await;
    assert!(matches!(w.cause(0).await, CauseStatus::Reproduced { run: id, .. } if id == run.id()));
    assert_eq!(w.ticket_status().await, TicketStatus::Diagnosed);
    let notices = w.jobs_of(JobKind::NotifyTicket);
    assert!(notices.iter().any(|p| matches!(p, JobPayload::NotifyTicket { notice: Notice::DiagnosisVerdict { run_id, .. }, .. } if *run_id == run.id())));
    assert_eq!(w.jobs_of(JobKind::FixBrief).len(), 1, "fix.brief follows a reproduced cause (org setting on)");
    assert!(w.get(run.id()).await.lease.is_none(), "finish released the lease");
}

#[tokio::test]
async fn failed_gate_sends_feedback_and_the_second_submission_passes() {
    let w = world().await;
    let run = w.diagnose(0).await;
    let r = w.runner("r1").await;
    w.lease(&r, FREE).await.unwrap();
    w.start(&r, run.id(), 1).await;
    let first = w.reproduced(&r, run.id(), 1, "repro-1.spec.ts").await;
    w.submit(&r, run.id(), 1, first).await.unwrap();
    w.verify(&r, run.id(), false).await;
    w.drain().await;
    let rec = w.get(run.id()).await;
    assert_eq!(rec.run.status, RunStatus::Running, "attempts left: back to Running");
    let cmds = w.commands(&r, run.id(), 1).await;
    let feedback = cmds.iter().find_map(|c| match c {
        RunCommand::GateFeedback(f) => Some(f.clone()),
        _ => None,
    });
    let feedback = feedback.expect("GateFeedback command");
    match feedback.outcome {
        GateOutcome::Failed { attempts_left, details, .. } => {
            assert_eq!(attempts_left, 1);
            assert!(details.contains("Expected signature") && details.contains("submit_diagnosis"));
        }
        GateOutcome::Passed => panic!("gate should fail"),
    }
    let gates = w.h.runs.gates(w.org, run.id()).await.unwrap();
    assert!(matches!(gates[0].gate.status, GateStatus::Failed { .. }) && gates[0].feedback.is_some());

    let second = w.reproduced(&r, run.id(), 1, "repro-2.spec.ts").await;
    let ack = w.submit(&r, run.id(), 1, second).await.unwrap();
    assert_eq!(ack.gate.map(|g| g.attempt), Some(2));
    w.verify(&r, run.id(), true).await;
    w.drain().await;
    assert_eq!(w.get(run.id()).await.run.status, RunStatus::Succeeded);
}

#[tokio::test]
async fn exhausted_gate_fails_the_run_and_the_cause_is_inconclusive() {
    let w = world().await;
    let run = w.diagnose(0).await;
    w.drain().await;
    let r = w.runner("r1").await;
    w.lease(&r, FREE).await.unwrap();
    w.start(&r, run.id(), 1).await;
    for i in 1..=2 {
        let res = w.reproduced(&r, run.id(), 1, &format!("repro-{i}.spec.ts")).await;
        w.submit(&r, run.id(), 1, res).await.unwrap();
        w.verify(&r, run.id(), false).await;
        w.drain().await;
    }
    assert_eq!(w.get(run.id()).await.run.status, RunStatus::Failed);
    let last = w.commands(&r, run.id(), 1).await.into_iter().rev().find_map(|c| match c {
        RunCommand::GateFeedback(f) => Some(f),
        _ => None,
    });
    assert!(matches!(last.unwrap().outcome, GateOutcome::Failed { attempts_left: 0, .. }));
    w.finish(&r, run.id(), 1, FinishOutcome::Done).await.unwrap();
    w.drain().await;
    match w.cause(0).await {
        CauseStatus::Inconclusive { run: id, reason } => {
            assert_eq!(id, run.id());
            assert!(reason.starts_with("claimed reproduced; repro gate failed"));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(w.ticket_status().await, TicketStatus::Triaged, "no cause reproduced");
}

#[tokio::test]
async fn apply_outcome_refuses_to_mark_without_a_rederivable_proof() {
    let w = world().await;
    let run = w.diagnose(0).await;
    w.drain().await;
    let r = w.runner("r1").await;
    w.lease(&r, FREE).await.unwrap();
    w.start(&r, run.id(), 1).await;
    let res = w.reproduced(&r, run.id(), 1, "repro.spec.ts").await;
    w.submit(&r, run.id(), 1, res).await.unwrap();
    let verify = w.verify(&r, run.id(), true).await;
    w.h.manager.apply_outcome(w.org, verify).await.unwrap();
    assert_eq!(w.get(run.id()).await.run.status, RunStatus::Succeeded, "the gate passed");
    // The stored diagnosis changes afterwards: its signature no longer matches the gate spec.
    let mut rec = w.get(run.id()).await;
    let Some(mage_core::domain::RunResult::Diagnose(mut d)) = rec.run.result.clone() else { unreachable!() };
    d.signature = Some(FailureSignature { value: "something else".into(), ..sig() });
    let mut data = rec.run.clone().into_data();
    data.result = Some(mage_core::domain::RunResult::Diagnose(d));
    rec.run = mage_core::domain::Run::rehydrate(data);
    w.h.runs.commit(w.org, crate::ports::runs::RunWrite { updates: vec![crate::ports::runs::RunUpdate::of(rec)], ..Default::default() }).await.unwrap();
    let err = w.h.manager.apply_outcome(w.org, run.id()).await.unwrap_err();
    assert!(matches!(&err, AppError::Invalid(m) if m.contains("proof")), "{err}");
    assert_eq!(w.cause(0).await, CauseStatus::Running(run.id()), "nothing is marked without a token");
}

// ---------------------------------------------------------------------------------------------
// Leases, reaper, registration, fencing
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn reaper_requeues_expired_leases_until_max_then_errors() {
    let w = world().await;
    let run = w.diagnose(0).await;
    let r = w.runner("r1").await;
    for attempt in 1..=3u16 {
        let a = w.lease(&r, FREE).await.unwrap();
        assert_eq!(a.attempt, attempt);
        w.h.advance(Duration::from_secs(61));
        let report = w.h.manager.reap_leases(100).await.unwrap();
        assert_eq!(report.handled, 1);
        let rec = w.get(run.id()).await;
        if attempt < 3 {
            assert_eq!((rec.run.status, rec.run.attempt), (RunStatus::Queued, attempt + 1), "LeaseExpiredBeforeStart");
        } else {
            assert_eq!(rec.run.status, RunStatus::Errored, "LeaseLost at the limit");
        }
    }
}

#[tokio::test]
async fn reaper_lease_lost_while_running_and_stale_fence_is_refused() {
    let w = world().await;
    let run = w.diagnose(0).await;
    let r = w.runner("r1").await;
    w.lease(&r, FREE).await.unwrap();
    w.start(&r, run.id(), 1).await;
    w.h.advance(Duration::from_secs(61));
    w.h.manager.reap_leases(100).await.unwrap();
    let rec = w.get(run.id()).await;
    assert_eq!((rec.run.status, rec.run.attempt), (RunStatus::Queued, 2));
    // the old attempt is fenced off everywhere
    let hb = w.h.manager.heartbeat(&r, run.id(), HeartbeatRequest { attempt: 1, phase: RunnerPhase::Running }).await;
    assert!(matches!(hb, Err(ProtocolError::LeaseLost)));
    let ev = w.h.manager.ingest_events(&r, run.id(), EventsRequest { attempt: 1, events: Vec::new() }).await;
    assert!(matches!(ev, Err(ProtocolError::LeaseLost)));
    // and the next lease is attempt 2 for any runner
    let r2 = w.runner("r2").await;
    assert_eq!(w.lease(&r2, FREE).await.unwrap().attempt, 2);
}

#[tokio::test]
async fn register_with_a_new_instance_releases_held_runs_per_status() {
    let w = world().await;
    let a = w.diagnose(0).await;
    let (r, ack) = w.h.runner("r1", RunnerScope::Shared).await;
    w.lease(&r, FREE).await.unwrap();
    w.start(&r, a.id(), 1).await;
    // refresh with the same instance releases nothing
    let token = assertion(&w, &r);
    let same = w.h.manager.register_runner(registration(&w, &r, token, r.instance_id)).await.unwrap();
    assert!(same.released.is_empty());
    assert_eq!(same.runner_id, ack.runner_id);
    // restart: new instance
    let token = assertion(&w, &r);
    let restarted = w.h.manager.register_runner(registration(&w, &r, token, Uuid::now_v7())).await.unwrap();
    assert_eq!(restarted.released.len(), 1);
    assert_eq!(restarted.released[0].run_id, a.id());
    assert_eq!(restarted.released[0].status, RunStatus::Queued, "Running → LeaseLost → requeued");
    assert_eq!(w.get(a.id()).await.run.attempt, 2);
}

fn assertion(w: &World, r: &RunnerCaller) -> String {
    w.h.assertion(&r.runner.name, r.id())
}

fn registration(w: &World, r: &RunnerCaller, assertion: String, instance: Uuid) -> mage_core::wire::runner::Registration {
    mage_core::wire::runner::Registration {
        grant: mage_core::wire::runner::RegisterGrant::Assertion { runner_id: r.id(), assertion },
        instance_id: instance,
        version: "0.1.0".into(),
        capabilities: w.h.capabilities(),
        held: Vec::new(),
    }
}

#[tokio::test]
async fn lease_loss_in_awaiting_gate_then_gate_pass_succeeds() {
    let w = world().await;
    let run = w.diagnose(0).await;
    let r = w.runner("r1").await;
    w.lease(&r, FREE).await.unwrap();
    w.start(&r, run.id(), 1).await;
    let res = w.reproduced(&r, run.id(), 1, "repro.spec.ts").await;
    w.submit(&r, run.id(), 1, res).await.unwrap();
    w.finish(&r, run.id(), 1, FinishOutcome::Abandoned).await.unwrap();
    let rec = w.get(run.id()).await;
    assert!(rec.session_lost && rec.lease.is_none());
    assert_eq!(rec.run.status, RunStatus::AwaitingGate);
    let v = w.runner("v1").await;
    w.h.advance(Duration::from_secs(31));
    w.verify(&v, run.id(), true).await;
    w.drain().await;
    assert_eq!(w.get(run.id()).await.run.status, RunStatus::Succeeded);
}

#[tokio::test]
async fn lease_loss_in_awaiting_gate_then_gate_fail_requeues_with_prior_feedback() {
    let w = world().await;
    let run = w.diagnose(0).await;
    let r = w.runner("r1").await;
    w.lease(&r, FREE).await.unwrap();
    w.start(&r, run.id(), 1).await;
    let res = w.reproduced(&r, run.id(), 1, "repro.spec.ts").await;
    w.submit(&r, run.id(), 1, res).await.unwrap();
    // the runner disappears; the reaper keeps the gate going
    w.h.advance(Duration::from_secs(61));
    w.h.manager.reap_leases(100).await.unwrap();
    let rec = w.get(run.id()).await;
    assert!(rec.session_lost && rec.lease.is_none());
    let v = w.runner("v1").await;
    w.verify(&v, run.id(), false).await;
    w.drain().await;
    let rec = w.get(run.id()).await;
    assert_eq!((rec.run.status, rec.run.attempt), (RunStatus::Queued, 2));
    let next = w.lease(&v, FREE).await.unwrap();
    assert_eq!(next.attempt, 2);
    assert_eq!(next.prior_feedback.len(), 1);
    assert!(next.prior_feedback[0].contains("Repro gate attempt 1 failed"));
}

#[tokio::test]
async fn heartbeat_phases_are_monotonic() {
    let w = world().await;
    let run = w.diagnose(0).await;
    let r = w.runner("r1").await;
    w.lease(&r, FREE).await.unwrap();
    // Running reported directly from Leased applies EnvPreparing then Started
    let ack = w.h.manager.heartbeat(&r, run.id(), HeartbeatRequest { attempt: 1, phase: RunnerPhase::Running }).await.unwrap();
    assert_eq!(ack.status, RunStatus::Running);
    let events = w.h.runs.events_after(w.org, run.id(), 0, 100).await.unwrap();
    let statuses: Vec<RunStatus> = events.iter().filter_map(|e| if let RunEventKind::Status(s) = e.kind { Some(s) } else { None }).collect();
    assert_eq!(statuses, vec![RunStatus::Queued, RunStatus::Leased, RunStatus::Preparing, RunStatus::Running]);
    // a phase behind the status is ignored
    let ack = w.h.manager.heartbeat(&r, run.id(), HeartbeatRequest { attempt: 1, phase: RunnerPhase::Preparing }).await.unwrap();
    assert_eq!(ack.status, RunStatus::Running);
}

// ---------------------------------------------------------------------------------------------
// Budgets
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn reported_usage_over_budget_times_out_with_abort_in_local_login_mode() {
    let mut policy = crate::app::runs::RunPolicy::default();
    policy.credential_mode = mage_core::wire::runner::CredentialModeKind::LocalLogin;
    let h = RunsHarness::with_policy(policy);
    let org = h.org();
    let repo = h.repo(org).await;
    let ticket = h.ticket(org, &repo, 1).await;
    let w = World { h, org, repo, ticket };
    let run = w.diagnose(0).await;
    let r = w.runner("r1").await;
    let a = w.lease(&r, FREE).await.unwrap();
    assert!(a.gateway.is_none(), "no gateway token in LocalLogin mode");
    w.start(&r, run.id(), 1).await;
    let usage = |cost| OutboundEvent { local_seq: cost, at: w.h.now(), kind: OutboundEventKind::Usage(Usage { cost_micros: cost, ..Usage::default() }) };
    let ack = w.h.manager.ingest_events(&r, run.id(), EventsRequest { attempt: 1, events: vec![usage(1_000)] }).await.unwrap();
    assert_eq!(ack.status, RunStatus::Running);
    let over = run.run.budget.max_cost_micros + 1;
    let ack = w.h.manager.ingest_events(&r, run.id(), EventsRequest { attempt: 1, events: vec![usage(over)] }).await.unwrap();
    assert_eq!(ack.status, RunStatus::TimedOut);
    let cmds = w.commands(&r, run.id(), 1).await;
    assert!(cmds.contains(&RunCommand::Abort { reason: AbortReason::Budget { limit: BudgetLimit::Cost } }));
}

#[tokio::test]
async fn gateway_record_usage_denies_over_budget_and_terminal_runs() {
    let w = world().await;
    let run = w.diagnose(0).await;
    let r = w.runner("r1").await;
    w.lease(&r, FREE).await.unwrap();
    w.start(&r, run.id(), 1).await;
    let rec = |cost| RecordUsage { org: w.org, run: run.id(), delta: Usage { cost_micros: cost, turns: 1, ..Usage::default() } };
    assert_eq!(w.h.manager.record_usage(rec(10)).await.unwrap(), UsageVerdict::Allow);
    let verdict = w.h.manager.record_usage(rec(run.run.budget.max_cost_micros)).await.unwrap();
    assert!(matches!(verdict, UsageVerdict::Deny(_)));
    let after = w.get(run.id()).await;
    assert_eq!(after.run.status, RunStatus::TimedOut);
    assert_eq!(after.usage_metered.cost_micros, run.run.budget.max_cost_micros + 10);
    assert!(matches!(w.h.manager.record_usage(rec(1)).await.unwrap(), UsageVerdict::Deny(m) if m.contains("timed_out")));
}

#[tokio::test]
async fn wall_excludes_awaiting_gate_and_spend_during_the_gate_times_out_on_return() {
    let w = world().await;
    let run = w.diagnose(0).await;
    let r = w.runner("r1").await;
    w.lease(&r, FREE).await.unwrap();
    w.start(&r, run.id(), 1).await;
    w.h.advance(Duration::from_secs(60));
    let res = w.reproduced(&r, run.id(), 1, "repro.spec.ts").await;
    w.submit(&r, run.id(), 1, res).await.unwrap();
    // a long gate does not count against max_wall (the runner keeps heartbeating meanwhile)
    let step = Duration::from_secs(30);
    let mut waited = Duration::ZERO;
    // (20 min: below gate_wait_max, which would time the gate out)
    while waited < Duration::from_secs(20 * 60) {
        w.h.advance(step);
        waited += step;
        w.h.manager.heartbeat(&r, run.id(), HeartbeatRequest { attempt: 1, phase: RunnerPhase::AwaitingGate }).await.unwrap();
    }
    let rec = w.get(run.id()).await;
    assert_eq!(rec.wall_at(w.h.now()), Duration::from_secs(60));
    // the harness closes its turn: metered spend during AwaitingGate is recorded, not enforced
    let spend = RecordUsage { org: w.org, run: run.id(), delta: Usage { cost_micros: run.run.budget.max_cost_micros + 1, ..Usage::default() } };
    assert!(matches!(w.h.manager.record_usage(spend).await.unwrap(), UsageVerdict::Deny(_)));
    assert_eq!(w.get(run.id()).await.run.status, RunStatus::AwaitingGate);
    let v = w.runner("v1").await;
    w.verify(&v, run.id(), false).await;
    w.drain().await;
    assert_eq!(w.get(run.id()).await.run.status, RunStatus::TimedOut, "re-checked on the return to Running");
}

#[tokio::test]
async fn heartbeat_and_reaper_apply_the_wall_budget() {
    let w = world().await;
    let run = w.diagnose(0).await;
    let r = w.runner("r1").await;
    w.lease(&r, FREE).await.unwrap();
    w.start(&r, run.id(), 1).await;
    w.h.advance(run.run.budget.max_wall + Duration::from_secs(1));
    let ack = w.h.manager.heartbeat(&r, run.id(), HeartbeatRequest { attempt: 1, phase: RunnerPhase::Running }).await.unwrap();
    assert_eq!(ack.status, RunStatus::TimedOut);
    // a terminal run keeps its lease until finish
    assert!(w.get(run.id()).await.lease.is_some());
    assert_eq!(w.finish(&r, run.id(), 1, FinishOutcome::Done).await.unwrap(), RunStatus::TimedOut);
}

// ---------------------------------------------------------------------------------------------
// Idempotency
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn retried_lease_result_finish_and_event_batch_are_idempotent() {
    let w = world().await;
    let run = w.diagnose(0).await;
    let r = w.runner("r1").await;
    let req = LeaseRequest { request_id: Uuid::now_v7(), wait: Duration::ZERO, free: FREE, prepared: Vec::new() };
    let first = w.h.manager.lease(&r, req.clone()).await.unwrap().unwrap();
    let again = w.h.manager.lease(&r, req).await.unwrap().unwrap();
    assert_eq!((again.run_id, again.attempt), (first.run_id, first.attempt));
    w.start(&r, run.id(), 1).await;
    // duplicate event batch
    let evs = |n: u64| (1..=n).map(|i| OutboundEvent { local_seq: i, at: w.h.now(), kind: OutboundEventKind::Notice(format!("e{i}")) }).collect::<Vec<_>>();
    let a1 = w.h.manager.ingest_events(&r, run.id(), EventsRequest { attempt: 1, events: evs(3) }).await.unwrap();
    let a2 = w.h.manager.ingest_events(&r, run.id(), EventsRequest { attempt: 1, events: evs(3) }).await.unwrap();
    assert_eq!((a1.acked_local_seq, a1.last_seq), (a2.acked_local_seq, a2.last_seq));
    // retried result: same reply, one gate
    let res = w.reproduced(&r, run.id(), 1, "repro.spec.ts").await;
    let sub = Uuid::now_v7();
    let ack1 = w.submit_as(&r, run.id(), 1, sub, res.clone()).await.unwrap();
    let ack2 = w.submit_as(&r, run.id(), 1, sub, res.clone()).await.unwrap();
    assert_eq!(ack1, ack2);
    assert_eq!(w.h.runs.gates(w.org, run.id()).await.unwrap().len(), 1);
    // same key, different body
    let mut other = res;
    if let WireRunResult::Diagnose(d) = &mut other {
        d.summary = "changed".into();
    }
    assert!(matches!(w.submit_as(&r, run.id(), 1, sub, other).await, Err(ProtocolError::IdempotencyMismatch)));
    // finish twice
    w.verify(&r, run.id(), true).await;
    w.drain().await;
    assert_eq!(w.finish(&r, run.id(), 1, FinishOutcome::Done).await.unwrap(), RunStatus::Succeeded);
    assert_eq!(w.finish(&r, run.id(), 1, FinishOutcome::Done).await.unwrap(), RunStatus::Succeeded);
}

// ---------------------------------------------------------------------------------------------
// Scheduling: repo slot, affinity, long-polls
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn one_work_run_per_repo_and_verify_children_still_lease() {
    let w = world().await;
    let a = w.diagnose(0).await;
    let b = w.diagnose(1).await;
    let (r1, r2) = (w.runner("r1").await, w.runner("r2").await);
    let got = w.lease(&r1, FREE).await.unwrap();
    assert_eq!(got.run_id, a.id());
    assert!(w.lease(&r2, FREE).await.is_none(), "repo slot taken");
    assert_eq!(w.get(b.id()).await.run.status, RunStatus::Queued);
    w.start(&r1, a.id(), 1).await;
    let res = w.reproduced(&r1, a.id(), 1, "repro.spec.ts").await;
    w.submit(&r1, a.id(), 1, res).await.unwrap();
    // the parent idles in AwaitingGate holding the slot; its Verify child is leasable
    let v = w.lease(&r1, VERIFY_ONLY).await.unwrap();
    assert_eq!(v.kind, RunKind::Verify);
    assert!(w.lease(&r2, SlotCounts { light: 0, work: 1, verify: 0 }).await.is_none());
    // the first run finishing frees the slot
    w.h.manager.cancel_run(CancelRun { org: w.org, run: a.id(), by: None, notice: None }).await.unwrap();
    w.finish(&r1, a.id(), 1, FinishOutcome::Done).await.unwrap();
    assert_eq!(w.lease(&r2, FREE).await.unwrap().run_id, b.id());
}

#[tokio::test]
async fn verify_affinity_prefers_the_parent_runner_until_the_wait_elapses() {
    let w = world().await;
    let run = w.diagnose(0).await;
    let (r1, r2) = (w.runner("r1").await, w.runner("r2").await);
    w.lease(&r1, FREE).await.unwrap();
    w.start(&r1, run.id(), 1).await;
    let res = w.reproduced(&r1, run.id(), 1, "repro.spec.ts").await;
    w.submit(&r1, run.id(), 1, res).await.unwrap();
    assert!(w.lease(&r2, VERIFY_ONLY).await.is_none(), "held for the parent's runner");
    w.h.advance(Duration::from_secs(31));
    assert_eq!(w.lease(&r2, VERIFY_ONLY).await.map(|a| a.kind), Some(RunKind::Verify));
}

#[tokio::test]
async fn long_polls_wake_on_signals() {
    let w = world().await;
    let r = w.runner("r1").await;
    // lease long-poll wakes when a run becomes leasable
    let mgr = w.h.manager.clone();
    let caller = r.clone();
    let poll = tokio::spawn(async move {
        let req = LeaseRequest { request_id: Uuid::now_v7(), wait: Duration::from_secs(20), free: FREE, prepared: Vec::new() };
        let started = std::time::Instant::now();
        (mgr.lease(&caller, req).await.unwrap(), started.elapsed())
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let run = w.diagnose(0).await;
    let (got, waited) = poll.await.unwrap();
    assert_eq!(got.map(|a| a.run_id), Some(run.id()));
    assert!(waited < Duration::from_secs(10));
    // command long-poll wakes on a steer
    w.start(&r, run.id(), 1).await;
    let mgr = w.h.manager.clone();
    let caller = r.clone();
    let id = run.id();
    let poll = tokio::spawn(async move {
        let started = std::time::Instant::now();
        let batch = mgr.poll_commands(&caller, id, CommandsQuery { attempt: 1, after: 0, wait: Duration::from_secs(20) }).await.unwrap();
        (batch, started.elapsed())
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let user = UserId::from_uuid(Uuid::now_v7());
    w.h.manager.steer_run(SteerRun { org: w.org, run: run.id(), text: "look at coupons".into(), by: user }).await.unwrap();
    let (batch, waited) = poll.await.unwrap();
    assert_eq!(batch.commands.len(), 1);
    assert!(matches!(&batch.commands[0].command, RunCommand::Steer { text, .. } if text == "look at coupons"));
    assert!(waited < Duration::from_secs(10));
    // shutdown ends an idle long-poll at once
    w.h.shutdown.cancel();
    let started = std::time::Instant::now();
    let batch = w.h.manager.poll_commands(&r, run.id(), CommandsQuery { attempt: 1, after: 1, wait: Duration::from_secs(20) }).await.unwrap();
    assert!(batch.commands.is_empty() && started.elapsed() < Duration::from_secs(2));
}

// ---------------------------------------------------------------------------------------------
// Cancel, steer, fail
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn cancel_from_every_non_terminal_state() {
    for target in [RunStatus::Queued, RunStatus::Leased, RunStatus::Preparing, RunStatus::Running, RunStatus::AwaitingGate] {
        let w = world().await;
        let run = w.diagnose(0).await;
        let r = w.runner("r1").await;
        if target != RunStatus::Queued {
            w.lease(&r, FREE).await.unwrap();
        }
        if matches!(target, RunStatus::Preparing | RunStatus::Running | RunStatus::AwaitingGate) {
            w.h.manager.heartbeat(&r, run.id(), HeartbeatRequest { attempt: 1, phase: RunnerPhase::Preparing }).await.unwrap();
        }
        if matches!(target, RunStatus::Running | RunStatus::AwaitingGate) {
            w.h.manager.heartbeat(&r, run.id(), HeartbeatRequest { attempt: 1, phase: RunnerPhase::Running }).await.unwrap();
        }
        if target == RunStatus::AwaitingGate {
            let res = w.reproduced(&r, run.id(), 1, "repro.spec.ts").await;
            w.submit(&r, run.id(), 1, res).await.unwrap();
        }
        assert_eq!(w.get(run.id()).await.run.status, target);
        let user = UserId::from_uuid(Uuid::now_v7());
        let status = w.h.manager.cancel_run(CancelRun { org: w.org, run: run.id(), by: Some(user), notice: None }).await.unwrap();
        assert_eq!(status, RunStatus::Cancelled, "from {target:?}");
        let aborts: Vec<_> = w.h.runs.commands_after(w.org, run.id(), 1, 0, 10).await.unwrap().into_iter().filter(|c| matches!(c.command, RunCommand::Abort { .. })).collect();
        assert_eq!(aborts.len(), usize::from(target != RunStatus::Queued), "Abort only to a leased runner ({target:?})");
        if target == RunStatus::AwaitingGate {
            let child = w.verify_child(run.id()).await;
            assert_eq!(child.run.status, RunStatus::Cancelled, "the Verify child is cancelled");
            let gates = w.h.runs.gates(w.org, run.id()).await.unwrap();
            assert!(matches!(&gates[0].gate.status, GateStatus::Failed { reason, .. } if reason == "parent terminal"));
        }
        // terminal → no-op
        assert_eq!(w.h.manager.cancel_run(CancelRun { org: w.org, run: run.id(), by: Some(user), notice: None }).await.unwrap(), RunStatus::Cancelled);
        w.drain().await;
        assert_eq!(w.cause(0).await, CauseStatus::Untested, "cancelled diagnose restores the previous verdict");
    }
}

#[tokio::test]
async fn steer_only_while_running_and_fail_run_errors_with_abort() {
    let w = world().await;
    let run = w.diagnose(0).await;
    let user = UserId::from_uuid(Uuid::now_v7());
    let steer = |text: &str| SteerRun { org: w.org, run: run.id(), text: text.into(), by: user };
    assert!(matches!(w.h.manager.steer_run(steer("hi")).await, Err(AppError::Conflict(_))));
    let r = w.runner("r1").await;
    w.lease(&r, FREE).await.unwrap();
    w.start(&r, run.id(), 1).await;
    assert!(matches!(w.h.manager.steer_run(steer(&"x".repeat(9000))).await, Err(AppError::Invalid(_))));
    assert_eq!(w.h.manager.steer_run(steer("hi")).await.unwrap(), 1);
    let err = mage_core::domain::RunError { code: RunErrorCode::ModelAuth, message: "tenant key rejected".into() };
    assert_eq!(w.h.manager.fail_run(FailRun { org: w.org, run: run.id(), error: err.clone() }).await.unwrap(), RunStatus::Errored);
    let rec = w.get(run.id()).await;
    assert_eq!(rec.run.error, Some(err.clone()));
    assert!(w.commands(&r, run.id(), 1).await.contains(&RunCommand::Abort { reason: AbortReason::Infra }));
    assert_eq!(w.h.manager.fail_run(FailRun { org: w.org, run: run.id(), error: err }).await.unwrap(), RunStatus::Errored);
}

#[tokio::test]
async fn second_active_diagnose_on_a_cause_conflicts() {
    let w = world().await;
    w.diagnose(0).await;
    let again = w.h.manager.start_diagnose(w.org, w.ticket.ticket, w.ticket.causes[0], None, UserId::from_uuid(Uuid::nil())).await;
    assert!(matches!(again, Err(AppError::Conflict(_))));
}

// ---------------------------------------------------------------------------------------------
// Triage
// ---------------------------------------------------------------------------------------------

fn submission() -> TriageSubmission {
    TriageSubmission { summary: "s".into(), causes: Vec::new(), missing_info: Vec::new() }
}

fn rejection(msg: &str) -> Rejection {
    Rejection { gate: None, message: msg.into(), items: vec![RejectionItem { path: "causes[0]".into(), reason: "bad".into() }] }
}

async fn triage_run(w: &World) -> (RunRecord, RunnerCaller) {
    let ticket = w.h.tickets.get_ticket(w.org, w.ticket.ticket).await.unwrap().value;
    let spec = crate::domain::triage::TriageSpec {
        mode: crate::domain::triage::TriageMode::Live,
        trigger: mage_core::domain::TriageTrigger::Auto,
        input: crate::domain::triage::TriageInput {
            content_hash: ticket.content_hash,
            identifier: "SUP-1".into(),
            title: ticket.title.clone(),
            body: ticket.body.clone(),
            error_strings: Vec::new(),
            target: ticket.target.clone().unwrap(),
            repo: mage_core::wire::runner::RepoRef { repo_id: w.repo.repo.id, owner: "acme".into(), name: "shop".into() },
            customer: None,
            prior_sha: None,
            prior_hypotheses: Vec::new(),
            seeds: None,
            snapshot_at: w.h.now(),
        },
        prompt_version: 1,
        max_causes: 5,
        degraded_layers: Vec::new(),
    };
    let mut req = CreateRun::new(w.org, RunKind::Triage, RunSubject::Ticket(w.ticket.ticket));
    req.triage = Some(spec);
    req.prompt = Some(crate::app::runs::RunPrompt { prompt: "triage".into(), system_append: String::new() });
    req.budget = Some(Budget { max_wall: Duration::from_secs(600), max_turns: 40, max_cost_micros: 1000, max_gate_attempts: 2 });
    req.create_key = Some("job-1".into());
    let run = w.h.manager.create_run(req.clone()).await.unwrap();
    assert_eq!(w.h.manager.create_run(req).await.unwrap().id(), run.id(), "a reused create key returns the run");
    let r = w.runner("t1").await;
    let a = w.lease(&r, SlotCounts { light: 1, work: 0, verify: 0 }).await.unwrap();
    assert!(a.source.is_none() && a.repo.is_none() && a.knowledge.is_some());
    w.start(&r, run.id(), 1).await;
    (run, r)
}

#[tokio::test]
async fn triage_decisions_invalid_reject_accept() {
    let w = world().await;
    let (run, r) = triage_run(&w).await;
    let gate = &w.h.hooks.triage;
    gate.decisions.lock().push_back(EvidenceDecision::Invalid { rejection: rejection("schema") });
    let err = w.submit(&r, run.id(), 1, WireRunResult::Triage(submission())).await.unwrap_err();
    assert!(matches!(&err, ProtocolError::Rejected(rj) if rj.gate.is_none()));
    assert!(w.h.runs.gates(w.org, run.id()).await.unwrap().is_empty(), "Invalid writes no gate row");

    gate.decisions.lock().push_back(EvidenceDecision::Reject {
        details: serde_json::json!({"d": 1}),
        rejection: rejection("fix citations"),
        reason: "1 cause without evidence".into(),
        attempts_left: 1,
    });
    let err = w.submit(&r, run.id(), 1, WireRunResult::Triage(submission())).await.unwrap_err();
    assert!(matches!(&err, ProtocolError::Rejected(rj) if rj.gate.map(|g| g.attempt) == Some(1)));
    assert_eq!(w.get(run.id()).await.run.status, RunStatus::Running);
    let gates = w.h.runs.gates(w.org, run.id()).await.unwrap();
    assert_eq!(gates.len(), 1);
    assert_eq!(gates[0].feedback.as_deref(), Some("fix citations"));

    let result = TriageResult { summary: "ok".into(), causes: Vec::new(), missing_info: Vec::new() };
    gate.decisions.lock().push_back(EvidenceDecision::Accept { result, details: serde_json::json!({}), message: "Accepted 0 causes.".into(), notices: vec!["merged".into()] });
    let ack = w.submit(&r, run.id(), 1, WireRunResult::Triage(submission())).await.unwrap();
    assert_eq!((ack.status, ack.message.as_str()), (RunStatus::Succeeded, "Accepted 0 causes."));
    assert_eq!(gate.calls.lock().iter().map(|(a, _)| *a).collect::<Vec<_>>(), vec![1, 1, 2]);
    w.drain().await;
    assert!(w.h.hooks.triage_outcome.calls.lock().iter().any(|(_, id)| *id == run.id()));
}

#[tokio::test]
async fn triage_fail_ends_failed_via_awaiting_gate_in_one_commit() {
    let w = world().await;
    let (run, r) = triage_run(&w).await;
    w.h.hooks.triage.decisions.lock().push_back(EvidenceDecision::Fail { details: serde_json::json!({}), rejection: rejection("no valid causes"), reason: "zero valid".into() });
    let err = w.submit(&r, run.id(), 1, WireRunResult::Triage(submission())).await.unwrap_err();
    assert!(matches!(err, ProtocolError::Rejected(_)));
    assert_eq!(w.get(run.id()).await.run.status, RunStatus::Failed);
    let statuses: Vec<RunStatus> = w.h.runs.events_after(w.org, run.id(), 0, 100).await.unwrap().iter().filter_map(|e| if let RunEventKind::Status(s) = e.kind { Some(s) } else { None }).collect();
    assert_eq!(&statuses[statuses.len() - 2..], &[RunStatus::AwaitingGate, RunStatus::Failed]);
    assert!(w.commands(&r, run.id(), 1).await.iter().any(|c| matches!(c, RunCommand::GateFeedback(f) if matches!(f.outcome, GateOutcome::Failed { attempts_left: 0, .. }))));
}

#[tokio::test]
async fn triage_salvage_on_budget_and_on_no_result() {
    let w = world().await;
    let (run, r) = triage_run(&w).await;
    *w.h.hooks.triage.salvage.lock() = Some(TriageResult { summary: "salvaged".into(), causes: Vec::new(), missing_info: Vec::new() });
    let over = RecordUsage { org: w.org, run: run.id(), delta: Usage { cost_micros: 5000, ..Usage::default() } };
    w.h.manager.record_usage(over).await.unwrap();
    let rec = w.get(run.id()).await;
    assert_eq!(rec.run.status, RunStatus::Succeeded, "salvage instead of TimedOut");
    assert!(matches!(&rec.run.result, Some(mage_core::domain::RunResult::Triage(t)) if t.summary == "salvaged"));

    // NoResult without a salvage → Errored(NoResult); with one → Succeeded
    let w2 = world().await;
    let (run2, r2) = triage_run(&w2).await;
    assert_eq!(w2.finish(&r2, run2.id(), 1, FinishOutcome::NoResult).await.unwrap(), RunStatus::Errored);
    assert_eq!(w2.get(run2.id()).await.run.error.clone().map(|e| e.code), Some(RunErrorCode::NoResult));
    let _ = r;
}

// ---------------------------------------------------------------------------------------------
// Artifacts, downloads, source readiness
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn uploads_are_validated_completed_once_and_downloads_are_scoped() {
    let w = world().await;
    let run = w.diagnose(0).await;
    let r = w.runner("r1").await;
    w.lease(&r, FREE).await.unwrap();
    let spec = |name: &str, kind| ArtifactSpec { kind, name: name.into(), bytes: 4, sha256: [1; 32], content_type: "text/plain".into() };
    let bad = w.h.manager.request_upload(&r, run.id(), UploadRequest { attempt: 1, spec: spec("../x", ArtifactKind::Log) }).await;
    assert!(matches!(bad, Err(ProtocolError::App(AppError::Invalid(_)))));
    let bad = w.h.manager.request_upload(&r, run.id(), UploadRequest { attempt: 1, spec: spec("b.zst", ArtifactKind::SourceBundle) }).await;
    assert!(matches!(bad, Err(ProtocolError::App(AppError::Invalid(_)))));
    let too_big = ArtifactSpec { bytes: ArtifactSpec::max_bytes(ArtifactKind::Log) + 1, ..spec("big.log", ArtifactKind::Log) };
    assert!(matches!(w.h.manager.request_upload(&r, run.id(), UploadRequest { attempt: 1, spec: too_big }).await, Err(ProtocolError::TooLarge(_))));
    let grant = w.h.manager.request_upload(&r, run.id(), UploadRequest { attempt: 1, spec: spec("run.log", ArtifactKind::Log) }).await.unwrap();
    // completing before the upload is a 422
    assert!(matches!(w.h.manager.complete_upload(&r, run.id(), grant.artifact_id, 1).await, Err(ProtocolError::Rejected(_))));
    // a wrong size is a 422 and the object is deleted
    w.h.artifacts.upload_signed(&grant.url, &grant.headers, b"toolong".to_vec()).unwrap_err();
    w.h.artifacts.upload_signed(&grant.url, &grant.headers, b"abc".to_vec()).unwrap();
    assert!(matches!(w.h.manager.complete_upload(&r, run.id(), grant.artifact_id, 1).await, Err(ProtocolError::Rejected(_))));
    w.h.artifacts.upload_signed(&grant.url, &grant.headers, b"abcd".to_vec()).unwrap();
    w.h.manager.complete_upload(&r, run.id(), grant.artifact_id, 1).await.unwrap();
    w.h.manager.complete_upload(&r, run.id(), grant.artifact_id, 1).await.unwrap();
    let events = w.h.runs.events_after(w.org, run.id(), 0, 100).await.unwrap();
    assert_eq!(events.iter().filter(|e| e.kind == RunEventKind::Artifact(grant.artifact_id)).count(), 1);
    // a retried request returns the same, now complete, artifact
    let again = w.h.manager.request_upload(&r, run.id(), UploadRequest { attempt: 1, spec: spec("run.log", ArtifactKind::Log) }).await.unwrap();
    assert_eq!(again.artifact_id, grant.artifact_id);
    assert!(again.already_complete);
    // downloads: own bundle yes, someone else's artifact no
    let refs = vec![mage_core::wire::runner::DownloadRef::SourceBundle { repo_id: w.repo.repo.id, sha: w.repo.sha }];
    let dl = w.h.manager.refresh_downloads(&r, run.id(), mage_core::wire::runner::RefreshDownloadsRequest { attempt: 1, refs }).await.unwrap();
    assert_eq!(dl[0].bytes, 1234);
    let refs = vec![mage_core::wire::runner::DownloadRef::Artifact(grant.artifact_id)];
    let forbidden = w.h.manager.refresh_downloads(&r, run.id(), mage_core::wire::runner::RefreshDownloadsRequest { attempt: 1, refs }).await;
    assert!(matches!(forbidden, Err(ProtocolError::App(AppError::Forbidden(_)))));
}

#[tokio::test]
async fn runs_wait_for_their_source_bundle() {
    let w = world().await;
    // retarget the ticket at an unindexed sha
    let ticket = w.h.tickets.get_ticket(w.org, w.ticket.ticket).await.unwrap();
    let sha = mage_core::domain::CommitSha([9; 20]);
    let target = mage_core::domain::TicketTarget { repo_id: w.repo.repo.id, sha, origin: mage_core::domain::TargetOrigin::Override };
    w.h.tickets.set_target(w.org, w.ticket.ticket, Some(target), ticket.rev, &[]).await.unwrap();
    let run = w.diagnose(0).await;
    assert!(!run.source_ready);
    assert!(w.jobs_of(JobKind::IndexVersion).iter().any(|p| matches!(p, JobPayload::IndexVersion { sha: s, .. } if *s == sha)));
    let r = w.runner("r1").await;
    assert!(w.lease(&r, FREE).await.is_none());
    w.h.bundle(w.org, w.repo.repo.id, sha).await;
    assert_eq!(w.h.manager.source_ready(w.org, w.repo.repo.id, sha).await.unwrap(), 1);
    assert_eq!(w.lease(&r, FREE).await.map(|a| a.run_id), Some(run.id()));

    // a permanent bundling failure errors the waiting runs
    let w = world().await;
    let ticket = w.h.tickets.get_ticket(w.org, w.ticket.ticket).await.unwrap();
    let target = mage_core::domain::TicketTarget { repo_id: w.repo.repo.id, sha, origin: mage_core::domain::TargetOrigin::Override };
    w.h.tickets.set_target(w.org, w.ticket.ticket, Some(target), ticket.rev, &[]).await.unwrap();
    let run = w.diagnose(0).await;
    assert_eq!(w.h.manager.source_ready(w.org, w.repo.repo.id, sha).await.unwrap(), 1);
    let rec = w.get(run.id()).await;
    assert_eq!((rec.run.status, rec.run.error.clone().map(|e| e.code)), (RunStatus::Errored, Some(RunErrorCode::SourceUnavailable)));
}

#[tokio::test]
async fn queue_wait_errors_with_no_runner() {
    let w = world().await;
    let run = w.diagnose(0).await;
    w.h.advance(Duration::from_secs(2 * 3600 + 1));
    w.h.manager.reap_leases(100).await.unwrap();
    let rec = w.get(run.id()).await;
    assert_eq!((rec.run.status, rec.run.error.clone().map(|e| e.code)), (RunStatus::Errored, Some(RunErrorCode::NoRunner)));
}

#[tokio::test]
async fn learn_boot_gate_validates_the_recipe() {
    let w = world().await;
    let learn = w.h.manager.start_learn(w.org, w.repo.repo.id, None, None, UserId::from_uuid(Uuid::nil())).await.unwrap();
    let r = w.runner("r1").await;
    let a = w.lease(&r, FREE).await.unwrap();
    assert_eq!(a.recipe.map(|x| x.hash), Some(w.repo.recipe), "Learn gets the current recipe");
    w.start(&r, learn.id(), 1).await;
    let bad = WireRunResult::Learn(mage_core::domain::RecipeProposal { toml: "version = ".into(), notes: String::new() });
    assert!(matches!(w.submit(&r, learn.id(), 1, bad).await, Err(ProtocolError::Rejected(_))), "a syntax error is not a gate attempt");
    assert!(w.h.runs.gates(w.org, learn.id()).await.unwrap().is_empty());
    let toml = LEARN_TOML.to_owned();
    let ack = w.submit(&r, learn.id(), 1, WireRunResult::Learn(mage_core::domain::RecipeProposal { toml: toml.clone(), notes: String::new() })).await.unwrap();
    assert_eq!(ack.gate.map(|g| g.kind), Some(GateKind::Boot));
    let v = w.lease(&r, VERIFY_ONLY).await.unwrap();
    let hash = mage_core::domain::Recipe::draft(w.org, w.repo.repo.id, toml).unwrap().hash;
    assert_eq!(v.recipe.map(|x| x.hash), Some(hash), "Boot verify runs the proposal");
    w.start(&r, v.run_id, 1).await;
    let boot = VerifyAttempt { index: 1, kind: AttemptKind::Boot, script_exit: None, script_passed: true, status: None, observed: Vec::new(), health_ok: true, duration_ms: 5 };
    let result = VerifyResult { outcome: VerifyOutcome::Completed, observed: None, attempts: vec![boot], artifacts: Vec::new(), fix_checks: None, rebased_patch: None };
    w.submit(&r, v.run_id, 1, WireRunResult::Verify(result)).await.unwrap();
    w.drain().await;
    assert_eq!(w.get(learn.id()).await.run.status, RunStatus::Succeeded);
    w.drain().await;
    use mage_core::ports::repos::RepoStore as _;
    let recipe = w.h.repos.get_recipe(w.org, w.repo.repo.id, hash).await.unwrap();
    assert!(matches!(recipe.status, mage_core::domain::RecipeStatus::Validated { .. }));
}

/// A recipe that passes `RecipeSpec::validate`.
const LEARN_TOML: &str = r#"version = 1
ports = ["web"]

[services.web]
run = "npm start"
ports = ["web"]

[services.web.health]
http = "http://127.0.0.1:${port.web}/"
"#;
