//! ApplyOutcome (job `runs.apply_outcome`, runs.md): projects a run's status onto the domain.
//! Idempotent: every projection is guarded by "the subject still points at this run".
//!
//! | Kind | Handler |
//! |---|---|
//! | Triage | `hooks.triage_outcome` |
//! | Fix | `hooks.fix_outcome` |
//! | Verify | `EvaluateGate` (gate-less Verify runs → `hooks.fix_outcome`) |
//! | Learn | Boot proof → `Recipe::validate`, SCIP ingest; failed Boot of the validated recipe → `Broken` |
//! | Diagnose | cause verdict, `notify.ticket{DiagnosisVerdict}`, `fix.brief`, ticket `Diagnosing → Diagnosed/Triaged` |

use mage_core::domain::{
    ArtifactKind, BudgetLimit, Cause, CauseStatus, GateKind, GateStatus, JobPayload, Notice, OrgId, Recipe, RecipeStatus, RunId, RunKind, RunResult,
    RunStatus, RunSubject, TicketEvent, TicketStatus, TriageTrigger, Verdict,
};
use mage_core::ports::jobs::NewJob;

use crate::app::runs::{RunManager, job_err, repo_err};
use crate::error::AppError;
use crate::ports::runs::{ArtifactState, GateRecord, RunQuery, RunRecord};
use crate::ports::ticket::{TicketStoreError, TicketTransition};

const OPEN: [RunStatus; 5] = [RunStatus::Queued, RunStatus::Leased, RunStatus::Preparing, RunStatus::Running, RunStatus::AwaitingGate];

/// A job failure that must not be retried (stored results no longer re-derive a proof).
fn proof_failure(what: &str, why: impl std::fmt::Display) -> AppError {
    tracing::error!(%why, "{what}: gate proof could not be re-derived");
    AppError::Invalid(format!("{what}: gate proof could not be re-derived: {why}"))
}

impl RunManager {
    /// Job `runs.apply_outcome { run_id }`.
    pub async fn apply_outcome(&self, org: OrgId, run: RunId) -> Result<(), AppError> {
        let rec = self.d.runs.get(org, run).await?;
        match rec.run.kind {
            RunKind::Triage => self.d.hooks.triage_outcome.apply(org, run).await,
            RunKind::Fix => self.d.hooks.fix_outcome.apply(org, run).await,
            RunKind::Verify => self.evaluate_gate(org, run).await,
            RunKind::Learn => self.learn_outcome(&rec).await,
            RunKind::Diagnose => self.diagnose_outcome(&rec).await,
        }
    }

    /// The newest gate of `kind` with `status` and its Verify run.
    async fn gate_with_verify(&self, rec: &RunRecord, kind: GateKind, passed: bool) -> Result<Option<(GateRecord, RunRecord)>, AppError> {
        let gates = self.gates_of(rec.org(), rec.id(), kind).await?;
        let wanted = |s: &GateStatus| if passed { matches!(s, GateStatus::Passed { .. }) } else { matches!(s, GateStatus::Failed { .. }) };
        let Some(gate) = gates.into_iter().find(|g| wanted(&g.gate.status)) else { return Ok(None) };
        let Some(v) = gate.gate.verify_run else { return Ok(Some((gate.clone(), rec.clone()))) };
        let verify = self.d.runs.get(rec.org(), v).await?;
        Ok(Some((gate, verify)))
    }

