//! Job handlers of intake.md: `intake.github`, `intake.github_fix`, `intake.linear`,
//! `intake.slack` (ProcessDelivery), `intake.linear_issue` (IngestLinearIssue), the periodic
//! chains `intake.github_resync`, `intake.linear_resync`, `intake.github_redeliver`,
//! `intake.inbox_purge` (Reconcile), and `notify.ticket` (Notify); [`IntakeChains`] bootstraps
//! the chains for the worker's periodic loop.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mage_core::domain::{JobKind, JobPayload, OrgId};
use mage_core::ports::jobs::{LeasedJob, NewJob};
use time::OffsetDateTime;

use crate::adapters::driving::worker::{JobDone, JobHandler, Periodic};
use crate::app::intake::IntakeError;
use crate::app::intake::deliver::ProcessDelivery;
use crate::app::intake::linear_issue::IngestLinearIssue;
use crate::app::intake::notify::Notify;
use crate::app::intake::reconcile::{ChainRun, Reconcile};
use crate::error::AppError;

const KINDS: [&str; 10] = [
    JobKind::IntakeGithub.as_str(),
    JobKind::IntakeGithubFix.as_str(),
    JobKind::IntakeLinear.as_str(),
    JobKind::IntakeSlack.as_str(),
    JobKind::IntakeLinearIssue.as_str(),
    JobKind::IntakeGithubResync.as_str(),
    JobKind::IntakeLinearResync.as_str(),
    JobKind::IntakeGithubRedeliver.as_str(),
    JobKind::IntakeInboxPurge.as_str(),
    JobKind::NotifyTicket.as_str(),
];

pub struct IntakeJobs {
    deliver: Arc<ProcessDelivery>,
    ingest: Arc<IngestLinearIssue>,
    reconcile: Arc<Reconcile>,
    notify: Arc<Notify>,
}

impl IntakeJobs {
    pub fn new(deliver: Arc<ProcessDelivery>, ingest: Arc<IngestLinearIssue>, reconcile: Arc<Reconcile>, notify: Arc<Notify>) -> Self {
        Self { deliver, ingest, reconcile, notify }
    }
}

/// A provider deadline retries at that time; other transient failures use the kind's backoff
/// (`AppError::Unavailable`); permanent ones kill the job (`AppError::Invalid`).
fn failed(err: IntakeError) -> Result<JobDone, AppError> {
    match err {
        IntakeError::Retry { reason, at: Some(at) } => Ok(JobDone::Retry { at, error: reason }),
        IntakeError::Retry { reason, at: None } => Err(AppError::Unavailable(reason)),
        IntakeError::Permanent(reason) => Err(AppError::Invalid(reason)),
    }
}

fn chain(run: Result<ChainRun, IntakeError>) -> Result<JobDone, AppError> {
    match run {
        Ok(run) => {
            tracing::info!(report = ?run.report, chain_continues = run.successor.is_some(), "intake chain run");
            Ok(JobDone::follow_up(run.successor.into_iter().collect()))
        }
        Err(e) => failed(e),
    }
}

#[async_trait]
impl JobHandler for IntakeJobs {
    fn kinds(&self) -> &'static [&'static str] {
        &KINDS
    }

    async fn handle(&self, job: &LeasedJob) -> Result<JobDone, AppError> {
        match &job.payload {
            JobPayload::IntakeDelivery { source, delivery_id } => match self.deliver.run(*source, delivery_id).await {
                Ok(outcome) => {
                    tracing::info!(source = source.as_str(), delivery_id = %delivery_id, outcome = outcome.as_str(), "webhook delivery processed");
                    Ok(JobDone::complete())
                }
                Err(e) => failed(e),
            },
            JobPayload::IntakeLinearIssue { org_id, issue_id } => match self.ingest.run(*org_id, issue_id).await {
                Ok(outcome) => {
                    tracing::info!(org = %org_id, issue_id = %issue_id, result = ?outcome.result, "linear issue ingested");
                    Ok(JobDone::follow_up(outcome.follow_ups))
                }
                Err(e) => failed(e),
            },
            JobPayload::IntakeGithubResync { org_id } => chain(self.reconcile.github_resync(*org_id).await),
            JobPayload::IntakeLinearResync { org_id, since } => chain(self.reconcile.linear_resync(*org_id, *since).await),
            JobPayload::IntakeGithubRedeliver { since } => chain(self.reconcile.github_redeliver(*since).await),
            JobPayload::IntakeInboxPurge => chain(self.reconcile.inbox_purge().await),
            JobPayload::NotifyTicket { org_id, ticket_id, notice } => match self.notify.run(*org_id, *ticket_id, notice).await {
                Ok(outcome) => {
                    tracing::info!(org = %org_id, ticket = %ticket_id, notice = %notice.key(), targets = ?outcome.targets, gone = outcome.ticket_gone, "notice delivered");
                    Ok(JobDone::complete())
                }
                Err(e) => failed(e),
            },
            other => Err(AppError::Invalid(format!("intake handler got job kind {}", other.kind()))),
        }
    }
}

/// The chain bootstrap at start and hourly (intake.md "Reconciliation"): `Reconcile::bootstrap`
/// for `orgs` (the tenants seeded at start) revives a dead resync / redeliver / purge chain and
/// is `Duplicate` while one is alive. It enqueues by itself, so it hands the worker no jobs.
pub struct IntakeChains {
    reconcile: Arc<Reconcile>,
    orgs: Vec<OrgId>,
}

impl IntakeChains {
    pub const PERIOD: Duration = Duration::from_secs(3600);

    pub fn new(reconcile: Arc<Reconcile>, orgs: Vec<OrgId>) -> Self {
        Self { reconcile, orgs }
    }
}

#[async_trait]
impl Periodic for IntakeChains {
    fn name(&self) -> &str {
        "intake.chains"
    }

    fn period(&self) -> Duration {
        Self::PERIOD
    }

    async fn jobs(&self, _now: OffsetDateTime) -> Result<Vec<NewJob>, AppError> {
        self.reconcile.bootstrap(&self.orgs).await?;
        Ok(Vec::new())
    }
}
