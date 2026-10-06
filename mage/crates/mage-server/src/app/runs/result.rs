//! SubmitResult (`POST /runs/{id}/result`, runs.md): replay by `submission_id`, fence, `Running`
//! only, kind check, artifact checks, then the per-kind outcome in one commit with the stored
//! reply.

use serde::{Deserialize, Serialize};

use mage_core::domain::recipe::RecipeSpec;
use mage_core::domain::{
    ArtifactId, ArtifactKind, DiagnoseResult, FixResult, GateEvent, GateFeedback, GateKind, GateOutcome, GateStatus, Recipe, RecipeProposal,
    RunCommand, RunEventKind, RunId, RunResult, RunStatus, RunStatusEvent, Verdict, VerifyResult, VerifySpec,
};
use mage_core::wire::runner::{GateTicket, Rejection, RejectionItem, SubmitAck, SubmitResultRequest, TriageSubmission, WireRunResult};

use crate::app::runs::hooks::{EvidenceDecision, FixAcceptance};
use crate::app::runs::machine::{Initiator, step};
use crate::app::runs::{MAX_COMMIT_RETRIES, ProtocolError, RunManager, RunnerCaller, contention, is_stale, repo_err, request_hash};
use crate::error::AppError;
use crate::ports::runs::{
    ArtifactState, CommandDraft, GateRecord, GateWrite, IdempotentReply, RunConflict, RunEventDraft, RunRecord, RunStoreError, RunUpdate, RunWrite,
    StoredReply,
};

/// The stored answer of one submission (replayed verbatim on retry).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "body", rename_all = "snake_case")]
enum StoredSubmit {
    Ack(SubmitAck),
    Rejected(Rejection),
}

impl StoredSubmit {
    fn into_result(self) -> Result<SubmitAck, ProtocolError> {
        match self {
            StoredSubmit::Ack(ack) => Ok(ack),
            StoredSubmit::Rejected(r) => Err(ProtocolError::Rejected(r)),
        }
    }
}

/// What a per-kind handler decided.
enum Submitted {
    /// Commit `w` and answer `reply` (stored for replay).
    Commit { w: RunWrite, reply: StoredSubmit },
    /// Validation failure: 422, nothing written, no attempt consumed.
    Invalid(Rejection),
}

fn invalid(message: impl Into<String>, items: Vec<RejectionItem>) -> Submitted {
    Submitted::Invalid(Rejection { gate: None, message: message.into(), items })
}

fn item(path: impl Into<String>, reason: impl Into<String>) -> RejectionItem {
    RejectionItem { path: path.into(), reason: reason.into() }
}

fn replay(stored: StoredReply, hash: [u8; 32]) -> Result<SubmitAck, ProtocolError> {
    if stored.request_hash != hash {
        return Err(ProtocolError::IdempotencyMismatch);
    }
    let reply: StoredSubmit = serde_json::from_slice(&stored.body).map_err(|e| AppError::Internal(format!("stored reply: {e}")))?;
    reply.into_result()
}

fn ticket(g: &GateRecord) -> GateTicket {
    GateTicket { gate_id: g.gate.id, kind: g.gate.kind, attempt: g.gate.attempt }
}

