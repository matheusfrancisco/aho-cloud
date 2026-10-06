//! Use case 6, integration lifecycle (intake.md): [`BindIntegration`] (provider exchange, secrets,
//! `Integration`, first sync; called by server.md's connect callbacks) and [`IntegrationEvents`]
//! (installation / repository / revocation webhooks), plus internal-v1 seeding.

use std::sync::Arc;

use mage_core::domain::{
    InstallationId, Integration, IntegrationKind, IntegrationStatus, JobPayload, OrgId, Repo, RepoAccess, WebhookSource,
};
use mage_core::ports::source_host::{RepoCoords, SourceHostError};
use mage_core::ports::{Clock, IdGen, JobQueue, NewJob, RepoStore, SourceHost};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use url::Url;

use crate::app::intake::config::{GithubInstallSeed, IntakeSection};
use crate::app::intake::error::IntakeError;
use crate::app::intake::github::{GithubIngest, Handled};
use crate::domain::intake::{IntegrationSettings, LinearAuthMode, secret_names};
use crate::error::AppError;
use crate::ports::audit::Actor;
use crate::ports::chat::ChatNotifier;
use crate::ports::hooks::{InstallationAction, RepoAction};
use crate::ports::org::{OrgStore, OrgStoreError};
use crate::ports::source_host_admin::{GithubApp, SourceHostAdmin};
use crate::ports::tracker::TicketTracker;
use crate::ports::vault::{Activation, SecretBytes, SecretName, SecretScope, SecretState, SecretVault, VaultError};

/// Linear OAuth grant as stored at `integration/linear/{workspace}/oauth` (read by
/// `adapters::driven::linear`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredLinearOAuth {
    pub access_token: String,
    pub refresh_token: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub expires_at: Option<OffsetDateTime>,
}

/// Provider URLs and client ids for `connect_url` (`[server.integrations]`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConnectConfig {
    /// `https://github.com/` (GitHub Enterprise: its web base).
    pub github_web_base: Url,
    /// Read app slug (`https://github.com/apps/{slug}`).
    pub github_app_slug: String,
    /// Mage Fix App slug; `None` until F3 is set up.
    pub github_fix_app_slug: Option<String>,
    pub linear_authorize_url: Url,
    pub linear_client_id: Option<String>,
    pub linear_scopes: Vec<String>,
    pub slack_authorize_url: Url,
    pub slack_client_id: Option<String>,
    pub slack_scopes: Vec<String>,
}

impl Default for ConnectConfig {
    fn default() -> Self {
        Self {
            github_web_base: Url::parse("https://github.com/").expect("static url"),
            github_app_slug: "ahomage".into(),
            github_fix_app_slug: None,
            linear_authorize_url: Url::parse("https://linear.app/oauth/authorize").expect("static url"),
            linear_client_id: None,
            linear_scopes: vec!["read".into(), "write".into()],
            slack_authorize_url: Url::parse("https://slack.com/oauth/v2/authorize").expect("static url"),
            slack_client_id: None,
            slack_scopes: ["channels:history", "groups:history", "chat:write", "team:read"].map(String::from).to_vec(),
        }
    }
}

fn integration_conflict(err: OrgStoreError, kind: IntegrationKind) -> AppError {
    match err {
        OrgStoreError::Conflict => AppError::Conflict(format!("this {kind:?} account is connected to another org")),
        other => other.into(),
    }
}

fn secret_name(name: String) -> Result<SecretName, AppError> {
    SecretName::new(name).map_err(AppError::from)
}

/// Destroy every live version of a secret (revocation: tokens are crypto-shredded).
async fn destroy_secret(vault: &dyn SecretVault, scope: &SecretScope, name: &SecretName, actor: &Actor) -> Result<(), VaultError> {
    let versions = match vault.versions(scope, name).await {
        Ok(v) => v,
        Err(VaultError::NotFound) => return Ok(()),
        Err(e) => return Err(e),
    };
    for meta in versions.into_iter().filter(|m| m.state != SecretState::Destroyed) {
        vault.set_state(scope, name, meta.version, SecretState::Destroyed, actor).await?;
    }
    Ok(())
}

pub struct BindIntegration {
    orgs: Arc<dyn OrgStore>,
    admin: Arc<dyn SourceHostAdmin>,
    tracker: Arc<dyn TicketTracker>,
    chat: Arc<dyn ChatNotifier>,
    vault: Arc<dyn SecretVault>,
    clock: Arc<dyn Clock>,
    config: ConnectConfig,
}