    async fn learn_outcome(&self, rec: &RunRecord) -> Result<(), AppError> {
        let org = rec.org();
        let Some(repo) = rec.spec.repo_id else { return Ok(()) };
        let Some(RunResult::Learn(proposal)) = &rec.run.result else { return Ok(()) };
        let hash = Recipe::draft(org, repo, proposal.toml.clone())?.hash;
        match rec.run.status {
            RunStatus::Succeeded => {
                let Some((gate, verify)) = self.gate_with_verify(rec, GateKind::Boot, true).await? else {
                    return Err(proof_failure("learn", "no passed boot gate"));
                };
                let Some(RunResult::Verify(result)) = &verify.run.result else { return Err(proof_failure("learn", "verify result missing")) };
                let proof = self.boot_gate(rec, &gate, &verify, result).map_err(|f| proof_failure("learn", f))?;
                if proof.recipe() != hash {
                    return Err(proof_failure("learn", "boot gate verified another recipe"));
                }
                let mut recipe = self.d.repos.get_recipe(org, repo, hash).await.map_err(repo_err)?;
                let already = matches!(recipe.status, RecipeStatus::Validated { .. }) && recipe.validated_at_sha == Some(proof.sha());
                if !already {
                    let expect = recipe.status.clone();
                    recipe.validate(proof, self.now())?;
                    self.d.repos.update_recipe(org, &recipe, &expect, &[]).await.map_err(repo_err)?;
                }
                let sha = rec.spec.sha;
                let jobs: Vec<NewJob> = self
                    .d
                    .runs
                    .artifacts(org, rec.id())
                    .await?
                    .into_iter()
                    .filter(|a| a.artifact.kind == ArtifactKind::ScipIndex && a.state == ArtifactState::Complete)
                    .filter_map(|a| {
                        sha.map(|sha| {
                            NewJob::new(JobPayload::IndexIngestScip {
                                org_id: org,
                                repo_id: repo,
                                sha,
                                run_id: rec.id(),
                                artifact_id: a.artifact.id,
                                key: a.artifact.key.clone(),
                            })
                        })
                    })
                    .collect();
                if !jobs.is_empty() {
                    self.d.jobs.enqueue_all(&jobs).await.map_err(job_err)?;
                }
                Ok(())
            }
            RunStatus::Failed => {
                let Some(current) = self.d.repos.latest_recipe(org, repo).await.map_err(repo_err)? else { return Ok(()) };
                if current.hash != hash || !matches!(current.status, RecipeStatus::Validated { .. }) {
                    return Ok(());
                }
                let reason = match self.gate_with_verify(rec, GateKind::Boot, false).await? {
                    Some((g, _)) => match g.gate.status {
                        GateStatus::Failed { reason, .. } => reason,
                        _ => "boot gate failed".into(),
                    },
                    None => "boot gate failed".into(),
                };
                let mut recipe = current;
                let expect = recipe.status.clone();
                recipe.mark_broken(format!("boot gate failed at {}: {reason}", rec.spec.sha.map_or_else(String::new, |s| s.to_string())), self.now())?;
                self.d.repos.update_recipe(org, &recipe, &expect, &[]).await.map_err(repo_err)
            }
            _ => Ok(()),
        }
    }

    async fn diagnose_outcome(&self, rec: &RunRecord) -> Result<(), AppError> {
        let org = rec.org();
        let RunSubject::Cause(cause_id) = rec.run.subject else { return Ok(()) };
        let cause = self.d.tickets.get_cause(org, cause_id).await?;
        let ticket = self.d.tickets.get_ticket(org, cause.ticket_id).await?.value;
        let startable = matches!(cause.status, CauseStatus::Untested | CauseStatus::NotReproduced { .. } | CauseStatus::Inconclusive { .. });
        let owns = cause.status.running() == Some(rec.id());

        if !rec.run.status.is_terminal() {
            if owns || !startable {
                return Ok(());
            }
            let mut next = cause.clone();
            next.start_run(rec.id())?;
            let transition = (ticket.status == TicketStatus::Triaged).then(|| {
                ticket.status.apply(&TicketEvent::DiagnoseStarted).map(|n| TicketTransition { expect: TicketStatus::Triaged, next: n })
            });
            let transition = transition.transpose()?;
            return self.set_cause(org, &cause, &next, transition, &[]).await;
        }

        if !owns {
            // The creation projection never ran (both jobs raced): start + verdict in one CAS, but
            // only for the cause's newest Diagnose run.
            let newest = self
                .d
                .runs
                .list(org, &RunQuery { kind: Some(RunKind::Diagnose), subject: Some(rec.run.subject), newest_first: true, limit: 1, ..RunQuery::default() })
                .await?;
            if !startable || newest.first().map(|r| r.id()) != Some(rec.id()) {
                return Ok(());
            }
        }
        let mut next = cause.clone();
        if !owns {
            next.start_run(rec.id())?;
        }
        let mut notify = true;
        match rec.run.status {
            RunStatus::Succeeded => match &rec.run.result {
                Some(RunResult::Diagnose(d)) => match &d.verdict {
                    Verdict::Reproduced => {
                        let Some((gate, verify)) = self.gate_with_verify(rec, GateKind::Repro, true).await? else {
                            return Err(proof_failure("diagnose", "no passed repro gate"));
                        };
                        let Some(RunResult::Verify(result)) = &verify.run.result else { return Err(proof_failure("diagnose", "verify result missing")) };
                        let proof = self.repro_gate(rec, &gate, &verify, result).map_err(|f| proof_failure("diagnose", f))?;
                        next.mark_reproduced(proof)?;
                    }
                    Verdict::NotReproduced => next.record_not_reproduced(rec.id())?,
                    Verdict::Inconclusive { reason } => next.record_inconclusive(rec.id(), reason.clone())?,
                },
                _ => return Err(AppError::Internal(format!("diagnose run {} succeeded without a diagnosis", rec.id()))),
            },
            RunStatus::Failed => {
                let reason = match self.gate_with_verify(rec, GateKind::Repro, false).await? {
                    Some((g, _)) => match g.gate.status {
                        GateStatus::Failed { reason, .. } => reason,
                        _ => String::new(),
                    },
                    None => String::new(),
                };
                next.record_inconclusive(rec.id(), format!("claimed reproduced; repro gate failed: {reason}"))?;
            }
            RunStatus::TimedOut => {
                let at = rec.run.finished_at.unwrap_or_else(|| self.now());
                let limit = rec.effective_usage(at).exceeds(&rec.run.budget).unwrap_or(BudgetLimit::Wall);
                next.record_inconclusive(rec.id(), format!("budget: {limit}"))?;
            }
            _ => {
                notify = false;
                next.restore(self.previous_verdict(org, cause_id, rec.id()).await?)?;
            }
        }

        let mut jobs = Vec::new();
        if notify {
            jobs.push(NewJob::new(JobPayload::NotifyTicket {
                org_id: org,
                ticket_id: ticket.id,
                notice: Notice::DiagnosisVerdict { cause_id, run_id: rec.id() },
            }));
        }
        let reproduced = matches!(next.status, CauseStatus::Reproduced { .. });
        if reproduced && self.d.orgs.fix_settings(org).await?.brief {
            jobs.push(NewJob::new(JobPayload::FixBrief { org_id: org, cause_id, diagnose_run_id: rec.id() }));
        }
        let mut transition = None;
        if ticket.status == TicketStatus::Diagnosing {
            let others = self
                .d
                .runs
                .list(org, &RunQuery { kind: Some(RunKind::Diagnose), ticket: Some(ticket.id), statuses: OPEN.to_vec(), limit: 2, ..RunQuery::default() })
                .await?;
            if others.iter().all(|r| r.id() == rec.id()) {
                let causes = self.d.tickets.causes(org, ticket.id, false).await?;
                let any_reproduced = reproduced || causes.iter().any(|c| c.id != cause_id && matches!(c.status, CauseStatus::Reproduced { .. }));
                let n = ticket.status.apply(&TicketEvent::DiagnosesFinished { any_reproduced })?;
                if n == TicketStatus::Triaged && ticket.content_changed_since_triage() {
                    jobs.push(NewJob::new(JobPayload::TriageTicket {
                        org_id: org,
                        ticket_id: ticket.id,
                        content_hash: ticket.content_hash,
                        trigger: TriageTrigger::Auto,
                    }));
                }
                transition = Some(TicketTransition { expect: TicketStatus::Diagnosing, next: n });
            }
        }
        self.set_cause(org, &cause, &next, transition, &jobs).await
    }

