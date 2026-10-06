//! Integrations (server.md UC3, catalog `…/integrations*`, `/integrations/*/callback`):
//! listing, connect (one-time install state bound to the user), provider callback (state
//! check, role re-check, `intake::BindIntegration`), revoke. The provider exchanges and token
//! storage are intake's (`BindIntegration`); this use case owns the state and the audit.

use std::sync::Arc;

use shared_auth::pkce::{Pkce, random_state};
use url::Url;

use mage_core::domain::{InstallationId, IntegrationKind, MemberRole, OrgId, UserId};
use mage_core::ports::Clock;
use mage_core::wire::api::{ConnectIntegrationResponse, IntegrationList, integration_slug};

use crate::app::auth::login::STATE_TTL;
use crate::app::auth::sessions::sha256;
use crate::app::auth::{RequestMeta, audit, event};
use crate::app::intake::lifecycle::BindIntegration;
use crate::error::AppError;
use crate::ports::audit::{Actor, AuditAction, AuditLog, AuditOutcome, AuditTarget};
use crate::ports::org::OrgStore;
use crate::ports::source_host_admin::GithubApp;
use crate::ports::tokens::{OAuthPurpose, OAuthState, TokenStore};

/// Query of a provider's redirect back to `/integrations/{kind}/callback`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IntegrationCallback {
    pub state: Option<String>,
    pub code: Option<String>,
    /// GitHub App setup redirect.
    pub installation_id: Option<String>,
    /// GitHub: `install`, `update`, or `request` (an org owner must approve; no installation yet).
    pub setup_action: Option<String>,
    /// Provider-side refusal (`?error=access_denied`).
    pub error: Option<String>,
}

pub struct Integrations {
    orgs: Arc<dyn OrgStore>,
    tokens: Arc<dyn TokenStore>,
    bind: Arc<BindIntegration>,
    audit: Arc<dyn AuditLog>,
    clock: Arc<dyn Clock>,
    public_url: Url,
}

/// Connecting, revoking and finishing an install need Admin; the fix app (code write access)
/// needs Owner (server.md RBAC).
pub fn required_role(kind: IntegrationKind) -> MemberRole {
    if kind == IntegrationKind::GitHubFix { MemberRole::Owner } else { MemberRole::Admin }
}

impl Integrations {
    pub fn new(
        orgs: Arc<dyn OrgStore>,
        tokens: Arc<dyn TokenStore>,
        bind: Arc<BindIntegration>,
        audit: Arc<dyn AuditLog>,
        clock: Arc<dyn Clock>,
        public_url: Url,
    ) -> Integrations {
        Integrations { orgs, tokens, bind, audit, clock, public_url }
    }

    /// `{public_url}/integrations/{slug}/callback`.
    pub fn redirect_uri(&self, kind: IntegrationKind) -> Result<Url, AppError> {
        self.public_url
            .join(&format!("/integrations/{}/callback", integration_slug(kind)))
            .map_err(|e| AppError::Internal(format!("callback url: {e}")))
    }

    pub async fn list(&self, org: OrgId) -> Result<IntegrationList, AppError> {
        Ok(IntegrationList { integrations: self.orgs.integrations(org).await? })
    }

    /// UC3 step 1: install state `Install { org, provider }` bound to the user (10 min,
    /// one-time) and the provider URL to send the browser to.
    pub async fn connect(&self, org: OrgId, user: UserId, role: MemberRole, kind: IntegrationKind) -> Result<ConnectIntegrationResponse, AppError> {
        check_role(role, kind)?;
        let slug = self.orgs.get_org(org).await?.slug;
        let state = random_state();
        let pkce = Pkce::generate();
        self.tokens
            .put_oauth_state(&OAuthState {
                state_hash: sha256(&state),
                purpose: OAuthPurpose::Install { org, provider: kind },
                user: Some(user),
                pkce_verifier: Some(pkce.verifier.clone()),
                return_to: Some(format!("/o/{slug}/settings/integrations")),
                expires_at: self.clock.now() + STATE_TTL,
            })
            .await?;
        let url = self.bind.connect_url(kind, &state, &pkce.challenge, &self.redirect_uri(kind)?)?;
        Ok(ConnectIntegrationResponse { url })
    }

    /// UC3 steps 2–4. Returns where to send the browser: the settings page, with
    /// `?error=<code>` on failure (`/?integration_error=<code>` before the state is known).
    pub async fn callback(&self, user: UserId, kind: IntegrationKind, cb: &IntegrationCallback, meta: &RequestMeta) -> String {
        let Some(state) = cb.state.as_deref().filter(|s| !s.is_empty()) else {
            return "/?integration_error=state_expired".into();
        };
        let stored = match self.tokens.take_oauth_state(&sha256(state)).await {
            Ok(Some(stored)) => stored,
            Ok(None) => return "/?integration_error=state_expired".into(),
            Err(e) => {
                tracing::warn!(error = %e, "taking an install state failed");
                return "/?integration_error=unavailable".into();
            }
        };
        let back = stored.return_to.clone().unwrap_or_else(|| "/".into());
        match self.finish(user, kind, &stored, cb, meta).await {
            Ok(()) => back,
            Err(code) => format!("{back}?error={code}"),
        }
    }

