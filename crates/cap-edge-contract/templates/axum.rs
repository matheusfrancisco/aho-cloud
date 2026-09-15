//! aho cloud principal verifier for axum, for project {{PROJECT}} ({{STAGE}}).
//! The edge authenticates users and forwards a signed principal; this extractor only verifies it.
//! Authorize in your handlers by `(principal.iss, principal.sub)`. Never add a login flow here.
//!
//! Cargo.toml: axum = "0.8", jsonwebtoken = "9", reqwest = { version = "0.12", features = ["json"] },
//! serde = { version = "1", features = ["derive"] }, tokio = { version = "1", features = ["sync"] }

use std::sync::Arc;

use axum::extract::{FromRef, FromRequestParts, OptionalFromRequestParts};
use axum::http::StatusCode;
use axum::http::request::Parts;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::Deserialize;
use tokio::sync::RwLock;

pub const ISSUER: &str = "{{ISSUER}}";
pub const AUDIENCE: &str = "{{AUDIENCE}}";
pub const JWKS_URL: &str = "{{JWKS_URL}}";
pub const HEADER: &str = "{{HEADER}}";
const MAX_LIFETIME_SECS: u64 = {{TTL}};
const LEEWAY_SECS: u64 = {{LEEWAY}};

/// The verified principal.
#[derive(Clone, Debug, Deserialize)]
pub struct Principal {
    pub iss: String,
    pub sub: String,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub email_verified: bool,
    #[serde(default)]
    pub groups: Vec<String>,
    pub amr: Vec<String>,
    pub sid: String,
    pub iat: u64,
    pub exp: u64,
}

/// The project's JWKS: fetched on first use, and again when a token names a key it lacks.
#[derive(Clone, Default)]
pub struct Jwks(Arc<RwLock<Option<JwkSet>>>);

impl Jwks {
    async fn find(&self, kid: &str) -> Option<DecodingKey> {
        let set = self.0.read().await;
        set.as_ref()?.find(kid).and_then(|jwk| DecodingKey::from_jwk(jwk).ok())
    }

    async fn key(&self, kid: &str) -> Option<DecodingKey> {
        if let Some(k) = self.find(kid).await {
            return Some(k);
        }
        let fresh: JwkSet = reqwest::get(JWKS_URL).await.ok()?.error_for_status().ok()?.json().await.ok()?;
        *self.0.write().await = Some(fresh);
        self.find(kid).await
    }

    /// Signature and every claim; `None` for anything invalid.
    pub async fn verify(&self, token: &str) -> Option<Principal> {
        let kid = decode_header(token).ok()?.kid?;
        let key = self.key(&kid).await?;
        let mut v = Validation::new(Algorithm::ES256);
        v.set_issuer(&[ISSUER]);
        v.set_audience(&[AUDIENCE]);
        v.set_required_spec_claims(&["iss", "aud", "sub", "exp"]);
        v.leeway = LEEWAY_SECS;
        let p = decode::<Principal>(token, &key, &v).ok()?.claims;
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
        (p.iat <= now + LEEWAY_SECS && p.exp.saturating_sub(p.iat) <= MAX_LIFETIME_SECS).then_some(p)
    }
}

/// Routes in `edge` mode: 401 without a valid principal.
impl<S> FromRequestParts<S> for Principal
where
    Jwks: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = StatusCode;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let token = parts.headers.get(HEADER).and_then(|v| v.to_str().ok()).ok_or(StatusCode::UNAUTHORIZED)?;
        Jwks::from_ref(state).verify(token).await.ok_or(StatusCode::UNAUTHORIZED)
    }
}

/// Routes in `optional` mode: `Option<Principal>` is `None` when nobody is signed in.
impl<S> OptionalFromRequestParts<S> for Principal
where
    Jwks: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = StatusCode;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Option<Self>, Self::Rejection> {
        if parts.headers.get(HEADER).is_none() {
            return Ok(None);
        }
        <Principal as FromRequestParts<S>>::from_request_parts(parts, state).await.map(Some)
    }
}

// let app = Router::new().route("/me", get(|p: Principal| async move { p.sub })).with_state(Jwks::default());
