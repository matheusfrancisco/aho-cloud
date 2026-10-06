//! UC1 Login (server.md "Login (GitHub, self-host)", "Login (Google, self-host)", UC1):
//! one-time `state` + PKCE stored hashed in `TokenStore`, identity key `(provider, subject)`,
//! pending invites converted on login, explicit provider linking only, logout.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;

use parking_lot::Mutex;
use shared_auth::pkce::{Pkce, random_state};
use time::OffsetDateTime;
use url::Url;

use mage_core::domain::{Membership, UserId};
use mage_core::ports::{Clock, IdGen};

use crate::app::auth::sessions::{SessionCookie, SessionId, Sessions, sha256};
use crate::app::auth::{RequestMeta, audit, event, role_name};
use crate::error::AppError;
use crate::ports::audit::{Actor, AuditAction, AuditLog, AuditOutcome, AuditTarget};
use crate::ports::auth::{AuthorizeParams, ExternalIdentity, IdentityError, IdentityProvider, IdentityProviderKind};
use crate::ports::org::{OrgStore, OrgStoreError};
use crate::ports::tokens::{OAuthPurpose, OAuthState, TokenStore};
use crate::ports::vault::constant_time_eq;

/// Login and install states live 10 minutes and are consumed once.
pub const STATE_TTL: time::Duration = time::Duration::minutes(10);
const RETURN_TO_MAX: usize = 2048;

/// `return_to` must be a relative path starting with `/` and not `//` or `/\`; anything else
/// (absolute URLs, scheme-relative, control characters a browser would strip into `//`) → `/`.
pub fn safe_return_to(raw: Option<&str>) -> String {
    let Some(raw) = raw else {
        return "/".into();
    };
    let ok = raw.starts_with('/')
        && !raw.starts_with("//")
        && !raw.starts_with("/\\")
        && raw.len() <= RETURN_TO_MAX
        && !raw.contains('\\')
        && !raw.chars().any(|c| c.is_control());
    if ok { raw.to_owned() } else { "/".into() }
}

/// Wiring for the login flows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginConfig {
    /// `public_url`: callback URLs are `{public_url}/auth/{provider}/callback`.
    pub public_url: Url,
}

/// Start of a login or link: set `state` as the `__Host-mage_oauth` cookie, redirect to `url`.
#[derive(Clone, PartialEq, Eq)]
pub struct LoginStarted {
    pub state: String,
    pub authorize_url: String,
    pub expires_at: OffsetDateTime,
}

impl std::fmt::Debug for LoginStarted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginStarted").field("expires_at", &self.expires_at).finish_non_exhaustive()
    }
}

/// End of a successful callback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginFinished {
    /// New session (login); `None` for a link (the current session stays).
    pub session: Option<SessionCookie>,
    pub user_id: UserId,
    pub return_to: String,
    /// Memberships created from pending invites.
    pub joined: Vec<Membership>,
}

/// Why a callback failed; `code` goes to `/login?error=<code>` (no detail to the browser).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("login failed ({code}): {detail}")]
pub struct LoginFailure {
    pub code: &'static str,
    pub detail: String,
}

impl LoginFailure {
    fn new(code: &'static str, detail: impl Into<String>) -> LoginFailure {
        LoginFailure { code, detail: detail.into() }
    }

    fn outcome(&self) -> AuditOutcome {
        match self.code {
            "unavailable" | "internal" => AuditOutcome::Failed,
            _ => AuditOutcome::Denied,
        }
    }
}

/// The callback query and request facts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Callback<'a> {
    pub code: Option<&'a str>,
    pub state: Option<&'a str>,
    /// Provider-side refusal (`?error=access_denied`).
    pub error: Option<&'a str>,
    /// `__Host-mage_oauth` cookie value.
    pub cookie_state: Option<&'a str>,
    /// The session user, if the browser has a session (required for links).
    pub current_user: Option<UserId>,
}

pub struct Login {
    providers: HashMap<IdentityProviderKind, Arc<dyn IdentityProvider>>,
    tokens: Arc<dyn TokenStore>,
    orgs: Arc<dyn OrgStore>,
    sessions: Arc<Sessions>,
    audit: Arc<dyn AuditLog>,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGen>,
    config: LoginConfig,
    /// `auth.login_failed` is recorded once per client IP per minute (server.md "Audit log").
    failures: Mutex<HashMap<Option<IpAddr>, (i64, u32)>>,
}

impl Login {
    pub fn new(
        providers: Vec<Arc<dyn IdentityProvider>>,
        tokens: Arc<dyn TokenStore>,
        orgs: Arc<dyn OrgStore>,
        sessions: Arc<Sessions>,
        audit: Arc<dyn AuditLog>,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGen>,
        config: LoginConfig,
    ) -> Login {
        let providers = providers.into_iter().map(|p| (p.kind(), p)).collect();
        Login { providers, tokens, orgs, sessions, audit, clock, ids, config, failures: Mutex::new(HashMap::new()) }
    }

