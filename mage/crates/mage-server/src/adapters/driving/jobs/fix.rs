//! `fix.*` job handlers (fix.md): `fix.brief` → `BuildFixBrief`, `fix.open_pr` →
//! `OpenPullRequest` (deferred while a rebase Verify run decides), `fix.pr_event` →
//! `OnPullRequestEvent`. Error mapping is the worker's: `Invalid` / `Forbidden` (missing GitHub
//! permissions) die without retry, `RateLimited` retries after the provider's wait,
//! `Unavailable` / `Conflict` retry with backoff.

use std::sync::Arc;

use async_trait::async_trait;
use mage_core::domain::JobPayload;
use mage_core::ports::jobs::LeasedJob;

use crate::adapters::driving::worker::{JobDone, JobHandler};
use crate::app::fix::{BriefOutcome, BuildFixBrief, FixDeps, OnPullRequestEvent, OpenPrOutcome, OpenPullRequest};
use crate::error::AppError;

pub struct FixJobs {
    brief: BuildFixBrief,
    open_pr: OpenPullRequest,
    pr_event: OnPullRequestEvent,
}

impl FixJobs {
    pub fn new(d: Arc<FixDeps>) -> Self {
        Self { brief: BuildFixBrief::new(d.clone()), open_pr: OpenPullRequest::new(d.clone()), pr_event: OnPullRequestEvent::new(d) }
    }
}

#[async_trait]
impl JobHandler for FixJobs {
    fn kinds(&self) -> &'static [&'static str] {
        &["fix.brief", "fix.open_pr", "fix.pr_event"]
    }

    async fn handle(&self, job: &LeasedJob) -> Result<JobDone, AppError> {
        match &job.payload {
            JobPayload::FixBrief { org_id, cause_id, diagnose_run_id } => {
                let outcome = self.brief.run(*org_id, *cause_id, *diagnose_run_id).await?;
                if !matches!(outcome, BriefOutcome::Written { .. }) {
                    tracing::info!(cause = %cause_id, ?outcome, "fix brief not written");
                }
                Ok(JobDone::complete())
            }
            JobPayload::FixOpenPr { org_id, ticket_id, fix_run_id } => match self.open_pr.run(*org_id, *ticket_id, *fix_run_id, None).await? {
                OpenPrOutcome::WaitingForRebase { until, .. } => Ok(JobDone::Defer { until }),
                OpenPrOutcome::Opened { number, .. } => {
                    tracing::info!(ticket = %ticket_id, number, "fix PR open");
                    Ok(JobDone::complete())
                }
                OpenPrOutcome::NotOpened { reason } => {
                    tracing::info!(ticket = %ticket_id, %reason, "no fix PR");
                    Ok(JobDone::complete())
                }
            },
            JobPayload::FixPrEvent { org_id, repo_id, number, action, merged } => {
                self.pr_event.run(*org_id, *repo_id, *number, action, *merged).await?;
                Ok(JobDone::complete())
            }
            other => Err(AppError::Internal(format!("fix handler leased a {} job", other.kind()))),
        }
    }
}
