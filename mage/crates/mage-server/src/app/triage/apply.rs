//! UC4 `ApplyTriageOutcome` (job `runs.apply_outcome` for `RunKind::Triage`; triage.md "UC4").
//!
//! Every ticket write is fenced on `(status = Triaging, triage_run = run.id)` by the store, so an
//! old run's outcome can never overwrite a newer triage, and a replayed job is a no-op.

use std::sync::Arc;

use async_trait::async_trait;

use mage_core::domain::evidence::{self, ManifestView};
use mage_core::domain::{
    Cause, CauseId, JobPayload, Notice, OrgId, RepoPath, RepoVersionKey, RunErrorCode, RunId, RunKind, RunResult, RunStatus, RunSubject,
    Ticket, TicketStatus, TriageResult, TriageTrigger,
};
use mage_core::ports::jobs::{JobQueue, NewJob};
use mage_core::ports::{Clock, IdGen, KnowledgeStore};

use crate::app::knowledge_query::SourceReader;
use crate::app::runs::KindOutcome;
use crate::app::triage::config::TriageConfig;
use crate::app::triage::eval::eval_case_result;
use crate::app::triage::job_error;
use crate::app::triage::view::load_manifest_view;
use crate::domain::triage::{TriageMode, TriageSpec};
use crate::error::AppError;
use crate::ports::runs::{RunRecord, RunStore};
use crate::ports::ticket::{TicketStore, TicketStoreError, TriageWrite};
use crate::ports::triage_eval::TriageEvalStore;

pub struct ApplyTriageDeps {
    pub tickets: Arc<dyn TicketStore>,
    pub runs: Arc<dyn RunStore>,
    pub knowledge: Arc<dyn KnowledgeStore>,
    pub reader: Arc<SourceReader>,
    pub evals: Arc<dyn TriageEvalStore>,
    pub jobs: Arc<dyn JobQueue>,
    pub clock: Arc<dyn Clock>,
    pub ids: Arc<dyn IdGen>,
}

pub struct ApplyTriageOutcome {
    d: ApplyTriageDeps,
    cfg: TriageConfig,
}

impl ApplyTriageOutcome {
    pub fn new(deps: ApplyTriageDeps, cfg: TriageConfig) -> Self {
        Self { d: deps, cfg }
    }