    /// Configured login providers (session `features`, login page buttons).
    pub fn providers(&self) -> Vec<IdentityProviderKind> {
        let mut kinds: Vec<_> = self.providers.keys().copied().collect();
        kinds.sort_by_key(|k| k.as_str());
        kinds
    }

    pub fn redirect_uri(&self, provider: IdentityProviderKind) -> String {
        format!("{}/auth/{}/callback", self.config.public_url.as_str().trim_end_matches('/'), provider.as_str())
    }

    fn provider(&self, kind: IdentityProviderKind) -> Result<&Arc<dyn IdentityProvider>, AppError> {
        self.providers.get(&kind).ok_or_else(|| AppError::NotFound(format!("{} login is not configured", kind.as_str())))
    }

    /// UC1 step 1 (`link_user = None`) or the start of an explicit provider link for the
    /// logged-in `link_user` (Settings → "Connect GitHub/Google").
    pub async fn start(&self, provider: IdentityProviderKind, return_to: Option<&str>, link_user: Option<UserId>) -> Result<LoginStarted, AppError> {
        let idp = self.provider(provider)?;
        let state = random_state();
        let pkce = Pkce::generate();
        let expires_at = self.clock.now() + STATE_TTL;
        self.tokens
            .put_oauth_state(&OAuthState {
                state_hash: sha256(&state),
                purpose: OAuthPurpose::Login,
                user: link_user,
                pkce_verifier: Some(pkce.verifier.clone()),
                return_to: Some(safe_return_to(return_to)),
                expires_at,
            })
            .await?;
        let authorize_url =
            idp.authorize_url(&AuthorizeParams { state: state.clone(), pkce_challenge: pkce.challenge.clone(), redirect_uri: self.redirect_uri(provider) });
        Ok(LoginStarted { state, authorize_url, expires_at })
    }

    /// UC1 steps 2–5. Every failure is audited (`auth.login_failed`, aggregated per IP per
    /// minute) and reported as a [`LoginFailure`] code.
    pub async fn finish(&self, provider: IdentityProviderKind, callback: Callback<'_>, meta: &RequestMeta) -> Result<LoginFinished, LoginFailure> {
        match self.finish_inner(provider, &callback, meta).await {
            Ok(done) => Ok(done),
            Err(failure) => {
                tracing::info!(provider = provider.as_str(), code = failure.code, detail = %failure.detail, "login failed");
                if let Err(e) = self.record_failure(provider, &failure, callback.current_user, meta).await {
                    tracing::error!(error = %e, "recording auth.login_failed failed");
                    return Err(LoginFailure::new("internal", "audit write failed"));
                }
                Err(failure)
            }
        }
    }

    async fn finish_inner(&self, provider: IdentityProviderKind, cb: &Callback<'_>, meta: &RequestMeta) -> Result<LoginFinished, LoginFailure> {
        let idp = self.provider(provider).map_err(|e| LoginFailure::new("invalid", e.to_string()))?.clone();
        let (Some(state), Some(cookie_state)) = (cb.state, cb.cookie_state) else {
            return Err(LoginFailure::new("state_mismatch", "missing state or pre-login cookie"));
        };
        if !constant_time_eq(state.as_bytes(), cookie_state.as_bytes()) {
            return Err(LoginFailure::new("state_mismatch", "state differs from the pre-login cookie"));
        }
        let stored = self
            .tokens
            .take_oauth_state(&sha256(state))
            .await
            .map_err(|e| LoginFailure::new("unavailable", e.to_string()))?
            .ok_or_else(|| LoginFailure::new("state_expired", "unknown, expired or already used state"))?;
        if stored.purpose != OAuthPurpose::Login {
            return Err(LoginFailure::new("state_mismatch", "state was issued for an integration install"));
        }
        if let Some(error) = cb.error {
            return Err(LoginFailure::new("denied", format!("provider error: {error}")));
        }
        let code = cb.code.filter(|c| !c.is_empty()).ok_or_else(|| LoginFailure::new("invalid", "missing code"))?;
        let verifier = stored.pkce_verifier.as_deref().ok_or_else(|| LoginFailure::new("invalid", "state without PKCE verifier"))?;
        let identity = idp.exchange(code, verifier, &self.redirect_uri(provider)).await.map_err(|e| match e {
            IdentityError::Denied => LoginFailure::new("denied", "provider denied the login"),
            IdentityError::Invalid(m) => LoginFailure::new("invalid", m),
            IdentityError::Unavailable(m) => LoginFailure::new("unavailable", m),
        })?;
        if identity.provider != provider {
            return Err(LoginFailure::new("invalid", "identity from another provider"));
        }
        let return_to = safe_return_to(stored.return_to.as_deref());
        match stored.user {
            Some(link_user) => self.link(link_user, cb.current_user, &identity, return_to, meta).await,
            None => self.login(&identity, return_to, meta).await,
        }
    }

