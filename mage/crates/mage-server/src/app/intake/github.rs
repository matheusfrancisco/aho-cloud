//! Use cases 4 IngestGithubPush and 5 IngestDeployment (inside `intake.github`, intake.md), plus
//! the shared steps resync and installation events reuse: default-head sync and deployment
//! recording with the backfill rule.

use std::sync::Arc;
use std::time::Duration;

use mage_core::domain::{CommitSha, Deployment, DeploymentSource, InstallationId, JobPayload, OrgId, Repo, RepoAccess, TicketSource, VersionReason};
use mage_core::ports::source_host::RepoCoords;
use mage_core::ports::{Clock, JobQueue, NewJob, RepoStore, SourceHost};
use time::OffsetDateTime;

use crate::app::intake::error::IntakeError;
use crate::domain::intake::IntakeConfig;
use crate::ports::hooks::DeploymentState;
use crate::ports::org::OrgStore;
use crate::ports::ticket::TicketStore;

/// A deployment observed more than this long ago arrived late: assumed tickets on the repo are
/// re-ingested so they can upgrade (3.6 "Backfill").
const BACKFILL_AGE: Duration = Duration::from_secs(10 * 60);

/// Outcome of an inline GitHub step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Handled {
    Processed,
    Ignored(String),
}

/// A `deployment_status` event as IngestDeployment needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeploymentStatusEvent {
    pub repo_external_id: u64,
    pub environment: String,
    pub sha: CommitSha,
    pub state: DeploymentState,
    pub created_at: OffsetDateTime,
}

pub struct GithubIngest {
    orgs: Arc<dyn OrgStore>,
    repos: Arc<dyn RepoStore>,
    tickets: Arc<dyn TicketStore>,
    source: Arc<dyn SourceHost>,
    jobs: Arc<dyn JobQueue>,
    clock: Arc<dyn Clock>,
}

pub fn coords(repo: &Repo) -> RepoCoords {
    RepoCoords { owner: repo.owner.clone(), name: repo.name.clone() }
}

impl GithubIngest {
    pub fn new(
        orgs: Arc<dyn OrgStore>,
        repos: Arc<dyn RepoStore>,
        tickets: Arc<dyn TicketStore>,
        source: Arc<dyn SourceHost>,
        jobs: Arc<dyn JobQueue>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self { orgs, repos, tickets, source, jobs, clock }
    }

    /// Use case 4.
    pub async fn ingest_push(&self, org: OrgId, inst: InstallationId, repo_external_id: u64, git_ref: &str, deleted: bool) -> Result<Handled, IntakeError> {
        let Some(repo) = self.repos.get_by_external_id(org, repo_external_id).await? else {
            return Ok(Handled::Ignored("unknown repo".into()));
        };
        if repo.access == RepoAccess::Lost {
            return Ok(Handled::Ignored("repo access lost".into()));
        }
        if repo.installation_id != inst {
            return Ok(Handled::Ignored("repo belongs to another installation".into()));
        }
        if deleted {
            // A deleted tag keeps its RepoVersion (tickets may cite it).
            return Ok(Handled::Ignored("ref deleted".into()));
        }
        let cfg = self.orgs.intake_config(org).await?;
        if git_ref.strip_prefix("refs/heads/") == Some(repo.default_branch.as_str()) {
            self.sync_default_head(org, &repo, &cfg).await?;
            return Ok(Handled::Processed);
        }
        if let Some(tag) = git_ref.strip_prefix("refs/tags/") {
            if !cfg.github.is_release_tag(tag) {
                return Ok(Handled::Ignored("tag does not match release_tag_glob".into()));
            }
            // Current tag target (peels annotated tags), not the payload's `after`.
            let sha = self.source.resolve_ref(repo.installation_id, &coords(&repo), &format!("tags/{tag}")).await?;
            let reason = VersionReason::Release(tag.to_owned());
            self.repos.add_version_reason(org, repo.id, sha, reason.clone()).await?;
            self.jobs.enqueue(NewJob::new(JobPayload::IndexVersion { org_id: org, repo_id: repo.id, sha, reason })).await?;
            return Ok(Handled::Processed);
        }
        Ok(Handled::Ignored("ref is neither the default branch nor a tag".into()))
    }