    /// No-op for non-terminal runs (the job also fires at creation).
    pub async fn run(&self, org: OrgId, run: RunId) -> Result<(), AppError> {
        let rec = self.d.runs.get(org, run).await?;
        if rec.run.kind != RunKind::Triage {
            return Err(AppError::Invalid(format!("run {run} is not a triage run")));
        }
        if !rec.run.status.is_terminal() {
            return Ok(());
        }
        let spec = rec.spec.triage.as_ref().ok_or_else(|| AppError::Internal(format!("triage run {run} has no triage spec")))?;
        emit_run_metrics(&rec);
        if let TriageMode::Eval { eval, repeat, .. } = spec.mode {
            return self.record_eval_case(org, &rec, eval, repeat).await;
        }
        let RunSubject::Ticket(ticket_id) = rec.run.subject else {
            return Err(AppError::Internal(format!("triage run {run} has subject {:?}", rec.run.subject)));
        };
        let ticket = match self.d.tickets.get_ticket(org, ticket_id).await {
            Ok(v) => v.value,
            Err(TicketStoreError::NotFound) => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        if ticket.triage_run != Some(run) || ticket.status != TicketStatus::Triaging {
            tracing::info!(%run, ticket = %ticket.id, status = ticket.status.name(), "triage outcome kept on the run only: the ticket moved on");
            return Ok(());
        }
        match rec.run.status {
            RunStatus::Succeeded => self.apply_success(org, &rec, spec, &ticket).await,
            _ => self.apply_failure(org, &rec, spec, &ticket).await,
        }
    }

    async fn apply_success(&self, org: OrgId, rec: &RunRecord, spec: &TriageSpec, ticket: &Ticket) -> Result<(), AppError> {
        let Some(RunResult::Triage(result)) = &rec.run.result else {
            return Err(AppError::Internal(format!("succeeded triage run {} has no triage result", rec.run.id)));
        };
        let key = RepoVersionKey::new(spec.input.target.repo_id, spec.input.target.sha);
        let paths: Vec<RepoPath> = result.causes.iter().flat_map(|c| c.evidence.iter().map(|e| e.path.clone())).collect();
        let view = load_manifest_view(self.d.knowledge.as_ref(), &self.d.reader, org, key, &paths).await?;
        let causes = rebuild_causes(org, ticket, rec.run.id, result, &view, || self.d.ids.new_id());
        let stored = causes.len();
        let write = TriageWrite {
            ticket: ticket.id,
            expect: TicketStatus::Triaging,
            content_hash: spec.input.content_hash,
            triage_run: rec.run.id,
            causes,
            next: TicketStatus::Triaged,
        };
        let notify = NewJob::new(JobPayload::NotifyTicket {
            org_id: org,
            ticket_id: ticket.id,
            notice: Notice::TriageReady { run_id: rec.run.id },
        });
        match self.d.tickets.apply_triage(org, write, &[notify]).await {
            Ok(_) => {
                tracing::info!(target: "mage.triage", run = %rec.run.id, ticket = %ticket.id, causes = stored, "triage applied");
                Ok(())
            }
            Err(TicketStoreError::Conflict) => {
                tracing::info!(run = %rec.run.id, ticket = %ticket.id, "triage outcome not applied: the ticket changed");
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    }

    async fn apply_failure(&self, org: OrgId, rec: &RunRecord, spec: &TriageSpec, ticket: &Ticket) -> Result<(), AppError> {
        let error_code = rec.run.error.as_ref().map(|e| e.code);
        let retryable = !matches!(error_code, Some(RunErrorCode::NoResult | RunErrorCode::ModelAuth));
        if rec.run.status == RunStatus::Errored && retryable && spec.trigger != TriageTrigger::Requeue {
            let at = self.d.clock.now() + self.cfg.requeue_delay;
            let job = NewJob::new(JobPayload::TriageTicket {
                org_id: org,
                ticket_id: ticket.id,
                content_hash: spec.input.content_hash,
                trigger: TriageTrigger::Requeue,
            })
            .after(at);
            self.d.jobs.enqueue(job).await.map_err(job_error)?;
            tracing::info!(run = %rec.run.id, ticket = %ticket.id, ?error_code, "errored triage requeued once");
            return Ok(());
        }
        match self.d.tickets.fail_triage(org, ticket.id, rec.run.id, &[]).await {
            Ok(_) | Err(TicketStoreError::Conflict) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    async fn record_eval_case(&self, org: OrgId, rec: &RunRecord, eval: mage_core::domain::TriageEvalId, repeat: u8) -> Result<(), AppError> {
        let RunSubject::Ticket(ticket) = rec.run.subject else {
            return Err(AppError::Internal(format!("eval triage run {} has subject {:?}", rec.run.id, rec.run.subject)));
        };
        let labels = self.d.evals.labels(org, &[ticket]).await?;
        let gates = self.d.runs.gates(org, rec.run.id).await?;
        let label = labels.iter().find(|l| l.ticket == ticket);
        let result = eval_case_result(eval, ticket, repeat, rec, label, &gates)?;
        self.d.evals.record_case_result(org, &result).await?;
        Ok(())
    }
}

#[async_trait]
impl KindOutcome for ApplyTriageOutcome {
    async fn apply(&self, org: OrgId, run: RunId) -> Result<(), AppError> {
        self.run(org, run).await
    }
}

/// Re-derives `EvidenceChecked` for every accepted draft and builds the causes in rank order.
/// A draft that fails the re-check (index corruption: manifests at a sha are immutable) is
/// dropped and logged; ranks stay contiguous.
pub(crate) fn rebuild_causes(
    org: OrgId,
    ticket: &Ticket,
    run: RunId,
    result: &TriageResult,
    view: &ManifestView,
    mut new_id: impl FnMut() -> CauseId,
) -> Vec<Cause> {
    let mut causes: Vec<Cause> = Vec::with_capacity(result.causes.len());
    for (i, draft) in result.causes.iter().enumerate() {
        let rank = u16::try_from(causes.len() + 1).unwrap_or(u16::MAX);
        let built = evidence::check(&draft.evidence, view)
            .into_checked()
            .and_then(|proof| Cause::new(new_id(), org, ticket, run, rank, draft.clone(), proof));
        match built {
            Ok(cause) => causes.push(cause),
            Err(e) => tracing::error!(
                target: "mage.triage",
                %run,
                cause_index = i,
                error = %e,
                "accepted triage cause failed the evidence re-check; dropped"
            ),
        }
    }
    causes
}

/// `mage.triage.runs{status,harness,model}` and `mage.triage.cost_micros` as tracing events.
fn emit_run_metrics(rec: &RunRecord) {
    tracing::info!(
        target: "mage.triage",
        metric = "mage.triage.runs",
        run = %rec.run.id,
        status = rec.run.status.name(),
        harness = ?rec.run.harness,
        model = %rec.spec.model,
        cost_micros = rec.run.usage.cost_micros,
        eval = rec.spec.triage.as_ref().is_some_and(TriageSpec::is_eval),
        "triage run finished"
    );
}

#[cfg(test)]
#[path = "apply_tests.rs"]
mod tests;