    async fn login(&self, identity: &ExternalIdentity, return_to: String, meta: &RequestMeta) -> Result<LoginFinished, LoginFailure> {
        let user = self.orgs.upsert_user_by_identity(identity, self.ids.new_id()).await.map_err(|e| match e {
            // An unknown identity whose email belongs to another user: never auto-linked.
            OrgStoreError::Conflict => LoginFailure::new("identity_conflict", "email already used by another account"),
            OrgStoreError::Invalid(m) => LoginFailure::new("invalid", m),
            other => LoginFailure::new("unavailable", other.to_string()),
        })?;
        let joined = self
            .orgs
            .accept_invites(identity.provider.as_str(), &identity.subject, user.id)
            .await
            .map_err(|e| LoginFailure::new("unavailable", e.to_string()))?;
        let internal = |e: AppError| LoginFailure::new("internal", e.to_string());
        for m in &joined {
            audit(
                self.audit.as_ref(),
                event(Some(m.org_id), Actor::User(user.id), AuditAction::MemberJoined, AuditTarget::new("user", user.id.to_string()), meta)
                    .detail("role", role_name(m.role)),
            )
            .await
            .map_err(internal)?;
        }
        let session = self.sessions.create(user.id, meta.ip, meta.user_agent.clone()).await.map_err(|e| LoginFailure::new("unavailable", e.to_string()))?;
        audit(
            self.audit.as_ref(),
            event(None, Actor::User(user.id), AuditAction::AuthLogin, AuditTarget::new("user", user.id.to_string()), meta)
                .detail("provider", identity.provider.as_str()),
        )
        .await
        .map_err(internal)?;
        Ok(LoginFinished { session: Some(session), user_id: user.id, return_to, joined })
    }

    async fn link(
        &self,
        link_user: UserId,
        current_user: Option<UserId>,
        identity: &ExternalIdentity,
        return_to: String,
        meta: &RequestMeta,
    ) -> Result<LoginFinished, LoginFailure> {
        if current_user != Some(link_user) {
            return Err(LoginFailure::new("link_session_mismatch", "linking needs the session that started it"));
        }
        self.orgs.link_identity(identity, link_user).await.map_err(|e| match e {
            OrgStoreError::Conflict => LoginFailure::new("identity_in_use", "identity belongs to another account"),
            OrgStoreError::NotFound => LoginFailure::new("invalid", "user not found"),
            other => LoginFailure::new("unavailable", other.to_string()),
        })?;
        let joined = self
            .orgs
            .accept_invites(identity.provider.as_str(), &identity.subject, link_user)
            .await
            .map_err(|e| LoginFailure::new("unavailable", e.to_string()))?;
        audit(
            self.audit.as_ref(),
            event(None, Actor::User(link_user), AuditAction::AuthLogin, AuditTarget::new("identity", format!("{}:{}", identity.provider, identity.subject)), meta)
                .detail("provider", identity.provider.as_str())
                .detail("linked", "true"),
        )
        .await
        .map_err(|e| LoginFailure::new("internal", e.to_string()))?;
        Ok(LoginFinished { session: None, user_id: link_user, return_to, joined })
    }

    async fn record_failure(&self, provider: IdentityProviderKind, failure: &LoginFailure, user: Option<UserId>, meta: &RequestMeta) -> Result<(), AppError> {
        let minute = self.clock.now().unix_timestamp() / 60;
        let first = {
            let mut failures = self.failures.lock();
            failures.retain(|_, (m, _)| *m == minute);
            let entry = failures.entry(meta.ip).or_insert((minute, 0));
            entry.1 += 1;
            entry.1 == 1
        };
        if !first {
            return Ok(());
        }
        let actor = user.map_or(Actor::System, Actor::User);
        let mut e = event(None, actor, AuditAction::AuthLoginFailed, AuditTarget::new("login", provider.as_str()), meta)
            .detail("provider", provider.as_str())
            .detail("reason", failure.code);
        e.0.outcome = failure.outcome();
        audit(self.audit.as_ref(), e.0).await
    }

    /// `POST /auth/logout`: this session, or every session of the user (`all`).
    pub async fn logout(&self, session: SessionId, user: UserId, all: bool, meta: &RequestMeta) -> Result<(), AppError> {
        if all {
            let ended = self.sessions.end_all(user).await?;
            audit(
                self.audit.as_ref(),
                event(None, Actor::User(user), AuditAction::AuthSessionsRevoked, AuditTarget::new("user", user.to_string()), meta)
                    .detail("sessions", ended.to_string()),
            )
            .await
        } else {
            self.sessions.end(session).await?;
            audit(self.audit.as_ref(), event(None, Actor::User(user), AuditAction::AuthLogout, AuditTarget::new("user", user.to_string()), meta).0).await
        }
    }
}
