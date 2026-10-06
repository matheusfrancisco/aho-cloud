//! `runs.*` job handlers (runs.md): `runs.apply_outcome`, `runs.source_ready`,
//! `runs.reap_leases` (reschedules itself every `reap_every_secs`).

use async_trait::async_trait;

use mage_core::domain::JobPayload;
use mage_core::ports::jobs::LeasedJob;

use crate::adapters::driving::worker::{JobDone, JobHandler};
use crate::app::runs::RunManager;
use crate::app::runs::reap::REAP_BATCH;
use crate::error::AppError;

pub struct RunJobs {
    runs: RunManager,
}

impl RunJobs {
    pub fn new(runs: RunManager) -> RunJobs {
        RunJobs { runs }
    }
}

#[async_trait]
impl JobHandler for RunJobs {
    fn kinds(&self) -> &'static [&'static str] {
        &["runs.apply_outcome", "runs.source_ready", "runs.reap_leases"]
    }

    async fn handle(&self, job: &LeasedJob) -> Result<JobDone, AppError> {
        match &job.payload {
            JobPayload::RunsApplyOutcome { org_id, run_id } => {
                self.runs.apply_outcome(*org_id, *run_id).await?;
                Ok(JobDone::complete())
            }
            JobPayload::RunsSourceReady { org_id, repo_id, sha } => {
                self.runs.source_ready(*org_id, *repo_id, *sha).await?;
                Ok(JobDone::complete())
            }
            JobPayload::RunsReapLeases => {
                let report = self.runs.reap_leases(REAP_BATCH).await?;
                if report.handled > 0 {
                    tracing::info!(handled = report.handled, skipped = report.skipped, "reaped run leases");
                }
                Ok(JobDone::follow_up(vec![self.runs.next_reaper_job()]))
            }
            other => Err(AppError::Invalid(format!("runs handler got {}", other.kind()))),
        }
    }
}
