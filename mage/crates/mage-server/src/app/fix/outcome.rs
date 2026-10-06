//! `ApplyFixOutcome` (fix.md F2 "Gate passed", "Ticket transitions used", rebase flow step 5):
//! the `runs.apply_outcome` projection for Fix runs and for the fix module's gate-less Verify runs.
//! Idempotent on replay: every ticket write is a status CAS guarded by the current state.
//!
//! | Run | Effect |
//! |---|---|
//! | Fix `Succeeded` | re-derive `FixGatePassed`; `Ticket::fix_ready` → `Fixing → FixReady` + `notify.ticket{FixReady}` (+ `fix.open_pr` when `open_pr = auto`), one transaction |
//! | Fix `Failed` / `TimedOut` / `Errored` | `Fixing → Diagnosed` + `notify.ticket{FixFailed}` |
//! | Fix `Cancelled` | `Fixing → Diagnosed` |
//! | Fix non-terminal | nothing (`StartFixRun` already moved the ticket) |
//! | Verify `TestBaseline` red | cancel the still-running Fix run (`BaselineRed`) |
//! | Verify rebase (terminal) | re-schedule `fix.open_pr` |

use std::sync::Arc;

use async_trait::async_trait;
use mage_core::domain::fix::OpenPrMode;
use mage_core::domain::{Notice, OrgId, RunId, RunKind, RunStatus, Ticket, TicketStatus, VerifySpec};

use crate::app::fix::proof::{FixProof, cause_of, is_rebase_spec};
use crate::app::fix::start::{Baseline, baseline_of};
use crate::app::fix::{FixDeps, notify, open_pr_job};
use crate::app::runs::{CancelRun, KindOutcome};
use crate::error::AppError;
use crate::ports::runs::RunRecord;
use crate::ports::ticket::TicketStoreError;

#[derive(Clone)]
pub struct ApplyFixOutcome {
    d: Arc<FixDeps>,
    proof: FixProof,
}

impl ApplyFixOutcome {
    pub fn new(d: Arc<FixDeps>) -> Self {
        let proof = FixProof::new(d.runs.clone());
        Self { d, proof }
    }

    pub async fn apply_run(&self, org: OrgId, run: RunId) -> Result<(), AppError> {
        let rec = self.d.runs.get(org, run).await?;
        match rec.run.kind {
            RunKind::Fix => self.fix_run(org, &rec).await,
            RunKind::Verify => self.verify_run(org, &rec).await,
            other => Err(AppError::Invalid(format!("fix outcome called for a {other:?} run"))),
        }
    }

    async fn fix_run(&self, org: OrgId, rec: &RunRecord) -> Result<(), AppError> {
        let status = rec.run.status;
        if !status.is_terminal() {
            return Ok(());
        }
        let cause_id = cause_of(rec)?;
        let cause = self.d.tickets.get_cause(org, cause_id).await?;
        let ticket = self.d.tickets.get_ticket(org, cause.ticket_id).await?.value;
        if ticket.status != TicketStatus::Fixing {
            // Already projected (replay), or the ticket moved on (resolved, a newer fix run).
            return Ok(());
        }
        if status == RunStatus::Succeeded {
            let proof = self.proof.gate_proof(org, rec.id()).await?;
            // Domain check of the transition with the proof; the store applies it as a CAS.
            let mut next: Ticket = ticket.clone();
            next.fix_ready(&proof.token, self.d.clock.now())?;
            let settings = self.d.orgs.fix_settings(org).await?;
            let mut jobs = vec![notify(org, ticket.id, Notice::FixReady { cause_id, run_id: rec.id() })];
            if settings.pull_requests && settings.open_pr == OpenPrMode::Auto {
                jobs.push(open_pr_job(org, ticket.id, rec.id(), &proof.token.digest().to_hex()));
            }
            return self.transition(org, &ticket, next.status, &jobs).await;
        }
        let mut jobs = Vec::new();
        if matches!(status, RunStatus::Failed | RunStatus::TimedOut | RunStatus::Errored) {
            jobs.push(notify(org, ticket.id, Notice::FixFailed { cause_id, run_id: rec.id() }));
        }
        let next = ticket.status.apply(&mage_core::domain::TicketEvent::FixEnded)?;
        self.transition(org, &ticket, next, &jobs).await
    }

    async fn transition(&self, org: OrgId, ticket: &Ticket, next: TicketStatus, jobs: &[mage_core::ports::jobs::NewJob]) -> Result<(), AppError> {
        match self.d.tickets.transition(org, ticket.id, ticket.status, next, jobs).await {
            Ok(_) => Ok(()),
            // Lost a race with another writer: the ticket is no longer `Fixing`; nothing to do.
            Err(TicketStoreError::Conflict) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    async fn verify_run(&self, org: OrgId, rec: &RunRecord) -> Result<(), AppError> {
        if !rec.run.status.is_terminal() {
            return Ok(());
        }
        let Some(parent_id) = rec.run.parent else { return Ok(()) };
        let parent = self.d.runs.get(org, parent_id).await?;
        if parent.run.kind != RunKind::Fix {
            return Ok(());
        }
        match rec.spec.verify.as_ref() {
            Some(VerifySpec::TestBaseline) => {
                if baseline_of(rec) == Baseline::Red && !parent.run.status.is_terminal() {
                    let rm = self.d.run_manager.get()?;
                    rm.cancel_run(CancelRun {
                        org,
                        run: parent_id,
                        by: None,
                        notice: Some(
                            "BaselineRed: the project test command fails on the unpatched base, so the fix gate cannot pass. \
                             Fix the baseline (or the recipe's test command) first."
                                .into(),
                        ),
                    })
                    .await?;
                }
                Ok(())
            }
            spec if is_rebase_spec(spec) => {
                // The open_pr job waits on this run (deferred); enqueue it now. While the deferred
                // job is still queued this is a duplicate and it picks the result up on its next look.
                let proof = self.proof.gate_proof(org, parent_id).await?;
                let cause = self.d.tickets.get_cause(org, proof.token.cause()).await?;
                let job = open_pr_job(org, cause.ticket_id, parent_id, &proof.token.digest().to_hex());
                self.d.jobs.enqueue(job).await.map_err(crate::app::fix::job_err)?;
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

#[async_trait]
impl KindOutcome for ApplyFixOutcome {
    async fn apply(&self, org: OrgId, run: RunId) -> Result<(), AppError> {
        self.apply_run(org, run).await
    }
}
