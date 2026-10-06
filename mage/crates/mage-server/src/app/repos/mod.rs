//! Repos, versions, recipes, deployments and customers for the UI API (server.md "Endpoint
//! catalog"; storage.md `RepoStore` / `OrgStore`; knowledge.md "Which shas get indexed";
//! intake.md deployments and customers).
//!
//! Every call takes the caller's `OrgId` and passes it to every store call, so another org's ids
//! are `NotFound`. Role checks are the driving adapter's (server.md auth column).

mod catalog;
mod connect;
mod customers;
mod deployments;
mod recipes;

#[cfg(test)]
pub(crate) mod tests;

use std::sync::Arc;

use mage_core::domain::{
    CommitSha, InstallationId, IntegrationKind, IntegrationStatus, JobPayload, OrgId, Repo, RepoAccess, RepoId, VersionReason,
};
use mage_core::ports::jobs::{JobError, NewJob};
use mage_core::ports::source_host::RepoCoords;
use mage_core::ports::{Clock, IdGen, JobQueue, Page, PageRequest, RepoStore, RepoStoreError, SourceHost};
use mage_core::wire::api::JobQueued;

use crate::error::AppError;
use crate::ports::org::OrgStore;
use crate::ports::runs::RunStore;

/// Page size used when a UI list reads a whole store listing.
const LIST_PAGE: u16 = PageRequest::MAX_LIMIT;

pub struct ReposService {
    repos: Arc<dyn RepoStore>,
    orgs: Arc<dyn OrgStore>,
    runs: Arc<dyn RunStore>,
    source: Arc<dyn SourceHost>,
    jobs: Arc<dyn JobQueue>,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGen>,
}

impl ReposService {
    pub fn new(
        repos: Arc<dyn RepoStore>,
        orgs: Arc<dyn OrgStore>,
        runs: Arc<dyn RunStore>,
        source: Arc<dyn SourceHost>,
        jobs: Arc<dyn JobQueue>,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGen>,
    ) -> Self {
        Self { repos, orgs, runs, source, jobs, clock, ids }
    }

    /// The org's repo, or `NotFound` (also for another org's id).
    async fn repo(&self, org: OrgId, id: RepoId) -> Result<Repo, AppError> {
        self.repos.repo(org, id).await.map_err(repo_err)
    }

    /// The org's repo with `access: granted`; a disconnected repo is `Invalid` (nothing new is
    /// indexed for it).
    async fn connected_repo(&self, org: OrgId, id: RepoId) -> Result<Repo, AppError> {
        let repo = self.repo(org, id).await?;
        if repo.access == RepoAccess::Lost {
            return Err(AppError::Invalid(format!("repository {}/{} is disconnected", repo.owner, repo.name)));
        }
        Ok(repo)
    }

    /// Installation ids of the org's active GitHub (read app) integrations.
    async fn installations(&self, org: OrgId) -> Result<Vec<InstallationId>, AppError> {
        self.orgs
            .integrations(org)
            .await?
            .into_iter()
            .filter(|i| i.kind == IntegrationKind::GitHub && i.status == IntegrationStatus::Active)
            .map(|i| {
                i.external_id
                    .parse::<u64>()
                    .map(InstallationId)
                    .map_err(|_| AppError::Internal(format!("GitHub integration with a non-numeric installation id {:?}", i.external_id)))
            })
            .collect()
    }

    /// Record `reason` on `sha`, then enqueue `index.version` (dedupe `index.version:{repo}:{sha}`).
    /// The reason is written first so a duplicate job (already queued for another reason) still
    /// leaves this reason on the version.
    async fn index(&self, org: OrgId, repo: RepoId, sha: CommitSha, reason: VersionReason) -> Result<JobQueued, AppError> {
        self.repos.add_version_reason(org, repo, sha, reason.clone()).await.map_err(repo_err)?;
        let job = NewJob::new(JobPayload::IndexVersion { org_id: org, repo_id: repo, sha, reason });
        let enqueued = self.jobs.enqueue(job).await.map_err(job_err)?;
        Ok(JobQueued { job_id: enqueued.job_id() })
    }
}

fn coords(repo: &Repo) -> RepoCoords {
    RepoCoords { owner: repo.owner.clone(), name: repo.name.clone() }
}

/// Every item of a paged store listing.
async fn collect_pages<T, E, F, Fut>(mut fetch: F) -> Result<Vec<T>, E>
where
    F: FnMut(PageRequest) -> Fut,
    Fut: Future<Output = Result<Page<T>, E>>,
{
    let mut out = Vec::new();
    let mut req = PageRequest::first(LIST_PAGE);
    loop {
        let page = fetch(req.clone()).await?;
        out.extend(page.items);
        match page.next {
            Some(cursor) => req = req.after(cursor),
            None => return Ok(out),
        }
    }
}

fn repo_err(err: RepoStoreError) -> AppError {
    match err {
        RepoStoreError::NotFound => AppError::NotFound("repo".into()),
        RepoStoreError::Conflict => AppError::Conflict("repo store".into()),
        RepoStoreError::Invalid(msg) => AppError::Invalid(msg),
        RepoStoreError::Unavailable(msg) => AppError::Unavailable(msg),
    }
}

fn job_err(err: JobError) -> AppError {
    match err {
        JobError::NotFound => AppError::NotFound("job".into()),
        JobError::Conflict | JobError::LeaseLost => AppError::Conflict("job queue".into()),
        JobError::Invalid(msg) => AppError::Internal(format!("index job rejected: {msg}")),
        JobError::Unavailable(msg) => AppError::Unavailable(msg),
    }
}