impl BindIntegration {
    pub fn new(
        orgs: Arc<dyn OrgStore>,
        admin: Arc<dyn SourceHostAdmin>,
        tracker: Arc<dyn TicketTracker>,
        chat: Arc<dyn ChatNotifier>,
        vault: Arc<dyn SecretVault>,
        clock: Arc<dyn Clock>,
        config: ConnectConfig,
    ) -> Self {
        Self { orgs, admin, tracker, chat, vault, clock, config }
    }

    /// Provider URL the connect route redirects to (server.md UC3 step 1).
    pub fn connect_url(&self, kind: IntegrationKind, state: &str, pkce_challenge: &str, redirect_uri: &Url) -> Result<String, AppError> {
        let not_configured = |what: &str| AppError::Unavailable(format!("{what} is not configured"));
        match kind {
            IntegrationKind::GitHub | IntegrationKind::GitHubFix => {
                let slug = match kind {
                    IntegrationKind::GitHub => self.config.github_app_slug.as_str(),
                    _ => self.config.github_fix_app_slug.as_deref().ok_or_else(|| not_configured("the Mage Fix GitHub App"))?,
                };
                let mut url = self.config.github_web_base.join(&format!("apps/{slug}/installations/new")).map_err(|e| AppError::Internal(e.to_string()))?;
                url.query_pairs_mut().append_pair("state", state);
                Ok(url.into())
            }
            IntegrationKind::Linear => {
                let client_id = self.config.linear_client_id.as_deref().ok_or_else(|| not_configured("Linear OAuth"))?;
                let mut url = self.config.linear_authorize_url.clone();
                url.query_pairs_mut()
                    .append_pair("client_id", client_id)
                    .append_pair("redirect_uri", redirect_uri.as_str())
                    .append_pair("response_type", "code")
                    .append_pair("scope", &self.config.linear_scopes.join(","))
                    .append_pair("state", state)
                    .append_pair("actor", "app")
                    .append_pair("code_challenge", pkce_challenge)
                    .append_pair("code_challenge_method", "S256");
                Ok(url.into())
            }
            IntegrationKind::Slack => {
                let client_id = self.config.slack_client_id.as_deref().ok_or_else(|| not_configured("Slack OAuth"))?;
                let mut url = self.config.slack_authorize_url.clone();
                url.query_pairs_mut()
                    .append_pair("client_id", client_id)
                    .append_pair("scope", &self.config.slack_scopes.join(","))
                    .append_pair("redirect_uri", redirect_uri.as_str())
                    .append_pair("state", state);
                Ok(url.into())
            }
        }
    }

    /// GitHub app setup callback: the installation id is user-controlled, so it binds only if
    /// the authenticated user can see it (`complete_install` → `Invalid` otherwise).
    pub async fn github(&self, org: OrgId, app: GithubApp, installation_id: InstallationId, code: &str, _actor: &Actor) -> Result<Integration, AppError> {
        let info = self.admin.complete_install(app, installation_id, code).await.map_err(|e| match e {
            SourceHostError::Invalid(msg) => AppError::Invalid(msg),
            SourceHostError::NotFound => AppError::Invalid("installation not found".into()),
            other => other.into(),
        })?;
        let kind = match app {
            GithubApp::Read => IntegrationKind::GitHub,
            GithubApp::Fix => IntegrationKind::GitHubFix,
        };
        let integration = Integration { org_id: org, kind, external_id: info.id.to_string(), status: IntegrationStatus::Active };
        // Repos come from the read app; the fix app needs no sync.
        let jobs = match app {
            GithubApp::Read => vec![NewJob::new(JobPayload::IntakeGithubResync { org_id: org })],
            GithubApp::Fix => vec![],
        };
        self.orgs.upsert_integration(&integration, &jobs).await.map_err(|e| integration_conflict(e, kind))?;
        let settings = IntegrationSettings { github_account_login: Some(info.account_login), ..IntegrationSettings::default() };
        self.orgs.set_integration_settings(org, kind, &integration.external_id, &settings).await?;
        Ok(integration)
    }

