//! Use case 2, ProcessDelivery (jobs `intake.github`, `intake.github_fix`, `intake.linear`,
//! `intake.slack`, intake.md): decode a stored delivery, resolve its tenant from the verified
//! envelope, dispatch, and record the outcome on the inbox row.

use std::collections::HashMap;
use std::sync::Arc;

use mage_core::domain::{Integration, IntegrationKind, IntegrationStatus, JobPayload, OrgId};
use mage_core::ports::{Clock, JobQueue, NewJob, RepoStore};

use crate::app::intake::error::IntakeError;
use crate::app::intake::github::{DeploymentStatusEvent, GithubIngest, Handled};
use crate::app::intake::lifecycle::IntegrationEvents;
use crate::app::intake::linear_issue::TrackerIdentityCache;
use crate::ports::hooks::{HookEvent, WebhookDecoder, WebhookSource};
use crate::ports::inbox::{InboxError, InboxOutcome, WebhookInbox};
use crate::ports::org::OrgStore;
use crate::ports::tracker::TrackerError;

pub struct ProcessDelivery {
    inbox: Arc<dyn WebhookInbox>,
    decoders: HashMap<WebhookSource, Arc<dyn WebhookDecoder>>,
    orgs: Arc<dyn OrgStore>,
    repos: Arc<dyn RepoStore>,
    jobs: Arc<dyn JobQueue>,
    identities: Arc<TrackerIdentityCache>,
    github: Arc<GithubIngest>,
    lifecycle: Arc<IntegrationEvents>,
    clock: Arc<dyn Clock>,
}

impl ProcessDelivery {
    /// `decoders` holds one decoder per source (`WebhookDecoder::source`).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        inbox: Arc<dyn WebhookInbox>,
        decoders: Vec<Arc<dyn WebhookDecoder>>,
        orgs: Arc<dyn OrgStore>,
        repos: Arc<dyn RepoStore>,
        jobs: Arc<dyn JobQueue>,
        identities: Arc<TrackerIdentityCache>,
        github: Arc<GithubIngest>,
        lifecycle: Arc<IntegrationEvents>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let decoders = decoders.into_iter().map(|d| (d.source(), d)).collect();
        Self { inbox, decoders, orgs, repos, jobs, identities, github, lifecycle, clock }
    }

    pub async fn run(&self, source: WebhookSource, delivery_id: &str) -> Result<InboxOutcome, IntakeError> {
        // 1. Load; a purged body or an already recorded outcome ends here.
        let delivery = match self.inbox.get(source, delivery_id).await {
            Ok(d) => d,
            Err(InboxError::NotFound) => return Err(IntakeError::permanent("inbox row missing")),
            Err(e) => return Err(e.into()),
        };
        if let Some(outcome) = delivery.outcome {
            return Ok(outcome);
        }
        let Some(body) = delivery.body else {
            return self.finish(source, delivery_id, InboxOutcome::Ignored("purged".into())).await;
        };

        // 2. Decode; a verified body that does not parse is kept as a fixture, never retried.
        let decoder = self.decoders.get(&source).ok_or_else(|| IntakeError::permanent(format!("no decoder for {}", source.as_str())))?;
        let event = match decoder.decode(&delivery.event, &body) {
            Ok(event) => event,
            Err(e) => return self.finish(source, delivery_id, InboxOutcome::Rejected(e.0)).await,
        };
        if let HookEvent::Ignored { reason } = &event {
            return self.finish(source, delivery_id, InboxOutcome::Ignored(reason.clone())).await;
        }

        // 3. Tenant from the verified envelope.
        let (kind, external) = tenant_key(source, &event);
        let integration = self.orgs.integration_by_external(kind, &external).await?;

        // GitHub installation events run whatever the binding (use case 6).
        if let HookEvent::GithubInstallation { installation, action, .. } = &event {
            if let Some(i) = &integration {
                self.inbox.resolve_org(source, delivery_id, i.org_id).await?;
            }
            let handled = self.lifecycle.installation(source, *installation, action, integration.as_ref()).await?;
            return self.finish(source, delivery_id, handled.into()).await;
        }
        let Some(integration) = integration else {
            return self.finish(source, delivery_id, InboxOutcome::Ignored("unbound".into())).await;
        };
        if integration.status == IntegrationStatus::Revoked {
            return self.finish(source, delivery_id, InboxOutcome::Ignored("revoked".into())).await;
        }
        let org = integration.org_id;

        // 4. Resolve, dispatch.
        self.inbox.resolve_org(source, delivery_id, org).await?;
        let handled = self.dispatch(org, source, delivery_id, &integration, event).await?;
        self.finish(source, delivery_id, handled.into()).await
    }

    async fn dispatch(&self, org: OrgId, source: WebhookSource, delivery_id: &str, integration: &Integration, event: HookEvent) -> Result<Handled, IntakeError> {
        match event {
            HookEvent::LinearIssue { issue_id, actor_id, .. }
            | HookEvent::LinearComment { issue_id, actor_id, .. }
            | HookEvent::LinearAttachment { issue_id, actor_id, .. } => self.linear_change(org, &issue_id, actor_id.as_deref()).await,
            HookEvent::GithubPush { installation, repo_external_id, git_ref, deleted, .. } => {
                if source != WebhookSource::GitHub {
                    return Ok(Handled::Ignored("push on the fix app".into()));
                }
                self.github.ingest_push(org, installation, repo_external_id, &git_ref, deleted).await
            }
            HookEvent::GithubDeploymentStatus { repo_external_id, environment, sha, state, created_at, .. } => {
                if source != WebhookSource::GitHub {
                    return Ok(Handled::Ignored("deployment on the fix app".into()));
                }
                let ev = DeploymentStatusEvent { repo_external_id, environment, sha, state, created_at };
                self.github.ingest_deployment(org, &ev).await
            }
            HookEvent::GithubPullRequest { repo_external_id, number, action, merged, head_ref, .. } => {
                if !head_ref.starts_with("mage/") {
                    return Ok(Handled::Ignored("not a mage/ branch".into()));
                }
                let Some(repo) = self.repos.get_by_external_id(org, repo_external_id).await? else {
                    return Ok(Handled::Ignored("unknown repo".into()));
                };
                let job = NewJob::new(JobPayload::FixPrEvent { org_id: org, repo_id: repo.id, number: u64::from(number), action, merged })
                    .dedupe(format!("fix.pr_event:{}:{number}:{delivery_id}", repo.id));
                self.jobs.enqueue(job).await?;
                Ok(Handled::Processed)
            }
            HookEvent::GithubInstallationRepos { installation, added, removed } => {
                if source != WebhookSource::GitHub {
                    return Ok(Handled::Ignored("fix app repositories come from the read app".into()));
                }
                self.lifecycle.installation_repos(org, installation, &added, &removed).await
            }
            HookEvent::GithubRepository { repo_external_id, action, repo, default_branch, .. } => {
                if source != WebhookSource::GitHub {
                    return Ok(Handled::Ignored("repository event on the fix app".into()));
                }
                self.lifecycle.repository(org, repo_external_id, &action, &repo, &default_branch).await
            }
            HookEvent::LinearAppRevoked { .. } | HookEvent::SlackAppUninstalled { .. } | HookEvent::SlackTokensRevoked { .. } => {
                self.lifecycle.provider_revoked(integration).await
            }
            // Optional extension (off in v1): refresh the ticket owning the thread.
            HookEvent::SlackThreadMessage { .. } => Ok(Handled::Ignored("thread message events are not enabled".into())),
            HookEvent::GithubInstallation { .. } | HookEvent::Ignored { .. } => Ok(Handled::Ignored("not dispatched".into())),
        }
    }

    /// Linear issue / comment / attachment: skip Mage's own actions, else a debounced,
    /// coalesced `intake.linear_issue`.
    async fn linear_change(&self, org: OrgId, issue_id: &str, actor_id: Option<&str>) -> Result<Handled, IntakeError> {
        if let Some(actor) = actor_id {
            match self.identities.get(org).await {
                Ok(me) if TrackerIdentityCache::is_ours(&me, actor) => return Ok(Handled::Ignored("own action".into())),
                Ok(_) => {}
                // A rejected credential is detected and handled by the ingest job.
                Err(TrackerError::Unauthorized) => {}
                Err(e) => return Err(e.into()),
            }
        }
        let cfg = self.orgs.intake_config(org).await?;
        let run_after = self.clock.now() + cfg.debounce.issue();
        self.jobs.enqueue(NewJob::new(JobPayload::IntakeLinearIssue { org_id: org, issue_id: issue_id.to_owned() }).after(run_after)).await?;
        Ok(Handled::Processed)
    }

    async fn finish(&self, source: WebhookSource, delivery_id: &str, outcome: InboxOutcome) -> Result<InboxOutcome, IntakeError> {
        self.inbox.mark_processed(source, delivery_id, outcome.clone()).await?;
        Ok(outcome)
    }
}

