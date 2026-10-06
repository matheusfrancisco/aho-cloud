//! Service authentication of the internal API (knowledge.md "Text search, blame, and diff on
//! Cloud Run"; token format in server.md, codec `mage_core::wire::token`): the server signs a
//! short-lived `mgi_` token per request; the indexer holds no signing key. Public keys come from
//! the server's `/.well-known/mage/keys.json`, cached and refreshed every 5 min and on an unknown
//! `kid` (at most once per minute). Replayed `jti`s are rejected with an in-memory set covering
//! each token's validity window.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use mage_core::domain::{OrgId, RepoId};
use mage_core::ports::Clock;
use mage_core::wire::token::{JwkSet, KEYS_PATH, KeyRing, LEEWAY, TokenCodec, TokenError, TokenKind, TokenScope};
use serde::Deserialize;
use time::OffsetDateTime;
use url::Url;
use uuid::Uuid;

/// Cached keys are refetched after this long.
pub const KEY_REFRESH: time::Duration = time::Duration::minutes(5);
/// An unknown `kid` triggers a refetch at most this often.
pub const UNKNOWN_KID_REFRESH: time::Duration = time::Duration::minutes(1);
/// `exp - iat` ceiling of an `mgi_` token.
pub const MAX_TOKEN_LIFETIME: time::Duration = time::Duration::seconds(60);
/// HTTP timeout of a key fetch.
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// `[indexer.internal_auth]`: whose tokens the internal API accepts.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct InternalAuthConfig {
    /// The server's `public_url` (token `iss`), no trailing slash.
    pub issuer: String,
    /// JWK set URL; default `{issuer}/.well-known/mage/keys.json`.
    #[serde(default)]
    pub keys_url: Option<Url>,
}

impl InternalAuthConfig {
    pub fn keys_url(&self) -> Result<Url, url::ParseError> {
        match &self.keys_url {
            Some(url) => Ok(url.clone()),
            None => Url::parse(&format!("{}{KEYS_PATH}", self.issuer.trim_end_matches('/'))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeySourceError {
    #[error("key source unavailable: {0}")]
    Unavailable(String),
    #[error("invalid key set: {0}")]
    Invalid(String),
}

/// Where the verifying keys come from.
#[async_trait]
pub trait KeySource: Send + Sync {
    async fn fetch(&self) -> Result<JwkSet, KeySourceError>;
}

/// `GET {keys_url}` with `reqwest`.
pub struct HttpKeySource {
    client: reqwest::Client,
    url: Url,
}

impl HttpKeySource {
    pub fn new(url: Url) -> Result<Self, KeySourceError> {
        let client = reqwest::Client::builder().timeout(FETCH_TIMEOUT).build().map_err(|e| KeySourceError::Unavailable(e.to_string()))?;
        Ok(Self { client, url })
    }
}

#[async_trait]
impl KeySource for HttpKeySource {
    async fn fetch(&self) -> Result<JwkSet, KeySourceError> {
        let response = self.client.get(self.url.clone()).send().await.map_err(|e| KeySourceError::Unavailable(e.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(KeySourceError::Unavailable(format!("{} answered {status}", self.url)));
        }
        response.json::<JwkSet>().await.map_err(|e| KeySourceError::Invalid(e.to_string()))
    }
}

/// The verified caller: the token's org and repo claims.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexerCaller {
    pub org: OrgId,
    pub repo: RepoId,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    #[error("missing bearer token")]
    Missing,
    #[error("{0}")]
    Token(TokenError),
    #[error("token lifetime above 60 s")]
    LifetimeTooLong,
    #[error("token replayed")]
    Replayed,
    /// No key set could be fetched yet: the indexer cannot verify anything.
    #[error("signing keys unavailable: {0}")]
    KeysUnavailable(String),
}

struct Keys {
    ring: KeyRing,
    /// When the cached ring is due for a refetch; `None` until a fetch succeeded.
    refresh_after: Option<OffsetDateTime>,
    unknown_refetched_at: Option<OffsetDateTime>,
}

/// Verifies `mgi_` tokens for one issuer.
pub struct ServiceAuth {
    codec: TokenCodec,
    source: Arc<dyn KeySource>,
    clock: Arc<dyn Clock>,
    keys: tokio::sync::Mutex<Keys>,
    /// `jti` → `exp` of accepted tokens still inside their validity window.
    seen: Mutex<HashMap<Uuid, OffsetDateTime>>,
}

impl ServiceAuth {
    pub fn new(issuer: impl Into<String>, source: Arc<dyn KeySource>, clock: Arc<dyn Clock>) -> Self {
        Self {
            codec: TokenCodec::new(issuer),
            source,
            clock,
            keys: tokio::sync::Mutex::new(Keys { ring: KeyRing::new(), refresh_after: None, unknown_refetched_at: None }),
            seen: Mutex::new(HashMap::new()),
        }
    }

    /// Fetch failures keep the previous ring (retried after [`UNKNOWN_KID_REFRESH`], so a down
    /// server does not cost every request a fetch); only an indexer that never got keys fails
    /// closed.
    async fn refresh(&self, keys: &mut Keys, now: OffsetDateTime) -> Result<(), AuthError> {
        let fetched = self.source.fetch().await.map_err(|e| e.to_string()).and_then(|set| KeyRing::from_jwks(&set).map_err(|e| e.to_string()));
        match fetched {
            Ok(ring) => {
                keys.ring = ring;
                keys.refresh_after = Some(now + KEY_REFRESH);
                Ok(())
            }
            Err(err) if keys.refresh_after.is_some() => {
                tracing::warn!(error = %err, "refreshing token keys failed; keeping the cached keys");
                keys.refresh_after = Some(now + UNKNOWN_KID_REFRESH);
                Ok(())
            }
            Err(err) => Err(AuthError::KeysUnavailable(err)),
        }
    }

    /// Checks an `mgi_` token: signature, kind, issuer, audience, time, lifetime ≤ 60 s, single
    /// use. Returns its org/repo claims.
    pub async fn verify(&self, token: &str) -> Result<IndexerCaller, AuthError> {
        let now = self.clock.now();
        let claims = {
            let mut keys = self.keys.lock().await;
            if keys.refresh_after.is_none_or(|due| now >= due) {
                self.refresh(&mut keys, now).await?;
            }
            match self.codec.verify(&keys.ring, token, TokenKind::Indexer, now) {
                Err(TokenError::UnknownKey) if keys.unknown_refetched_at.is_none_or(|at| now - at >= UNKNOWN_KID_REFRESH) => {
                    keys.unknown_refetched_at = Some(now);
                    self.refresh(&mut keys, now).await?;
                    self.codec.verify(&keys.ring, token, TokenKind::Indexer, now)
                }
                other => other,
            }
            .map_err(AuthError::Token)?
        };
        if claims.exp - claims.iat > MAX_TOKEN_LIFETIME {
            return Err(AuthError::LifetimeTooLong);
        }
        let TokenScope::Indexer { org, repo } = claims.scope else {
            return Err(AuthError::Token(TokenError::WrongKind));
        };
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        seen.retain(|_, exp| *exp + LEEWAY > now);
        if seen.insert(claims.jti, claims.exp).is_some() {
            return Err(AuthError::Replayed);
        }
        Ok(IndexerCaller { org, repo })
    }
}
