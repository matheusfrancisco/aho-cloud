//! Org → Linear workspace → credential (intake.md "Auth", "Credentials"): the org's active
//! `Linear` integration names the workspace, its settings pick API key or OAuth, the secret comes
//! from `SecretVault` (Org scope). Decrypted credentials are cached 5 minutes; OAuth tokens are
//! refreshed single-flight per workspace on expiry or 401 and written back immediately.

use std::collections::HashMap;
use std::sync::Arc;

use mage_core::domain::{IntegrationKind, IntegrationStatus, OrgId};
use mage_core::ports::Clock;
use parking_lot::Mutex;
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};
use url::Url;

use super::client::{transport_error, Caller};
use crate::domain::intake::{secret_names, LinearAuthMode};
use crate::ports::audit::Actor;
use crate::ports::org::{OrgStore, OrgStoreError};
use crate::ports::tracker::TrackerError;
use crate::ports::vault::{Activation, SecretBytes, SecretName, SecretScope, SecretVault, VaultError};

/// How long decrypted credentials stay in memory.
const CREDENTIAL_TTL: Duration = Duration::minutes(5);
/// An OAuth token this close to `expires_at` is refreshed before use.
const EXPIRY_MARGIN: Duration = Duration::seconds(60);

/// The stored `integration/linear/{workspace}/oauth` value.
#[derive(Serialize, Deserialize)]
pub(super) struct OAuthSecret {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub expires_at: Option<OffsetDateTime>,
}

impl OAuthSecret {
    fn expiring(&self, now: OffsetDateTime) -> bool {
        self.expires_at.is_some_and(|at| at - EXPIRY_MARGIN <= now)
    }
}

/// A resolved credential for one org's Linear workspace.
#[derive(Clone)]
pub(super) struct Session {
    pub org: OrgId,
    pub workspace_id: String,
    pub mode: LinearAuthMode,
    /// The `Authorization` header value: the key itself or `Bearer <token>`.
    authorization: SecretBytes,
    expires_at: Option<OffsetDateTime>,
    loaded_at: OffsetDateTime,
}

impl Session {
    pub(super) fn caller(&self) -> Result<Caller<'_>, TrackerError> {
        let authorization = self.authorization.expose_str().ok_or_else(|| TrackerError::Invalid("Linear credential is not UTF-8".into()))?;
        Ok(Caller { workspace_id: &self.workspace_id, mode: self.mode, authorization })
    }

    fn oauth(org: OrgId, workspace_id: String, secret: &OAuthSecret, now: OffsetDateTime) -> Session {
        Session {
            org,
            workspace_id,
            mode: LinearAuthMode::OAuth,
            authorization: SecretBytes::from(format!("Bearer {}", secret.access_token)),
            expires_at: secret.expires_at,
            loaded_at: now,
        }
    }

    fn fresh(&self, now: OffsetDateTime) -> bool {
        now - self.loaded_at < CREDENTIAL_TTL && self.expires_at.is_none_or(|at| at - EXPIRY_MARGIN > now)
    }
}

/// Successful answer of the OAuth token endpoint.
#[derive(Deserialize)]
pub(super) struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<i64>,
    /// Space- or comma-separated.
    #[serde(default)]
    pub scope: Option<String>,
}

/// Failure of the OAuth token endpoint.
pub(super) enum TokenError {
    /// 4xx: the code, verifier, refresh token or client credentials were refused.
    Rejected(String),
    Tracker(TrackerError),
}

