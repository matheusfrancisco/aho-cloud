//! Google OIDC: authorization code flow with PKCE (S256), `state` and `nonce`. The `id_token` is
//! verified locally against Google's JWKS (RS256; issuer, audience = client id, expiry, nonce) and
//! `email_verified` must be true. Fixed endpoints (no discovery), overridable for tests.

use std::time::{Duration, Instant};

use jsonwebtoken::jwk::{Jwk, JwkSet};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use reqwest::StatusCode;
use reqwest::header::ACCEPT;
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::http::{exchange_code, read_json, transport, with_query};
use crate::pkce::{CHALLENGE_METHOD, is_valid_verifier};
use crate::{AuthError, AuthorizeParams, ExternalIdentity, Provider};

pub const AUTHORIZE_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
pub const JWKS_URL: &str = "https://www.googleapis.com/oauth2/v3/certs";
/// Google issues `id_token`s with either issuer spelling.
pub const ISSUERS: [&str; 2] = ["https://accounts.google.com", "accounts.google.com"];
pub const DEFAULT_SCOPE: &str = "openid email profile";
/// Clock-skew allowance for `exp` (server.md: 30 s leeway).
pub const LEEWAY: Duration = Duration::from_secs(30);
/// Cached JWKS older than this is refetched even when the `kid` is known (Google rotates keys).
pub const DEFAULT_JWKS_MAX_AGE: Duration = Duration::from_secs(60 * 60);

/// Google OIDC client configuration.
#[derive(Clone)]
pub struct GoogleConfig {
    pub client_id: String,
    pub client_secret: String,
    pub scope: String,
    pub authorize_url: String,
    pub token_url: String,
    pub jwks_url: String,
    /// Accepted `iss` values.
    pub issuers: Vec<String>,
    pub jwks_max_age: Duration,
}

impl GoogleConfig {
    /// Google's endpoints and issuers, [`DEFAULT_SCOPE`], [`DEFAULT_JWKS_MAX_AGE`].
    pub fn new(client_id: impl Into<String>, client_secret: impl Into<String>) -> Self {
        Self {
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            scope: DEFAULT_SCOPE.to_owned(),
            authorize_url: AUTHORIZE_URL.to_owned(),
            token_url: TOKEN_URL.to_owned(),
            jwks_url: JWKS_URL.to_owned(),
            issuers: ISSUERS.iter().map(|s| (*s).to_owned()).collect(),
            jwks_max_age: DEFAULT_JWKS_MAX_AGE,
        }
    }
}

impl std::fmt::Debug for GoogleConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoogleConfig")
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .field("scope", &self.scope)
            .field("authorize_url", &self.authorize_url)
            .field("token_url", &self.token_url)
            .field("jwks_url", &self.jwks_url)
            .field("issuers", &self.issuers)
            .field("jwks_max_age", &self.jwks_max_age)
            .finish()
    }
}

#[derive(Debug)]
struct CachedJwks {
    set: JwkSet,
    fetched_at: Instant,
}

/// Google login provider. Holds the JWKS cache; share one instance (e.g. in an `Arc`).
#[derive(Debug)]
pub struct GoogleOidc {
    config: GoogleConfig,
    http: reqwest::Client,
    /// Also serializes refreshes, so concurrent logins with a new `kid` fetch once.
    jwks: Mutex<Option<CachedJwks>>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Flag {
    Bool(bool),
    Text(String),
}

impl Flag {
    fn is_true(&self) -> bool {
        match self {
            Self::Bool(b) => *b,
            Self::Text(s) => s == "true",
        }
    }
}

/// Claims read after `jsonwebtoken` has checked signature, `iss`, `aud`, `exp`.
#[derive(Deserialize)]
struct IdTokenClaims {
    sub: String,
    nonce: Option<String>,
    email: Option<String>,
    email_verified: Option<Flag>,
    name: Option<String>,
    picture: Option<String>,
}

impl GoogleOidc {
    pub fn new(config: GoogleConfig, http: reqwest::Client) -> Self {
        Self { config, http, jwks: Mutex::new(None) }
    }

    pub fn config(&self) -> &GoogleConfig {
        &self.config
    }

    /// Authorize redirect: `response_type=code`, `client_id`, `redirect_uri`, `scope`, `state`,
    /// `nonce`, `code_challenge`, `code_challenge_method=S256`.
    pub fn authorize_url(&self, params: &AuthorizeParams, nonce: &str) -> String {
        with_query(
            &self.config.authorize_url,
            [
                ("response_type", "code"),
                ("client_id", self.config.client_id.as_str()),
                ("redirect_uri", params.redirect_uri.as_str()),
                ("scope", self.config.scope.as_str()),
                ("state", params.state.as_str()),
                ("nonce", nonce),
                ("code_challenge", params.pkce_challenge.as_str()),
                ("code_challenge_method", CHALLENGE_METHOD),
            ],
        )
    }

