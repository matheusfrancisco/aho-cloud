//! StartGate, EvaluateGate and feedback rendering (runs.md "StartGate", "EvaluateGate"), plus the
//! onboarding Boot verify of a saved recipe (`POST …/repos/{repo}/recipe/verify`).

use std::fmt::Write as _;

use time::OffsetDateTime;

use mage_core::domain::gates::{self, BootGate, ReproGate};
use mage_core::domain::{
    ArtifactKind, CommitSha, Gate, GateEvent, GateFailure, GateFailureCode, GateFeedback, GateKind, GateOutcome, GateStatus, OrgId, RecipeHash,
    RepoId, RepoVersionKey, Run, RunCommand, RunError, RunErrorCode, RunEventKind, RunId, RunInit, RunKind, RunResult, RunStatus, RunStatusEvent, RunSubject, UserId, VerifyOutcome, VerifyResult, VerifySpec,
};
use mage_core::ports::blob::BlobKey;

use crate::app::runs::machine::{Initiator, step};
use crate::app::runs::{MAX_COMMIT_RETRIES, RunManager, blob_err, contention, is_stale, repo_err};
use crate::error::AppError;
use crate::ports::runs::{ArtifactState, CommandDraft, GateRecord, GateWrite, NewRun, RunEventDraft, RunQuery, RunRecord, RunUpdate, RunWrite};

/// Rendered feedback cap.
pub const FEEDBACK_MAX_BYTES: usize = 8 * 1024;
/// Log tail included in feedback.
pub const LOG_TAIL_BYTES: u64 = 2 * 1024;

/// How a finished Verify run decides its gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Decision {
    Pass,
    Fail(GateFailure),
    /// Infrastructure error: retry the Verify run (no attempt consumed).
    InfraRetry(String),
}

impl RunManager {
    /// Inserts a Verify run for `parent` into `w` (no separate commit); returns its id.
    pub(crate) async fn new_verify_run(
        &self,
        parent: &RunRecord,
        verify: VerifySpec,
        recipe_hash: Option<RecipeHash>,
        sha: Option<CommitSha>,
        affinity: bool,
        now: OffsetDateTime,
        w: &mut RunWrite,
    ) -> Result<RunId, AppError> {
        let mut plan = self.verify_plan(parent, verify, recipe_hash, now).await?;
        if !affinity {
            plan.affinity = None;
        }
        if let Some(sha) = sha.filter(|s| Some(*s) != parent.spec.sha) {
            let repo = parent.spec.repo_id.ok_or_else(|| AppError::Invalid("the parent run has no repo".into()))?;
            let (ready, job) = self.source_state(parent.org(), RepoVersionKey::new(repo, sha), parent.spec.ticket_id).await?;
            plan.spec.sha = Some(sha);
            plan.source_ready = ready;
            w.jobs.extend(job);
        }
        let id: RunId = self.d.ids.new_id();
        let run = Run::new(RunInit {
            id,
            org_id: parent.org(),
            kind: RunKind::Verify,
            subject: RunSubject::Run(parent.id()),
            harness: None,
            budget: plan.budget,
            created_at: now,
        })?;
        w.events.push(RunEventDraft { run_id: id, attempt: 1, at: now, kind: RunEventKind::Status(RunStatus::Queued) });
        w.inserts.push(NewRun { run, spec: plan.spec, create_key: None, source_ready: plan.source_ready, affinity: plan.affinity });
        Ok(id)
    }