impl RunManager {
    pub async fn submit_result(&self, caller: &RunnerCaller, run: RunId, req: SubmitResultRequest) -> Result<SubmitAck, ProtocolError> {
        let scope = format!("runner:{}:result:{run}", caller.id());
        let key = req.submission_id.to_string();
        let body = serde_json::to_vec(&(req.attempt, &req.result)).map_err(|e| AppError::Internal(e.to_string()))?;
        let hash = request_hash(&body);
        if let Some(stored) = self.d.runs.reply(&scope, &key).await? {
            return replay(stored, hash);
        }
        for _ in 0..MAX_COMMIT_RETRIES {
            let rec = self.fenced(caller, run, req.attempt).await?;
            let org = rec.org();
            match rec.run.status {
                RunStatus::Running => {}
                s if s.is_terminal() => return Err(ProtocolError::RunTerminal(s)),
                s => return Err(ProtocolError::InvalidState(s)),
            }
            if req.result.kind() != rec.run.kind {
                return Err(AppError::Invalid(format!("{:?} result on a {:?} run", req.result.kind(), rec.run.kind)).into());
            }
            let submitted = match req.result.clone() {
                WireRunResult::Learn(p) => self.submit_learn(rec, p).await?,
                WireRunResult::Triage(s) => self.submit_triage(rec, s).await?,
                WireRunResult::Diagnose(d) => self.submit_diagnose(rec, d).await?,
                WireRunResult::Fix(f) => self.submit_fix(rec, f).await?,
                WireRunResult::Verify(v) => self.submit_verify(rec, v).await?,
            };
            let (mut w, reply) = match submitted {
                Submitted::Invalid(r) => return Err(ProtocolError::Rejected(r)),
                Submitted::Commit { w, reply } => (w, reply),
            };
            let stored = serde_json::to_vec(&reply).map_err(|e| AppError::Internal(e.to_string()))?;
            w.reply = Some(IdempotentReply { scope: scope.clone(), key: key.clone(), request_hash: hash, body: stored });
            match self.d.runs.commit(org, w).await {
                Ok(_) => return reply.into_result(),
                Err(RunStoreError::Conflict(RunConflict::ReplyExists(stored))) => return replay(stored, hash),
                Err(e) if is_stale(&e) => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Err(contention().into())
    }

    /// Every referenced artifact belongs to the run, is complete and (when given) of `kind`.
    async fn check_artifacts(&self, rec: &RunRecord, refs: &[(String, ArtifactId, Option<ArtifactKind>)]) -> Result<Vec<RejectionItem>, AppError> {
        let mut problems = Vec::new();
        for (path, id, kind) in refs {
            match self.d.runs.artifact(rec.org(), *id).await {
                Ok(a) if a.artifact.run_id != rec.id() => problems.push(item(path.clone(), "artifact belongs to another run")),
                Ok(a) if a.state != ArtifactState::Complete => problems.push(item(path.clone(), "artifact upload is not complete")),
                Ok(a) if kind.is_some_and(|k| k != a.artifact.kind) => {
                    problems.push(item(path.clone(), format!("expected a {} artifact", kind.map_or("", |k| k.as_str()))))
                }
                Ok(_) => {}
                Err(RunStoreError::NotFound) => problems.push(item(path.clone(), "unknown artifact")),
                Err(e) => return Err(e.into()),
            }
        }
        Ok(problems)
    }

    async fn submit_learn(&self, mut rec: RunRecord, proposal: RecipeProposal) -> Result<Submitted, AppError> {
        let repo = rec.spec.repo_id.ok_or_else(|| AppError::Internal("learn run without repo".into()))?;
        let spec = match RecipeSpec::parse(&proposal.toml) {
            Ok(s) => s,
            Err(e) => return Ok(invalid("the recipe does not parse", vec![item("toml", e.to_string())])),
        };
        let report = spec.validate();
        if !report.is_valid() {
            let items = report.errors.iter().map(|i| item(i.path.clone(), i.message.clone())).collect();
            return Ok(invalid("the recipe has errors", items));
        }
        let recipe = Recipe::draft(rec.org(), repo, proposal.toml.clone())?;
        self.d.repos.put_recipe(rec.org(), &recipe).await.map_err(repo_err)?;
        let now = self.now();
        let n = next_attempt(&self.gates_of(rec.org(), rec.id(), GateKind::Boot).await?);
        rec.run.set_result(RunResult::Learn(proposal))?;
        let mut w = RunWrite::default();
        let gate = self.start_gate(&mut rec, GateKind::Boot, n, VerifySpec::Boot, Some(recipe.hash), None, now, &mut w).await?;
        step(&mut rec, RunStatusEvent::ResultSubmitted { needs_gate: true }, Initiator::Runner, now, &mut w)?;
        w.updates.push(RunUpdate::of(rec));
        let message = format!("Submitted. Boot gate attempt {n} is running in a clean fork. End your turn now; feedback will follow.");
        Ok(Submitted::Commit { w, reply: StoredSubmit::Ack(SubmitAck { status: RunStatus::AwaitingGate, gate: Some(ticket(&gate)), message }) })
    }

    async fn submit_triage(&self, mut rec: RunRecord, sub: TriageSubmission) -> Result<Submitted, AppError> {
        let org = rec.org();
        let n = next_attempt(&self.gates_of(org, rec.id(), GateKind::Evidence).await?);
        let decision = self.d.hooks.triage_gate.evaluate(org, &rec, n, sub).await?;
        let now = self.now();
        let mut w = RunWrite::default();
        let mut gate = GateRecord {
            gate: mage_core::domain::Gate {
                id: self.d.ids.new_id(),
                org_id: org,
                run_id: rec.id(),
                kind: GateKind::Evidence,
                attempt: n,
                status: GateStatus::Pending,
                verify_run: None,
            },
            feedback: None,
            details: None,
            created_at: now,
        };
        let attempt = rec.run.attempt;
        let run_id = rec.id();
        let event = |kind: RunEventKind| RunEventDraft { run_id, attempt, at: now, kind };
        let reply = match decision {
            EvidenceDecision::Invalid { rejection } => return Ok(Submitted::Invalid(Rejection { gate: None, ..rejection })),
            EvidenceDecision::Accept { result, details, message, notices } => {
                gate.gate.status = GateStatus::Pending.apply(GateEvent::Pass { at: now })?;
                gate.details = Some(details);
                for n in notices {
                    w.events.push(event(RunEventKind::Notice(n)));
                }
                w.events.push(event(RunEventKind::GateResult(GateKind::Evidence, gate.gate.status.clone())));
                rec.run.set_result(RunResult::Triage(result))?;
                step(&mut rec, RunStatusEvent::ResultSubmitted { needs_gate: false }, Initiator::Runner, now, &mut w)?;
                StoredSubmit::Ack(SubmitAck { status: rec.run.status, gate: Some(ticket(&gate)), message })
            }
            EvidenceDecision::Reject { details, rejection, reason, .. } => {
                gate.gate.status = GateStatus::Pending.apply(GateEvent::Fail { reason, at: now })?;
                gate.details = Some(details);
                gate.feedback = Some(rejection.message.clone());
                w.events.push(event(RunEventKind::GateResult(GateKind::Evidence, gate.gate.status.clone())));
                StoredSubmit::Rejected(Rejection { gate: Some(ticket(&gate)), ..rejection })
            }
            EvidenceDecision::Fail { details, rejection, reason } => {
                gate.gate.status = GateStatus::Pending.apply(GateEvent::Fail { reason: reason.clone(), at: now })?;
                gate.details = Some(details);
                gate.feedback = Some(rejection.message.clone());
                w.events.push(event(RunEventKind::GateResult(GateKind::Evidence, gate.gate.status.clone())));
                step(&mut rec, RunStatusEvent::ResultSubmitted { needs_gate: true }, Initiator::Runner, now, &mut w)?;
                let stepped = step(&mut rec, RunStatusEvent::GateFailed { attempts_left: 0 }, Initiator::Server, now, &mut w)?;
                if stepped.cascade {
                    self.cascade(&rec, now, &mut w).await?;
                }
                w.commands.push(CommandDraft {
                    run_id: rec.id(),
                    attempt,
                    command: RunCommand::GateFeedback(GateFeedback {
                        gate_id: gate.gate.id,
                        kind: GateKind::Evidence,
                        attempt: n,
                        outcome: GateOutcome::Failed { reason, details: rejection.message.clone(), attempts_left: 0 },
                    }),
                });
                StoredSubmit::Rejected(Rejection { gate: Some(ticket(&gate)), ..rejection })
            }
        };
        w.gates.push(GateWrite::Insert(gate));
        w.updates.push(RunUpdate::of(rec));
        Ok(Submitted::Commit { w, reply })
    }

    async fn submit_diagnose(&self, mut rec: RunRecord, d: DiagnoseResult) -> Result<Submitted, AppError> {
        if let Err(e) = d.validate() {
            return Ok(invalid("the diagnosis is incomplete", vec![item("", e.to_string())]));
        }
        if let Some(script) = d.script {
            let problems = self.check_artifacts(&rec, &[("script".into(), script, Some(ArtifactKind::Script))]).await?;
            if !problems.is_empty() {
                return Ok(invalid("the script artifact is not usable", problems));
            }
        }
        let now = self.now();
        let mut w = RunWrite::default();
        let reply = match (&d.verdict, d.script, d.signature.clone()) {
            (Verdict::Reproduced, Some(script), Some(signature)) => {
                let n = next_attempt(&self.gates_of(rec.org(), rec.id(), GateKind::Repro).await?);
                let verify = VerifySpec::Repro { script, signature, repetitions: self.d.policy.repro_repetitions };
                rec.run.set_result(RunResult::Diagnose(d))?;
                let recipe = rec.spec.recipe_hash;
                let gate = self.start_gate(&mut rec, GateKind::Repro, n, verify, recipe, None, now, &mut w).await?;
                step(&mut rec, RunStatusEvent::ResultSubmitted { needs_gate: true }, Initiator::Runner, now, &mut w)?;
                let message = format!("Submitted. Repro gate attempt {n} is running in a clean fork. End your turn now; feedback will follow.");
                StoredSubmit::Ack(SubmitAck { status: RunStatus::AwaitingGate, gate: Some(ticket(&gate)), message })
            }
            _ => {
                rec.run.set_result(RunResult::Diagnose(d))?;
                step(&mut rec, RunStatusEvent::ResultSubmitted { needs_gate: false }, Initiator::Runner, now, &mut w)?;
                StoredSubmit::Ack(SubmitAck { status: RunStatus::Succeeded, gate: None, message: "Diagnosis recorded. End your turn.".into() })
            }
        };
        w.updates.push(RunUpdate::of(rec));
        Ok(Submitted::Commit { w, reply })
    }

    async fn submit_fix(&self, mut rec: RunRecord, f: FixResult) -> Result<Submitted, AppError> {
        let problems = self.check_artifacts(&rec, &[("patch".into(), f.patch, Some(ArtifactKind::Patch))]).await?;
        if !problems.is_empty() {
            return Ok(invalid("the patch artifact is not usable", problems));
        }
        let org = rec.org();
        let n = next_attempt(&self.gates_of(org, rec.id(), GateKind::Fix).await?);
        let acceptance = self.d.hooks.fix_gate.accept(org, &rec, n, &f).await?;
        let now = self.now();
        let mut w = RunWrite::default();
        rec.run.set_result(RunResult::Fix(f))?;
        let reply = match acceptance {
            FixAcceptance::Accepted { verify, details } => {
                let recipe = rec.spec.recipe_hash;
                let gate = self.start_gate(&mut rec, GateKind::Fix, n, verify, recipe, Some(details), now, &mut w).await?;
                step(&mut rec, RunStatusEvent::ResultSubmitted { needs_gate: true }, Initiator::Runner, now, &mut w)?;
                let message = format!("Submitted. Fix gate attempt {n} is running in a clean fork. End your turn now; feedback will follow.");
                StoredSubmit::Ack(SubmitAck { status: RunStatus::AwaitingGate, gate: Some(ticket(&gate)), message })
            }
            FixAcceptance::Rejected { reason, details } => {
                let gate = GateRecord {
                    gate: mage_core::domain::Gate {
                        id: self.d.ids.new_id(),
                        org_id: org,
                        run_id: rec.id(),
                        kind: GateKind::Fix,
                        attempt: n,
                        status: GateStatus::Pending.apply(GateEvent::Fail { reason: reason.clone(), at: now })?,
                        verify_run: None,
                    },
                    feedback: Some(details.clone()),
                    details: None,
                    created_at: now,
                };
                step(&mut rec, RunStatusEvent::ResultSubmitted { needs_gate: true }, Initiator::Runner, now, &mut w)?;
                self.fail_gate(&mut rec, &gate, reason, details.clone(), now, &mut w).await?;
                w.gates.push(GateWrite::Insert(gate.clone()));
                StoredSubmit::Rejected(Rejection { gate: Some(ticket(&gate)), message: details, items: Vec::new() })
            }
        };
        w.updates.push(RunUpdate::of(rec));
        Ok(Submitted::Commit { w, reply })
    }

    async fn submit_verify(&self, mut rec: RunRecord, v: VerifyResult) -> Result<Submitted, AppError> {
        let mut refs: Vec<(String, ArtifactId, Option<ArtifactKind>)> =
            v.artifacts.iter().enumerate().map(|(i, a)| (format!("artifacts[{i}]"), *a, None)).collect();
        if let Some(p) = v.rebased_patch {
            refs.push(("rebased_patch".into(), p, Some(ArtifactKind::Patch)));
        }
        let problems = self.check_artifacts(&rec, &refs).await?;
        if !problems.is_empty() {
            return Ok(invalid("verify artifacts are not usable", problems));
        }
        let now = self.now();
        let mut w = RunWrite::default();
        rec.run.set_result(RunResult::Verify(v))?;
        step(&mut rec, RunStatusEvent::ResultSubmitted { needs_gate: false }, Initiator::Runner, now, &mut w)?;
        w.updates.push(RunUpdate::of(rec));
        Ok(Submitted::Commit { w, reply: StoredSubmit::Ack(SubmitAck { status: RunStatus::Succeeded, gate: None, message: "Verify result recorded.".into() }) })
    }
}

/// The gate attempt `n`: existing gates of the kind plus one.
fn next_attempt(gates: &[GateRecord]) -> u8 {
    u8::try_from(gates.len()).unwrap_or(u8::MAX).saturating_add(1)
}
