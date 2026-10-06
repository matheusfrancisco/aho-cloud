//! Job handlers for `triage.ticket` (`StartTriage`) and `triage.eval_case` (`StartEvalCase`).
//! `runs.apply_outcome` for Triage runs is dispatched by the runs handler to
//! `app::triage::ApplyTriageOutcome`.

use std::sync::Arc;

use async_trait::async_trait;

use mage_core::domain::JobKind;
use mage_core::ports::jobs::LeasedJob;

use crate::adapters::driving::worker::{JobDone, JobHandler};
use crate::app::triage::start::TriageTicketPayload;
use crate::app::triage::{StartEvalCase, StartOutcome, StartTriage};
use crate::error::AppError;

const KINDS: [&str; 2] = [JobKind::TriageTicket.as_str(), JobKind::TriageEvalCase.as_str()];

pub struct TriageJobs {
    start: Arc<StartTriage>,
    eval_case: Arc<StartEvalCase>,
}

impl TriageJobs {
    pub fn new(start: Arc<StartTriage>, eval_case: Arc<StartEvalCase>) -> Self {
        Self { start, eval_case }
    }
}

/// Started and skipped jobs complete; deferred ones go back to the queue without using an
/// attempt (`JobQueue::defer`).
fn done(outcome: StartOutcome) -> JobDone {
    match outcome {
        StartOutcome::Started { .. } | StartOutcome::Skipped(_) => JobDone::complete(),
        StartOutcome::Deferred { until, .. } => JobDone::Defer { until },
    }
}

#[async_trait]
impl JobHandler for TriageJobs {
    fn kinds(&self) -> &'static [&'static str] {
        &KINDS
    }

    async fn handle(&self, job: &LeasedJob) -> Result<JobDone, AppError> {
        match job.kind {
            JobKind::TriageTicket => {
                let payload = TriageTicketPayload::from_payload(&job.payload)
                    .ok_or_else(|| AppError::Invalid(format!("job {} carries a {} payload", job.kind, job.payload.kind())))?;
                Ok(done(self.start.run(job, &payload).await?))
            }
            JobKind::TriageEvalCase => Ok(done(self.eval_case.run(job).await?)),
            other => Err(AppError::Invalid(format!("triage handler got job kind {other}"))),
        }
    }
}