#[derive(Deserialize)]
struct OAuthErrorBody {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

pub(super) fn vault_error(err: VaultError) -> TrackerError {
    match err {
        VaultError::NotFound => TrackerError::Unauthorized,
        VaultError::Unavailable(msg) => TrackerError::Unavailable(format!("secret vault: {msg}")),
        other => TrackerError::Unavailable(format!("secret vault: {other}")),
    }
}

fn org_error(err: OrgStoreError) -> TrackerError {
    match err {
        OrgStoreError::NotFound => TrackerError::Unauthorized,
        OrgStoreError::Unavailable(msg) => TrackerError::Unavailable(format!("org store: {msg}")),
        other => TrackerError::Unavailable(format!("org store: {other}")),
    }
}

fn secret_name(name: String) -> Result<SecretName, TrackerError> {
    SecretName::new(name).map_err(|e| TrackerError::Invalid(e.to_string()))
}

pub(super) struct Credentials {
    vault: Arc<dyn SecretVault>,
    orgs: Arc<dyn OrgStore>,
    clock: Arc<dyn Clock>,
    http: reqwest::Client,
    token_url: Url,
    client_id: Option<String>,
    cache: Mutex<HashMap<OrgId, Session>>,
    refresh_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl Credentials {
    pub(super) fn new(
        vault: Arc<dyn SecretVault>,
        orgs: Arc<dyn OrgStore>,
        clock: Arc<dyn Clock>,
        http: reqwest::Client,
        token_url: Url,
        client_id: Option<String>,
    ) -> Self {
        Credentials { vault, orgs, clock, http, token_url, client_id, cache: Mutex::new(HashMap::new()), refresh_locks: Mutex::new(HashMap::new()) }
    }

    pub(super) fn client_id(&self) -> Result<&str, TrackerError> {
        self.client_id.as_deref().ok_or_else(|| TrackerError::Invalid("Linear OAuth client id (linear.oauth_client_id) is not configured".into()))
    }

    /// The org's credential: cached for 5 minutes; an expiring OAuth token is refreshed first.
    pub(super) async fn session(&self, org: OrgId) -> Result<Session, TrackerError> {
        let now = self.clock.now();
        let cached = self.cache.lock().get(&org).cloned();
        if let Some(session) = cached
            && session.fresh(now)
        {
            return Ok(session);
        }
        let (workspace_id, mode) = self.workspace(org).await?;
        let scope = SecretScope::Org(org);
        let session = match mode {
            LinearAuthMode::ApiKey => {
                let name = secret_name(secret_names::linear_api_key(&workspace_id))?;
                let (_, key) = self.vault.load(&scope, &name).await.map_err(vault_error)?;
                Session { org, workspace_id, mode, authorization: key, expires_at: None, loaded_at: now }
            }
            LinearAuthMode::OAuth => {
                let secret = self.load_oauth(org, &workspace_id).await?;
                if secret.expiring(now) {
                    return self.refresh_locked(org, workspace_id, None).await;
                }
                Session::oauth(org, workspace_id, &secret, now)
            }
        };
        self.cache.lock().insert(org, session.clone());
        Ok(session)
    }

    /// After a 401: OAuth refreshes (unless another task already did); API key is evicted and
    /// stays `Unauthorized`.
    pub(super) async fn refresh(&self, stale: &Session) -> Result<Session, TrackerError> {
        match stale.mode {
            LinearAuthMode::ApiKey => {
                self.evict(stale.org);
                Err(TrackerError::Unauthorized)
            }
            LinearAuthMode::OAuth => self.refresh_locked(stale.org, stale.workspace_id.clone(), Some(stale.authorization.clone())).await,
        }
    }

    pub(super) fn evict(&self, org: OrgId) {
        self.cache.lock().remove(&org);
    }

    /// `(workspace id, auth mode)` of the org's active Linear integration. Settings without an
    /// auth mode mean API key (internal v1 seeds API-key integrations).
    async fn workspace(&self, org: OrgId) -> Result<(String, LinearAuthMode), TrackerError> {
        let integrations = self.orgs.integrations(org).await.map_err(org_error)?;
        let integration = integrations
            .into_iter()
            .find(|i| i.kind == IntegrationKind::Linear && i.status == IntegrationStatus::Active)
            .ok_or(TrackerError::Unauthorized)?;
        let settings = self.orgs.integration_settings(org, IntegrationKind::Linear, &integration.external_id).await.map_err(org_error)?;
        Ok((integration.external_id, settings.linear_auth.unwrap_or(LinearAuthMode::ApiKey)))
    }

    async fn load_oauth(&self, org: OrgId, workspace_id: &str) -> Result<OAuthSecret, TrackerError> {
        let name = secret_name(secret_names::linear_oauth(workspace_id))?;
        let (_, bytes) = self.vault.load(&SecretScope::Org(org), &name).await.map_err(vault_error)?;
        serde_json::from_slice(bytes.expose()).map_err(|e| TrackerError::Invalid(format!("stored Linear OAuth grant is malformed: {e}")))
    }

    /// Single-flight per workspace: re-reads the stored grant under the lock and refreshes only if
    /// it is still the token that failed (`stale`) or is expiring.
    async fn refresh_locked(&self, org: OrgId, workspace_id: String, stale: Option<SecretBytes>) -> Result<Session, TrackerError> {
        let lock = self.refresh_locks.lock().entry(workspace_id.clone()).or_default().clone();
        let _guard = lock.lock().await;
        let current = self.load_oauth(org, &workspace_id).await?;
        let now = self.clock.now();
        let current_session = Session::oauth(org, workspace_id.clone(), &current, now);
        // Reuse the stored grant when it is not expiring and is not the token that just failed
        // (another task refreshed it, or the caller only saw an expiring cached copy).
        let usable = !current.expiring(now) && stale.as_ref().is_none_or(|s| *s != current_session.authorization);
        if usable {
            self.cache.lock().insert(org, current_session.clone());
            return Ok(current_session);
        }
        let Some(refresh_token) = current.refresh_token.as_deref() else {
            self.evict(org);
            return Err(TrackerError::Unauthorized);
        };
        let client_id = self.client_id()?;
        let client_secret = self.client_secret().await?;
        let client_secret = client_secret.expose_str().ok_or_else(|| TrackerError::Invalid("Linear OAuth client secret is not UTF-8".into()))?;
        let grant = self
            .token_request(&[("grant_type", "refresh_token"), ("refresh_token", refresh_token), ("client_id", client_id), ("client_secret", client_secret)])
            .await
            .map_err(|e| match e {
                TokenError::Rejected(_) => TrackerError::Unauthorized,
                TokenError::Tracker(e) => e,
            });
        let grant = match grant {
            Ok(grant) => grant,
            Err(err) => {
                self.evict(org);
                return Err(err);
            }
        };
        let next = OAuthSecret {
            access_token: grant.access_token,
            refresh_token: grant.refresh_token.or(current.refresh_token),
            expires_at: grant.expires_in.map(|secs| now + Duration::seconds(secs)),
        };
        let json = serde_json::to_vec(&next).map_err(|e| TrackerError::Unavailable(format!("encode Linear OAuth grant: {e}")))?;
        let name = secret_name(secret_names::linear_oauth(&workspace_id))?;
        self.vault.put(&SecretScope::Org(org), &name, SecretBytes::from(json), Activation::Now, &Actor::System).await.map_err(vault_error)?;
        let session = Session::oauth(org, workspace_id, &next, now);
        self.cache.lock().insert(org, session.clone());
        Ok(session)
    }

    /// Platform `linear/oauth_client_secret`.
    pub(super) async fn client_secret(&self) -> Result<SecretBytes, TrackerError> {
        let name = secret_name(secret_names::LINEAR_OAUTH_CLIENT_SECRET.to_owned())?;
        match self.vault.load(&SecretScope::Platform, &name).await {
            Ok((_, secret)) => Ok(secret),
            Err(VaultError::NotFound) => Err(TrackerError::Invalid("Platform secret linear/oauth_client_secret is not configured".into())),
            Err(err) => Err(vault_error(err)),
        }
    }

    /// Form-encoded POST to the OAuth token endpoint.
    pub(super) async fn token_request(&self, form: &[(&str, &str)]) -> Result<TokenResponse, TokenError> {
        let body = url::form_urlencoded::Serializer::new(String::new()).extend_pairs(form).finish();
        let response = self
            .http
            .post(self.token_url.clone())
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(ACCEPT, "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| TokenError::Tracker(transport_error(&e)))?;
        let status = response.status();
        let bytes = response.bytes().await.map_err(|e| TokenError::Tracker(transport_error(&e)))?;
        if status.is_success() {
            return serde_json::from_slice(&bytes).map_err(|e| TokenError::Tracker(TrackerError::Unavailable(format!("Linear token endpoint: unexpected response: {e}"))));
        }
        if status.is_client_error() && status.as_u16() != 429 {
            let detail = serde_json::from_slice::<OAuthErrorBody>(&bytes)
                .ok()
                .map(|b| [b.error, b.error_description].into_iter().flatten().collect::<Vec<_>>().join(": "))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| format!("HTTP {status}"));
            return Err(TokenError::Rejected(detail));
        }
        Err(TokenError::Tracker(TrackerError::Unavailable(format!("Linear token endpoint: HTTP {status}"))))
    }
}