    /// StartGate (inside the commit of `ResultSubmitted{needs_gate}`; call before stepping the
    /// parent): gate row `Pending` with a fresh Verify run (affinity to the parent's runner) and
    /// the parent's `GateStarted` + `Notice` events.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn start_gate(
        &self,
        parent: &mut RunRecord,
        kind: GateKind,
        attempt: u8,
        verify: VerifySpec,
        recipe_hash: Option<RecipeHash>,
        details: Option<serde_json::Value>,
        now: OffsetDateTime,
        w: &mut RunWrite,
    ) -> Result<GateRecord, AppError> {
        let v = self.new_verify_run(parent, verify, recipe_hash, None, true, now, w).await?;
        let gate = GateRecord {
            gate: Gate { id: self.d.ids.new_id(), org_id: parent.org(), run_id: parent.id(), kind, attempt, status: GateStatus::Pending, verify_run: Some(v) },
            feedback: None,
            details,
            created_at: now,
        };
        w.gates.push(GateWrite::Insert(gate.clone()));
        let at = parent.run.attempt;
        w.events.push(RunEventDraft { run_id: parent.id(), attempt: at, at: now, kind: RunEventKind::GateStarted(kind) });
        w.events.push(RunEventDraft { run_id: parent.id(), attempt: at, at: now, kind: RunEventKind::Notice(format!("verify run {v}")) });
        Ok(gate)
    }

    /// A decided failure of `gate` on a parent in `AwaitingGate`: `GateFailed{left}`, then
    /// alive + left > 0 → `Running` with `GateFeedback` (and the budget re-check), lost session +
    /// left > 0 → `LeaseLost` (requeue; feedback via `prior_feedback`), left = 0 → `Failed` with
    /// `GateFeedback{.., 0}`.
    pub(crate) async fn fail_gate(
        &self,
        parent: &mut RunRecord,
        gate: &GateRecord,
        reason: String,
        details: String,
        now: OffsetDateTime,
        w: &mut RunWrite,
    ) -> Result<(), AppError> {
        let left = parent.run.budget.max_gate_attempts.saturating_sub(gate.gate.attempt);
        let attempt = parent.run.attempt;
        w.events.push(RunEventDraft { run_id: parent.id(), attempt, at: now, kind: RunEventKind::GateResult(gate.gate.kind, gate.gate.status.clone()) });
        let stepped = step(parent, RunStatusEvent::GateFailed { attempts_left: left }, Initiator::Server, now, w)?;
        if left > 0 && parent.session_lost {
            let max = self.d.policy.max_lease_attempts;
            let lost = step(parent, RunStatusEvent::LeaseLost { max_attempts: max }, Initiator::Server, now, w)?;
            if lost.cascade {
                self.cascade(parent, now, w).await?;
            }
            return Ok(());
        }
        w.commands.push(CommandDraft {
            run_id: parent.id(),
            attempt,
            command: RunCommand::GateFeedback(GateFeedback {
                gate_id: gate.gate.id,
                kind: gate.gate.kind,
                attempt: gate.gate.attempt,
                outcome: GateOutcome::Failed { reason, details, attempts_left: left },
            }),
        });
        if stepped.cascade {
            self.cascade(parent, now, w).await?;
        } else {
            self.enforce_budget(parent, now, w).await?;
        }
        Ok(())
    }

    /// EvaluateGate for a terminal Verify run (job `runs.apply_outcome`).
    pub(crate) async fn evaluate_gate(&self, org: OrgId, verify_id: RunId) -> Result<(), AppError> {
        let verify = self.d.runs.get(org, verify_id).await?;
        if !verify.run.status.is_terminal() {
            return Ok(());
        }
        let Some(gate) = self.d.runs.gate_for_verify(org, verify_id).await? else {
            return self.d.hooks.fix_outcome.apply(org, verify_id).await;
        };
        if gate.gate.status != GateStatus::Pending || gate.gate.verify_run != Some(verify_id) || verify.run.status == RunStatus::Cancelled {
            return Ok(());
        }
        for _ in 0..MAX_COMMIT_RETRIES {
            let mut parent = self.d.runs.get(org, gate.gate.run_id).await?;
            let manual = parent.run.status != RunStatus::AwaitingGate;
            if manual && !(gate.gate.kind == GateKind::Boot && parent.run.kind == RunKind::Learn && parent.run.status.is_terminal()) {
                return Ok(());
            }
            let decision = self.decide(&parent, &gate, &verify).await?;
            let now = self.now();
            let mut w = RunWrite::default();
            let mut gate = gate.clone();
            let mut validate_manual = false;
            match decision {
                Decision::InfraRetry(why) => {
                    let since = self.verify_children_since(org, parent.id(), gate.created_at).await?;
                    if since <= usize::from(self.d.policy.verify_infra_retries) {
                        let spec = verify.spec.verify.clone().ok_or_else(|| AppError::Internal("verify run without spec".into()))?;
                        let v = self.new_verify_run(&parent, spec, verify.spec.recipe_hash, verify.spec.sha, false, now, &mut w).await?;
                        gate.gate.verify_run = Some(v);
                        w.gates.push(GateWrite::Update(gate.clone()));
                        w.events.push(RunEventDraft {
                            run_id: parent.id(),
                            attempt: parent.run.attempt,
                            at: now,
                            kind: RunEventKind::Notice(format!("verify run {verify_id} hit an infrastructure error ({why}); retrying as verify run {v}")),
                        });
                    } else {
                        gate.gate.status = GateStatus::Pending.apply(GateEvent::Fail { reason: format!("verify infrastructure failed: {why}"), at: now })?;
                        w.gates.push(GateWrite::Update(gate.clone()));
                        if !manual {
                            parent.run.set_error(RunError { code: RunErrorCode::VerifyInfra, message: why });
                            self.step_and_queue(parent.clone(), RunStatusEvent::InfraError, Initiator::Server, now, &mut w).await?;
                        }
                    }
                }
                Decision::Pass => {
                    gate.gate.status = GateStatus::Pending.apply(GateEvent::Pass { at: now })?;
                    w.gates.push(GateWrite::Update(gate.clone()));
                    let attempt = parent.run.attempt;
                    w.events.push(RunEventDraft { run_id: parent.id(), attempt, at: now, kind: RunEventKind::GateResult(gate.gate.kind, gate.gate.status.clone()) });
                    if manual {
                        validate_manual = true;
                    } else {
                        w.commands.push(CommandDraft {
                            run_id: parent.id(),
                            attempt,
                            command: RunCommand::GateFeedback(GateFeedback {
                                gate_id: gate.gate.id,
                                kind: gate.gate.kind,
                                attempt: gate.gate.attempt,
                                outcome: GateOutcome::Passed,
                            }),
                        });
                        self.step_and_queue(parent.clone(), RunStatusEvent::GatePassed, Initiator::Server, now, &mut w).await?;
                    }
                }
                Decision::Fail(failure) => {
                    let feedback = self.render_feedback(&gate, &failure, &verify).await?;
                    gate.gate.status = GateStatus::Pending.apply(GateEvent::Fail { reason: failure.reason.clone(), at: now })?;
                    gate.feedback = Some(feedback.clone());
                    w.gates.push(GateWrite::Update(gate.clone()));
                    if manual {
                        w.events.push(RunEventDraft {
                            run_id: parent.id(),
                            attempt: parent.run.attempt,
                            at: now,
                            kind: RunEventKind::GateResult(gate.gate.kind, gate.gate.status.clone()),
                        });
                    } else {
                        self.fail_gate(&mut parent, &gate, failure.reason.clone(), feedback, now, &mut w).await?;
                        w.updates.push(RunUpdate::of(parent.clone()));
                    }
                }
            }
            match self.d.runs.commit(org, w).await {
                Ok(_) => {
                    if validate_manual {
                        self.validate_recipe(org, &parent, &gate, &verify).await?;
                    }
                    return Ok(());
                }
                Err(e) if is_stale(&e) => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Err(contention())
    }

    /// Verify children of `parent` created at or after `since` (the gate attempt's runs).
    async fn verify_children_since(&self, org: OrgId, parent: RunId, since: OffsetDateTime) -> Result<usize, AppError> {
        let children = self
            .d
            .runs
            .list(org, &RunQuery { parent: Some(parent), kind: Some(RunKind::Verify), limit: RunQuery::MAX_LIMIT, ..RunQuery::default() })
            .await?;
        Ok(children.iter().filter(|c| c.run.created_at >= since).count())
    }

    /// The gate decision for a terminal, non-cancelled Verify run (runs.md EvaluateGate table).
    pub(crate) async fn decide(&self, parent: &RunRecord, gate: &GateRecord, verify: &RunRecord) -> Result<Decision, AppError> {
        let result = match (verify.run.status, &verify.run.result) {
            (RunStatus::TimedOut, _) => {
                return Ok(Decision::Fail(GateFailure { code: GateFailureCode::VerifyErrored, reason: "verify exceeded max_wall".into() }));
            }
            (RunStatus::Errored | RunStatus::Failed, _) => {
                let why = verify.run.error.as_ref().map_or_else(|| "verify run errored".to_owned(), |e| e.message.clone());
                return Ok(Decision::InfraRetry(why));
            }
            (RunStatus::Succeeded, Some(RunResult::Verify(v))) => v,
            (RunStatus::Succeeded, _) => return Err(AppError::Internal(format!("verify run {} succeeded without a result", verify.id()))),
            (s, _) => return Err(AppError::Internal(format!("verify run {} is {}", verify.id(), s.name()))),
        };
        if let VerifyOutcome::Errored { reason } = &result.outcome {
            return Ok(Decision::InfraRetry(reason.clone()));
        }
        let outcome = match gate.gate.kind {
            GateKind::Boot => self.boot_gate(parent, gate, verify, result).map(|_| ()),
            GateKind::Repro => self.repro_gate(parent, gate, verify, result).map(|_| ()),
            GateKind::Fix => self.d.hooks.fix_gate.judge(parent.org(), parent, gate, verify).await?,
            GateKind::Evidence => return Err(AppError::Internal("evidence gates have no verify run".into())),
        };
        Ok(match outcome {
            Ok(()) => Decision::Pass,
            Err(f) => Decision::Fail(f),
        })
    }

    pub(crate) fn boot_gate(
        &self,
        parent: &RunRecord,
        gate: &GateRecord,
        verify: &RunRecord,
        result: &VerifyResult,
    ) -> Result<gates::BootGatePassed, GateFailure> {
        let mismatch = |what: &str| GateFailure { code: GateFailureCode::SpecMismatch, reason: format!("verify run has no {what}") };
        let input = BootGate {
            gate: gate.gate.id,
            learn_run: parent.id(),
            repo_id: verify.spec.repo_id.ok_or_else(|| mismatch("repo"))?,
            recipe: verify.spec.recipe_hash.ok_or_else(|| mismatch("recipe"))?,
            sha: verify.spec.sha.ok_or_else(|| mismatch("sha"))?,
        };
        gates::boot(&input, result)
    }

    pub(crate) fn repro_gate(
        &self,
        parent: &RunRecord,
        gate: &GateRecord,
        verify: &RunRecord,
        result: &VerifyResult,
    ) -> Result<gates::ReproGatePassed, GateFailure> {
        let mismatch = |why: &str| GateFailure { code: GateFailureCode::SpecMismatch, reason: why.to_owned() };
        let RunSubject::Cause(cause) = parent.run.subject else { return Err(mismatch("repro gate on a run without a cause")) };
        let Some(RunResult::Diagnose(diagnose)) = &parent.run.result else { return Err(mismatch("no stored diagnosis")) };
        let spec = verify.spec.verify.as_ref().ok_or_else(|| mismatch("verify run without spec"))?;
        gates::repro(&ReproGate { gate: gate.gate.id, diagnose_run: parent.id(), cause, spec }, diagnose, result)
    }

    /// Recipe validation of an onboarding Boot verify (no Learn status change).
    async fn validate_recipe(&self, org: OrgId, learn: &RunRecord, gate: &GateRecord, verify: &RunRecord) -> Result<(), AppError> {
        let Some(RunResult::Verify(result)) = &verify.run.result else { return Err(AppError::Internal("verify result missing".into())) };
        let proof = self.boot_gate(learn, gate, verify, result).map_err(|f| AppError::Invalid(format!("boot proof could not be re-derived: {f}")))?;
        let mut recipe = self.d.repos.get_recipe(org, proof.repo_id(), proof.recipe()).await.map_err(repo_err)?;
        let expect = recipe.status.clone();
        recipe.validate(proof, self.now())?;
        self.d.repos.update_recipe(org, &recipe, &expect, &[]).await.map_err(repo_err)
    }

    /// Feedback for a failed gate (≤ 8 KiB): reason, expected and observed signatures per
    /// repetition, exit codes and health, fix checks, the Verify run's log tail and an instruction.
    pub(crate) async fn render_feedback(&self, gate: &GateRecord, failure: &GateFailure, verify: &RunRecord) -> Result<String, AppError> {
        let mut out = String::new();
        let _ = writeln!(out, "{:?} gate attempt {} failed ({}): {}", gate.gate.kind, gate.gate.attempt, failure.code, failure.reason);
        let expected = match &verify.spec.verify {
            Some(VerifySpec::Repro { signature, .. } | VerifySpec::Fix { signature, .. }) => Some(signature),
            _ => None,
        };
        if let Some(sig) = expected {
            let _ = writeln!(out, "Expected signature: {:?} {:?} {:?}", sig.kind, sig.matcher, sig.value);
        }
        if let Some(RunResult::Verify(v)) = &verify.run.result {
            match &v.outcome {
                VerifyOutcome::BootFailed { reason } => {
                    let _ = writeln!(out, "Boot failed: {reason}");
                }
                VerifyOutcome::PatchRejected { reason } => {
                    let _ = writeln!(out, "Patch rejected: {reason}");
                }
                _ => {}
            }
            for a in &v.attempts {
                let observed: Vec<String> = a.observed.iter().map(|o| format!("{:?} {:?}", o.kind, o.value)).collect();
                let _ = writeln!(
                    out,
                    "Repetition {} ({:?}): exit {}, passed {}, healthy {}, observed [{}]",
                    a.index,
                    a.kind,
                    a.script_exit.map_or_else(|| "n/a".to_owned(), |c| c.to_string()),
                    a.script_passed,
                    a.health_ok,
                    observed.join(", ")
                );
            }
            if let Some(fc) = &v.fix_checks {
                let _ = writeln!(
                    out,
                    "Fix checks: rebuilt and healthy {}, test exit {}, lint exit {}, applies to source {}",
                    fc.boot,
                    fc.test_exit.map_or_else(|| "n/a".to_owned(), |c| c.to_string()),
                    fc.lint_exit.map_or_else(|| "n/a".to_owned(), |c| c.to_string()),
                    fc.source_applies.map_or_else(|| "n/a".to_owned(), |b| b.to_string()),
                );
            }
        }
        if let Some(tail) = self.log_tail(verify).await? {
            let _ = writeln!(out, "Log tail:\n{tail}");
        }
        out.push_str(instruction(gate.gate.kind));
        truncate(&mut out, FEEDBACK_MAX_BYTES);
        Ok(out)
    }

    /// The last 2 KiB of the Verify run's newest complete `Log` artifact.
    async fn log_tail(&self, verify: &RunRecord) -> Result<Option<String>, AppError> {
        let logs = self.d.runs.artifacts(verify.org(), verify.id()).await?;
        let Some(log) = logs.iter().rev().find(|a| a.artifact.kind == ArtifactKind::Log && a.state == ArtifactState::Complete) else {
            return Ok(None);
        };
        if log.artifact.bytes == 0 {
            return Ok(None);
        }
        let key = BlobKey::parse(&log.artifact.key).map_err(blob_err)?;
        let start = log.artifact.bytes.saturating_sub(LOG_TAIL_BYTES);
        match self.d.artifacts.get_range(&key, start..log.artifact.bytes).await {
            Ok(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
            Err(e) => {
                tracing::warn!(error = %blob_err(e), "log tail unavailable");
                Ok(None)
            }
        }
    }

    /// Onboarding Boot verify of a saved recipe: a Verify run whose parent is the repo's newest
    /// (terminal) Learn run, with a `Pending` Boot gate row on that run. A pass validates the
    /// recipe; the Learn run's status is unchanged.
    pub async fn verify_recipe(&self, org: OrgId, repo: RepoId, hash: RecipeHash, sha: Option<CommitSha>, by: UserId) -> Result<RunRecord, AppError> {
        self.d.repos.get_recipe(org, repo, hash).await.map_err(repo_err)?;
        let sha = match sha {
            Some(s) => s,
            None => self.d.repos.default_head(org, repo).await.map_err(repo_err)?.ok_or_else(|| AppError::Invalid("the repo has no default head yet".into()))?,
        };
        for _ in 0..MAX_COMMIT_RETRIES {
            let learn = self
                .d
                .runs
                .list(org, &RunQuery { kind: Some(RunKind::Learn), repo: Some(repo), newest_first: true, limit: 1, ..RunQuery::default() })
                .await?
                .into_iter()
                .next()
                .ok_or_else(|| AppError::Invalid("start a Learn run first".into()))?;
            if !learn.run.status.is_terminal() {
                return Err(AppError::Conflict("a Learn run is in progress".into()));
            }
            let now = self.now();
            let mut w = RunWrite::default();
            let n = u8::try_from(self.gates_of(org, learn.id(), GateKind::Boot).await?.len()).unwrap_or(u8::MAX).saturating_add(1);
            let v = self.new_verify_run(&learn, VerifySpec::Boot, Some(hash), Some(sha), false, now, &mut w).await?;
            w.gates.push(GateWrite::Insert(GateRecord {
                gate: Gate { id: self.d.ids.new_id(), org_id: org, run_id: learn.id(), kind: GateKind::Boot, attempt: n, status: GateStatus::Pending, verify_run: Some(v) },
                feedback: None,
                details: None,
                created_at: now,
            }));
            w.events.push(RunEventDraft { run_id: learn.id(), attempt: learn.run.attempt, at: now, kind: RunEventKind::GateStarted(GateKind::Boot) });
            w.events.push(RunEventDraft { run_id: learn.id(), attempt: learn.run.attempt, at: now, kind: RunEventKind::Notice(format!("verify run {v}")) });
            w.updates.push(RunUpdate::of(learn));
            match self.d.runs.commit(org, w).await {
                Ok(_) => {
                    self.audit_user(org, by, crate::ports::audit::AuditAction::RunStarted, v).await;
                    return Ok(self.d.runs.get(org, v).await?);
                }
                Err(e) if is_stale(&e) => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Err(contention())
    }
}

fn instruction(kind: GateKind) -> &'static str {
    match kind {
        GateKind::Repro => {
            "The script did not fail the same way in a clean fork. Do not rely on edits to application code; seed data through the recipe or the script. Fix the script and call submit_diagnosis again.\n"
        }
        GateKind::Boot => {
            "The recipe did not boot cleanly in a fresh fork. Fix the failing step (build, database, seed or service health) in .mage/env.toml and call submit_recipe again.\n"
        }
        GateKind::Fix => {
            "The patch did not pass the fix gate in a clean fork. Address the failing check, keep the repro script unchanged, and call submit_fix again.\n"
        }
        GateKind::Evidence => "Fix the cited evidence and call submit_triage again.\n",
    }
}

/// Truncates on a char boundary, marking the cut.
pub(crate) fn truncate(s: &mut String, max: usize) {
    const MARK: &str = "\n[truncated]";
    if s.len() <= max {
        return;
    }
    let mut end = max.saturating_sub(MARK.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    s.push_str(MARK);
}