    async fn finish(&self, user: UserId, kind: IntegrationKind, stored: &OAuthState, cb: &IntegrationCallback, meta: &RequestMeta) -> Result<(), &'static str> {
        let OAuthPurpose::Install { org, provider } = stored.purpose else {
            return Err("state_expired");
        };
        if provider != kind || stored.user != Some(user) {
            return Err("forbidden");
        }
        // Still allowed to connect: the role may have changed since the state was issued.
        let role = match self.orgs.membership(org, user).await {
            Ok(Some(m)) => m.role,
            Ok(None) => return Err("forbidden"),
            Err(_) => return Err("unavailable"),
        };
        if check_role(role, kind).is_err() {
            return Err("forbidden");
        }
        if cb.error.is_some() {
            return Err("denied");
        }
        let actor = Actor::User(user);
        let result = self.bind(org, kind, stored, cb, &actor).await;
        match result {
            Ok(external_id) => {
                let recorded = audit(
                    self.audit.as_ref(),
                    event(Some(org), actor, AuditAction::IntegrationConnected, AuditTarget::new("integration", format!("{}/{external_id}", integration_slug(kind))), meta)
                        .detail("provider", integration_slug(kind))
                        .detail("external_id", external_id),
                )
                .await;
                recorded.map_err(|_| "internal")
            }
            Err(AppError::Invalid(reason)) => {
                tracing::warn!(provider = integration_slug(kind), %reason, "integration install rejected");
                let mut rejected = event(Some(org), actor, AuditAction::IntegrationInstallRejected, AuditTarget::new("integration", integration_slug(kind)), meta)
                    .detail("provider", integration_slug(kind));
                if let Some(id) = &cb.installation_id {
                    rejected = rejected.detail("installation_id", id.clone());
                }
                rejected.0.outcome = AuditOutcome::Denied;
                audit(self.audit.as_ref(), rejected).await.map_err(|_| "internal")?;
                Err("rejected")
            }
            Err(AppError::Unavailable(_) | AppError::RateLimited { .. }) => Err("unavailable"),
            Err(e) => {
                tracing::warn!(provider = integration_slug(kind), error = %e, "integration install failed");
                Err("failed")
            }
        }
    }

    /// Runs the provider exchange; returns the integration's external id.
    async fn bind(&self, org: OrgId, kind: IntegrationKind, stored: &OAuthState, cb: &IntegrationCallback, actor: &Actor) -> Result<String, AppError> {
        let code = cb.code.as_deref().filter(|c| !c.is_empty());
        let integration = match kind {
            IntegrationKind::GitHub | IntegrationKind::GitHubFix => {
                if cb.setup_action.as_deref() == Some("request") {
                    return Err(AppError::Invalid("installation awaits approval by a GitHub organization owner".into()));
                }
                let installation = cb
                    .installation_id
                    .as_deref()
                    .and_then(|id| id.parse::<u64>().ok())
                    .ok_or_else(|| AppError::Invalid("missing installation_id".into()))?;
                let code = code.ok_or_else(|| AppError::Invalid("missing code".into()))?;
                let app = if kind == IntegrationKind::GitHub { GithubApp::Read } else { GithubApp::Fix };
                self.bind.github(org, app, InstallationId(installation), code, actor).await?
            }
            IntegrationKind::Linear => {
                let code = code.ok_or_else(|| AppError::Invalid("missing code".into()))?;
                let verifier = stored.pkce_verifier.as_deref().ok_or_else(|| AppError::Invalid("state without PKCE verifier".into()))?;
                self.bind.linear(org, code, &self.redirect_uri(kind)?, verifier, actor).await?
            }
            IntegrationKind::Slack => {
                let code = code.ok_or_else(|| AppError::Invalid("missing code".into()))?;
                self.bind.slack(org, code, &self.redirect_uri(kind)?, actor).await?
            }
        };
        Ok(integration.external_id)
    }

    /// `DELETE …/integrations/{kind}/{external_id}`: revoke and destroy stored tokens.
    pub async fn revoke(&self, org: OrgId, user: UserId, role: MemberRole, kind: IntegrationKind, external_id: &str, meta: &RequestMeta) -> Result<(), AppError> {
        check_role(role, kind)?;
        let actor = Actor::User(user);
        self.bind.revoke(org, kind, external_id, &actor).await?;
        audit(
            self.audit.as_ref(),
            event(Some(org), actor, AuditAction::IntegrationRevoked, AuditTarget::new("integration", format!("{}/{external_id}", integration_slug(kind))), meta)
                .detail("provider", integration_slug(kind))
                .detail("external_id", external_id.to_owned()),
        )
        .await
    }
}

fn check_role(role: MemberRole, kind: IntegrationKind) -> Result<(), AppError> {
    let needed = required_role(kind);
    if role.at_least(needed) {
        Ok(())
    } else {
        Err(AppError::Forbidden(format!("{} integrations need the {needed:?} role", integration_slug(kind))))
    }
}