    /// Exchange `code` + PKCE verifier, verify the returned `id_token` against `nonce`, and
    /// return the identity. Google's access token is dropped unused.
    pub async fn exchange(
        &self,
        code: &str,
        pkce_verifier: &str,
        redirect_uri: &str,
        nonce: &str,
    ) -> Result<ExternalIdentity, AuthError> {
        if code.is_empty() {
            return Err(AuthError::invalid("empty authorization code"));
        }
        if !is_valid_verifier(pkce_verifier) {
            return Err(AuthError::invalid("malformed PKCE verifier"));
        }
        if nonce.is_empty() {
            return Err(AuthError::invalid("empty nonce"));
        }
        let request = self
            .http
            .post(&self.config.token_url)
            .header(ACCEPT, "application/json")
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("client_id", self.config.client_id.as_str()),
                ("client_secret", self.config.client_secret.as_str()),
                ("redirect_uri", redirect_uri),
                ("code_verifier", pkce_verifier),
            ]);
        let token = exchange_code("google token exchange", request).await?;
        let id_token = token
            .id_token
            .filter(|t| !t.is_empty())
            .ok_or_else(|| AuthError::invalid("google token exchange: no id_token"))?;
        drop(token.access_token);
        self.verify_id_token(&id_token, nonce).await
    }

    /// Verify an `id_token`: RS256 signature by a key in the JWKS (refetched once when the
    /// `kid` is unknown), `iss`, `aud` = client id, `exp` (with [`LEEWAY`]), `nonce`, and
    /// `email_verified == true`. Token defects are `Invalid`; an unverified email is `Denied`.
    pub async fn verify_id_token(
        &self,
        id_token: &str,
        nonce: &str,
    ) -> Result<ExternalIdentity, AuthError> {
        let header = decode_header(id_token)
            .map_err(|e| AuthError::invalid(format!("id_token header: {e}")))?;
        if header.alg != Algorithm::RS256 {
            return Err(AuthError::invalid(format!("id_token alg {:?} not allowed", header.alg)));
        }
        let kid = header
            .kid
            .ok_or_else(|| AuthError::invalid("id_token without kid"))?;
        let key = self.decoding_key(&kid).await?;

        let mut validation = Validation::new(Algorithm::RS256);
        validation.leeway = LEEWAY.as_secs();
        validation.set_audience(&[self.config.client_id.as_str()]);
        validation.set_issuer(&self.config.issuers);
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        let claims = decode::<IdTokenClaims>(id_token, &key, &validation)
            .map_err(|e| AuthError::invalid(format!("id_token: {e}")))?
            .claims;

        if nonce.is_empty() || claims.nonce.as_deref() != Some(nonce) {
            return Err(AuthError::invalid("id_token nonce mismatch"));
        }
        if claims.sub.is_empty() {
            return Err(AuthError::invalid("id_token without subject"));
        }
        let verified = claims.email_verified.as_ref().is_some_and(Flag::is_true);
        let email = match claims.email {
            Some(email) if verified && !email.is_empty() => email,
            _ => return Err(AuthError::Denied),
        };
        Ok(ExternalIdentity {
            provider: Provider::Google,
            subject: claims.sub,
            login: email.clone(),
            name: claims.name.filter(|n| !n.is_empty()),
            email: Some(email),
            avatar_url: claims.picture.filter(|p| !p.is_empty()),
        })
    }

    async fn decoding_key(&self, kid: &str) -> Result<DecodingKey, AuthError> {
        let mut cache = self.jwks.lock().await;
        let fresh = cache
            .as_ref()
            .filter(|cached| cached.fetched_at.elapsed() < self.config.jwks_max_age);
        if let Some(jwk) = fresh.and_then(|cached| cached.set.find(kid)) {
            return key_from_jwk(jwk);
        }
        let set = self.fetch_jwks().await?;
        let key = set.find(kid).map(key_from_jwk);
        *cache = Some(CachedJwks { set, fetched_at: Instant::now() });
        key.unwrap_or_else(|| Err(AuthError::invalid("id_token signed by an unknown key")))
    }

    async fn fetch_jwks(&self) -> Result<JwkSet, AuthError> {
        let what = "google jwks";
        let response = self
            .http
            .get(&self.config.jwks_url)
            .header(ACCEPT, "application/json")
            .send()
            .await
            .map_err(|e| transport(what, e))?;
        let status = response.status();
        if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS {
            return Err(AuthError::unavailable(format!("{what}: status {status}")));
        }
        if !status.is_success() {
            return Err(AuthError::invalid(format!("{what}: unexpected status {status}")));
        }
        read_json(what, response).await
    }
}

fn key_from_jwk(jwk: &Jwk) -> Result<DecodingKey, AuthError> {
    DecodingKey::from_jwk(jwk).map_err(|e| AuthError::invalid(format!("google jwk: {e}")))
}