    /// Linear OAuth callback.
    pub async fn linear(&self, org: OrgId, code: &str, redirect_uri: &Url, pkce_verifier: &str, actor: &Actor) -> Result<Integration, AppError> {
        let grant = self.tracker.complete_install(code, redirect_uri, pkce_verifier).await?;
        let stored = StoredLinearOAuth {
            access_token: grant.access_token.expose().to_owned(),
            refresh_token: grant.refresh_token.as_ref().map(|t| t.expose().to_owned()),
            expires_at: grant.expires_at,
        };
        let json = serde_json::to_string(&stored).map_err(|e| AppError::Internal(e.to_string()))?;
        let name = secret_name(secret_names::linear_oauth(&grant.workspace_id))?;
        self.vault.put(&SecretScope::Org(org), &name, SecretBytes::from(json), Activation::Now, actor).await?;
        let integration = Integration { org_id: org, kind: IntegrationKind::Linear, external_id: grant.workspace_id.clone(), status: IntegrationStatus::Active };
        let since = self.clock.now() - time::Duration::hours(24);
        let jobs = [NewJob::new(JobPayload::IntakeLinearResync { org_id: org, since })];
        self.orgs.upsert_integration(&integration, &jobs).await.map_err(|e| integration_conflict(e, IntegrationKind::Linear))?;
        let settings = IntegrationSettings {
            linear_auth: Some(LinearAuthMode::OAuth),
            linear_workspace_name: Some(grant.workspace_name),
            ..IntegrationSettings::default()
        };
        self.orgs.set_integration_settings(org, IntegrationKind::Linear, &grant.workspace_id, &settings).await?;
        Ok(integration)
    }

    /// Slack OAuth callback.
    pub async fn slack(&self, org: OrgId, code: &str, redirect_uri: &Url, actor: &Actor) -> Result<Integration, AppError> {
        let grant = self.chat.complete_install(code, redirect_uri).await?;
        let name = secret_name(secret_names::slack_bot_token(&grant.team_id))?;
        self.vault.put(&SecretScope::Org(org), &name, SecretBytes::from(grant.bot_token.expose().to_owned()), Activation::Now, actor).await?;
        let integration = Integration { org_id: org, kind: IntegrationKind::Slack, external_id: grant.team_id.clone(), status: IntegrationStatus::Active };
        self.orgs.upsert_integration(&integration, &[]).await.map_err(|e| integration_conflict(e, IntegrationKind::Slack))?;
        let settings = IntegrationSettings {
            slack_team_domain: Some(grant.team_domain),
            slack_bot_user_id: Some(grant.bot_user_id),
            ..IntegrationSettings::default()
        };
        self.orgs.set_integration_settings(org, IntegrationKind::Slack, &grant.team_id, &settings).await?;
        Ok(integration)
    }

    /// Admin disconnect: `Revoked`, stored tokens destroyed, GitHub tokens evicted.
    pub async fn revoke(&self, org: OrgId, kind: IntegrationKind, external_id: &str, actor: &Actor) -> Result<(), AppError> {
        self.orgs.set_integration_status(org, kind, external_id, IntegrationStatus::Revoked, &[]).await?;
        let scope = SecretScope::Org(org);
        match kind {
            IntegrationKind::Linear => {
                for name in [secret_names::linear_api_key(external_id), secret_names::linear_oauth(external_id)] {
                    destroy_secret(self.vault.as_ref(), &scope, &secret_name(name)?, actor).await?;
                }
            }
            IntegrationKind::Slack => destroy_secret(self.vault.as_ref(), &scope, &secret_name(secret_names::slack_bot_token(external_id))?, actor).await?,
            IntegrationKind::GitHub | IntegrationKind::GitHubFix => {
                if let Ok(id) = external_id.parse::<u64>() {
                    let app = if kind == IntegrationKind::GitHub { GithubApp::Read } else { GithubApp::Fix };
                    self.admin.evict_installation(app, InstallationId(id));
                }
            }
        }
        Ok(())
    }
}

/// Webhook-driven lifecycle (use case 6 tables).
pub struct IntegrationEvents {
    orgs: Arc<dyn OrgStore>,
    repos: Arc<dyn RepoStore>,
    source: Arc<dyn SourceHost>,
    admin: Arc<dyn SourceHostAdmin>,
    vault: Arc<dyn SecretVault>,
    github: Arc<GithubIngest>,
    jobs: Arc<dyn JobQueue>,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGen>,
}