    async fn set_cause(&self, org: OrgId, before: &Cause, after: &Cause, ticket: Option<TicketTransition>, jobs: &[NewJob]) -> Result<(), AppError> {
        match self.d.tickets.set_cause_status(org, before.id, &before.status, after.status.clone(), ticket, jobs).await {
            Ok(_) => Ok(()),
            Err(TicketStoreError::Conflict) => Err(AppError::Conflict("cause or ticket changed concurrently".into())),
            Err(e) => Err(e.into()),
        }
    }

    /// The verdict of the newest earlier terminal Diagnose run of `cause` that produced one
    /// (`NotReproduced` / `Inconclusive`), else `Untested`.
    async fn previous_verdict(&self, org: OrgId, cause: mage_core::domain::CauseId, current: RunId) -> Result<CauseStatus, AppError> {
        let runs = self
            .d
            .runs
            .list(org, &RunQuery { kind: Some(RunKind::Diagnose), subject: Some(RunSubject::Cause(cause)), newest_first: true, limit: RunQuery::MAX_LIMIT, ..RunQuery::default() })
            .await?;
        for r in runs.iter().filter(|r| r.id() != current && r.run.status.is_terminal()) {
            let status = match (r.run.status, &r.run.result) {
                (RunStatus::Succeeded, Some(RunResult::Diagnose(d))) => match &d.verdict {
                    Verdict::NotReproduced => Some(CauseStatus::NotReproduced { run: r.id() }),
                    Verdict::Inconclusive { reason } => Some(CauseStatus::Inconclusive { run: r.id(), reason: reason.clone() }),
                    Verdict::Reproduced => None,
                },
                (RunStatus::Failed, _) => Some(CauseStatus::Inconclusive { run: r.id(), reason: "claimed reproduced; repro gate failed".into() }),
                (RunStatus::TimedOut, _) => Some(CauseStatus::Inconclusive { run: r.id(), reason: "budget".into() }),
                _ => None,
            };
            if let Some(s) = status {
                return Ok(s);
            }
        }
        Ok(CauseStatus::Untested)
    }
}