impl From<Handled> for InboxOutcome {
    fn from(h: Handled) -> Self {
        match h {
            Handled::Processed => InboxOutcome::Processed,
            Handled::Ignored(reason) => InboxOutcome::Ignored(reason),
        }
    }
}

/// Integration kind and external id naming the tenant of an event.
fn tenant_key(source: WebhookSource, event: &HookEvent) -> (IntegrationKind, String) {
    let github_kind = if source == WebhookSource::GitHubFix { IntegrationKind::GitHubFix } else { IntegrationKind::GitHub };
    match event {
        HookEvent::GithubPush { installation, .. }
        | HookEvent::GithubInstallation { installation, .. }
        | HookEvent::GithubInstallationRepos { installation, .. }
        | HookEvent::GithubRepository { installation, .. }
        | HookEvent::GithubDeploymentStatus { installation, .. }
        | HookEvent::GithubPullRequest { installation, .. } => (github_kind, installation.to_string()),
        HookEvent::LinearIssue { workspace_id, .. }
        | HookEvent::LinearComment { workspace_id, .. }
        | HookEvent::LinearAttachment { workspace_id, .. }
        | HookEvent::LinearAppRevoked { workspace_id } => (IntegrationKind::Linear, workspace_id.clone()),
        HookEvent::SlackAppUninstalled { team_id } | HookEvent::SlackTokensRevoked { team_id } | HookEvent::SlackThreadMessage { team_id, .. } => {
            (IntegrationKind::Slack, team_id.clone())
        }
        HookEvent::Ignored { .. } => (github_kind, String::new()),
    }
}

#[cfg(test)]
mod tests;