    /// Use case 4 step 2: current head of the default branch (not a payload sha, so coalesced or
    /// out-of-order pushes converge); when it moved, `index.version { DefaultHead }` after
    /// `debounce.push_index`, enqueued before the head moves so a crash in between cannot lose it.
    pub async fn sync_default_head(&self, org: OrgId, repo: &Repo, cfg: &IntakeConfig) -> Result<bool, IntakeError> {
        let sha = self.source.resolve_ref(repo.installation_id, &coords(repo), &format!("heads/{}", repo.default_branch)).await?;
        if self.repos.default_head(org, repo.id).await? == Some(sha) {
            return Ok(false);
        }
        let run_after = self.clock.now() + cfg.debounce.push_index();
        let job = NewJob::new(JobPayload::IndexVersion { org_id: org, repo_id: repo.id, sha, reason: VersionReason::DefaultHead }).after(run_after);
        self.jobs.enqueue(job).await?;
        self.repos.set_default_head(org, repo.id, sha).await?;
        Ok(true)
    }

    /// Use case 5.
    pub async fn ingest_deployment(&self, org: OrgId, ev: &DeploymentStatusEvent) -> Result<Handled, IntakeError> {
        if ev.state != DeploymentState::Success {
            return Ok(Handled::Ignored(format!("deployment state {:?}", ev.state)));
        }
        let cfg = self.orgs.intake_config(org).await?;
        let Some(environment) = cfg.environments.allowed(&ev.environment) else {
            return Ok(Handled::Ignored("environment not allowed".into()));
        };
        let Some(repo) = self.repos.get_by_external_id(org, ev.repo_external_id).await? else {
            return Ok(Handled::Ignored("unknown repo".into()));
        };
        if repo.access == RepoAccess::Lost {
            return Ok(Handled::Ignored("repo access lost".into()));
        }
        let deployment = Deployment {
            org_id: org,
            repo_id: repo.id,
            environment,
            sha: ev.sha,
            observed_at: ev.created_at,
            source: DeploymentSource::GitHubDeployments,
        };
        self.record_deployment(org, &deployment).await?;
        Ok(Handled::Processed)
    }

    /// Append to the history (idempotent); when this is the environment's newest deployment,
    /// move `Deployed(env)` to it and index it without debounce; apply the backfill rule to a
    /// late arrival. Returns whether the observation was new.
    pub async fn record_deployment(&self, org: OrgId, d: &Deployment) -> Result<bool, IntakeError> {
        let new = self.repos.record_deployment(org, d).await?;
        let now = self.clock.now();
        let newest = self.repos.latest_deployment(org, d.repo_id, &d.environment, now).await?;
        if newest.as_ref().is_some_and(|n| n.sha == d.sha && n.observed_at == d.observed_at) {
            let reason = VersionReason::Deployed(d.environment.clone());
            self.repos.add_version_reason(org, d.repo_id, d.sha, reason.clone()).await?;
            self.jobs.enqueue(NewJob::new(JobPayload::IndexVersion { org_id: org, repo_id: d.repo_id, sha: d.sha, reason })).await?;
        }
        if new && d.observed_at < now - BACKFILL_AGE {
            self.backfill(org, d.repo_id).await?;
        }
        Ok(new)
    }

    /// Re-ingest open tickets of `repo` whose target is assumed (`list_open_assumed`).
    async fn backfill(&self, org: OrgId, repo: mage_core::domain::RepoId) -> Result<(), IntakeError> {
        for ticket_id in self.tickets.list_open_assumed(org, repo).await? {
            let ticket = self.tickets.get_ticket(org, ticket_id).await?.value;
            let TicketSource::Linear { issue_id, .. } = &ticket.source;
            self.jobs.enqueue(NewJob::new(JobPayload::IntakeLinearIssue { org_id: org, issue_id: issue_id.clone() })).await?;
        }
        Ok(())
    }
}