impl IntegrationEvents {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        orgs: Arc<dyn OrgStore>,
        repos: Arc<dyn RepoStore>,
        source: Arc<dyn SourceHost>,
        admin: Arc<dyn SourceHostAdmin>,
        vault: Arc<dyn SecretVault>,
        github: Arc<GithubIngest>,
        jobs: Arc<dyn JobQueue>,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGen>,
    ) -> Self {
        Self { orgs, repos, source, admin, vault, github, jobs, clock, ids }
    }

    /// `installation` on `/hooks/github` (`GitHub`) or `/hooks/github-fix` (`GitHubFix`).
    /// `bound` = the integration row for this installation, if any (any status).
    pub async fn installation(&self, source: WebhookSource, inst: InstallationId, action: &InstallationAction, bound: Option<&Integration>) -> Result<Handled, IntakeError> {
        let app = if source == WebhookSource::GitHubFix { GithubApp::Fix } else { GithubApp::Read };
        let Some(integration) = bound else {
            if matches!(action, InstallationAction::Deleted | InstallationAction::Suspend | InstallationAction::NewPermissionsAccepted) {
                self.admin.evict_installation(app, inst);
            }
            return Ok(Handled::Ignored("unbound".into()));
        };
        let org = integration.org_id;
        let set_status = |status: IntegrationStatus, jobs: Vec<NewJob>| {
            let kind = integration.kind;
            let external = integration.external_id.clone();
            async move { self.orgs.set_integration_status(org, kind, &external, status, &jobs).await }
        };
        match action {
            InstallationAction::Created => {
                if integration.status != IntegrationStatus::Active {
                    return Ok(Handled::Ignored("revoked".into()));
                }
                if app == GithubApp::Read {
                    self.jobs.enqueue(NewJob::new(JobPayload::IntakeGithubResync { org_id: org })).await?;
                }
                Ok(Handled::Processed)
            }
            InstallationAction::Deleted | InstallationAction::Suspend => {
                self.admin.evict_installation(app, inst);
                set_status(IntegrationStatus::Revoked, vec![]).await?;
                if app == GithubApp::Read {
                    for repo in self.repos.list_repos(org).await? {
                        if repo.installation_id == inst && repo.access != RepoAccess::Lost {
                            self.repos.set_access(org, repo.id, RepoAccess::Lost).await?;
                        }
                    }
                }
                Ok(Handled::Processed)
            }
            InstallationAction::Unsuspend => {
                let jobs = if app == GithubApp::Read { vec![NewJob::new(JobPayload::IntakeGithubResync { org_id: org })] } else { vec![] };
                set_status(IntegrationStatus::Active, jobs).await?;
                Ok(Handled::Processed)
            }
            InstallationAction::NewPermissionsAccepted => {
                self.admin.evict_installation(app, inst);
                if app == GithubApp::Read {
                    let info = self.source.installation(inst).await?;
                    let mut settings = self.orgs.integration_settings(org, integration.kind, &integration.external_id).await?;
                    settings.github_account_login = Some(info.account_login);
                    self.orgs.set_integration_settings(org, integration.kind, &integration.external_id, &settings).await?;
                }
                Ok(Handled::Processed)
            }
            InstallationAction::Other(action) => Ok(Handled::Ignored(format!("installation action {action}"))),
        }
    }

    /// `installation_repositories` (read app): added → upsert `Granted` + default-head flow;
    /// removed → `Lost` (rows stay).
    pub async fn installation_repos(&self, org: OrgId, inst: InstallationId, added: &[(u64, RepoCoords)], removed: &[u64]) -> Result<Handled, IntakeError> {
        let cfg = self.orgs.intake_config(org).await?;
        for (_, coords) in added {
            let remote = match self.source.repo(inst, coords).await {
                Ok(r) => r,
                Err(SourceHostError::NotFound) => continue,
                Err(e) => return Err(e.into()),
            };
            let repo = self.upsert_remote(org, inst, &remote).await?;
            if !remote.archived {
                self.github.sync_default_head(org, &repo, &cfg).await?;
            }
        }
        for external in removed {
            if let Some(repo) = self.repos.get_by_external_id(org, *external).await? {
                self.repos.set_access(org, repo.id, RepoAccess::Lost).await?;
            }
        }
        Ok(Handled::Processed)
    }

    /// Insert or update by external id (stored id and `created_at` win), `Granted`.
    pub async fn upsert_remote(&self, org: OrgId, inst: InstallationId, remote: &mage_core::ports::source_host::RemoteRepo) -> Result<Repo, IntakeError> {
        let existing = self.repos.get_by_external_id(org, remote.external_id).await?;
        let repo = Repo {
            id: existing.as_ref().map_or_else(|| self.ids.new_id(), |r| r.id),
            org_id: org,
            external_id: remote.external_id,
            owner: remote.coords.owner.clone(),
            name: remote.coords.name.clone(),
            default_branch: remote.default_branch.clone(),
            installation_id: inst,
            access: RepoAccess::Granted,
            created_at: existing.as_ref().map_or_else(|| self.clock.now(), |r| r.created_at),
        };
        Ok(self.repos.upsert_repo(&repo, &[]).await?)
    }

    /// `repository` (read app).
    pub async fn repository(&self, org: OrgId, repo_external_id: u64, action: &RepoAction, coords_now: &RepoCoords, default_branch: &str) -> Result<Handled, IntakeError> {
        let Some(repo) = self.repos.get_by_external_id(org, repo_external_id).await? else {
            return Ok(Handled::Ignored("unknown repo".into()));
        };
        match action {
            RepoAction::Renamed | RepoAction::Transferred | RepoAction::Edited => {
                let branch_changed = repo.default_branch != default_branch;
                let updated = Repo { owner: coords_now.owner.clone(), name: coords_now.name.clone(), default_branch: default_branch.to_owned(), ..repo };
                let stored = self.repos.upsert_repo(&updated, &[]).await?;
                if branch_changed && stored.access == RepoAccess::Granted {
                    let cfg = self.orgs.intake_config(org).await?;
                    self.github.sync_default_head(org, &stored, &cfg).await?;
                }
                Ok(Handled::Processed)
            }
            RepoAction::Deleted => {
                self.repos.set_access(org, repo.id, RepoAccess::Lost).await?;
                Ok(Handled::Processed)
            }
            RepoAction::Archived | RepoAction::Unarchived => Ok(Handled::Processed),
            RepoAction::Other(a) => Ok(Handled::Ignored(format!("repository action {a}"))),
        }
    }

    /// `OAuthApp` revoked (Linear) or `app_uninstalled` / `tokens_revoked` (Slack): `Revoked`,
    /// stored token retired so the adapter stops using it.
    pub async fn provider_revoked(&self, integration: &Integration) -> Result<Handled, IntakeError> {
        let org = integration.org_id;
        self.orgs.set_integration_status(org, integration.kind, &integration.external_id, IntegrationStatus::Revoked, &[]).await?;
        let names = match integration.kind {
            IntegrationKind::Linear => vec![secret_names::linear_oauth(&integration.external_id), secret_names::linear_api_key(&integration.external_id)],
            IntegrationKind::Slack => vec![secret_names::slack_bot_token(&integration.external_id)],
            IntegrationKind::GitHub | IntegrationKind::GitHubFix => vec![],
        };
        let scope = SecretScope::Org(org);
        for name in names {
            let name = SecretName::new(name)?;
            match self.vault.load(&scope, &name).await {
                Ok((meta, _)) => {
                    self.vault.set_state(&scope, &name, meta.version, SecretState::Retired, &Actor::System).await?;
                }
                Err(VaultError::NotFound) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(Handled::Processed)
    }
}

/// Internal v1 start-up: per-org intake config from `[server.intake.orgs]` and the GitHub
/// installations of `[server.github] installations`, both idempotent. Returns the org ids
/// involved (for the periodic chain bootstrap).
pub async fn seed(orgs: &dyn OrgStore, section: &IntakeSection, installs: &[GithubInstallSeed]) -> Result<Vec<OrgId>, AppError> {
    let mut seeded = Vec::new();
    for (slug, config) in &section.orgs {
        let org = orgs.org_by_slug(slug).await?.ok_or_else(|| AppError::Invalid(format!("[server.intake.orgs.{slug}]: no org with slug {slug}")))?;
        orgs.put_intake_config(org.id, config).await?;
        seeded.push(org.id);
    }
    for install in installs {
        let org = orgs
            .org_by_slug(&install.org)
            .await?
            .ok_or_else(|| AppError::Invalid(format!("[server.github] installation {}: no org with slug {}", install.id, install.org)))?;
        let integration = Integration { org_id: org.id, kind: IntegrationKind::GitHub, external_id: install.id.to_string(), status: IntegrationStatus::Active };
        orgs.upsert_integration(&integration, &[NewJob::new(JobPayload::IntakeGithubResync { org_id: org.id })])
            .await
            .map_err(|e| integration_conflict(e, IntegrationKind::GitHub))?;
        if !seeded.contains(&org.id) {
            seeded.push(org.id);
        }
    }
    Ok(seeded)
}

#[cfg(test)]
mod tests;
