//! Opaque server-side UI sessions and the CSRF token (server.md "Sessions", "CSRF").
//!
//! The cookie carries 256 random bits; the store keeps only their SHA-256 ([`SessionId`]).
//! Lifetime: idle 12 h (sliding, `last_seen` written at most every 5 min), absolute 7 d.
//! Lookup per request, no cache, so revocation is effective on the next request.
//!
//! CSRF token: `base64url(HMAC-SHA256(auth/csrf_key, id_hash ‖ csrf_secret))` where
//! `csrf_secret` is the session row's random per-session value; nothing else is stored and the
//! UI keeps the token in memory.

use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use parking_lot::Mutex;
use rand::RngCore;
use rand::rngs::OsRng;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

use mage_core::domain::UserId;
use mage_core::ports::Clock;

use crate::error::AppError;
use crate::ports::tokens::{NewSession, TokenStore};
use crate::ports::vault::{SecretBytes, SecretName, SecretScope, SecretVault, VaultError, constant_time_eq, names};

/// Session cookie name; `__Host-` pins it to the exact origin (Secure, Path=/, no Domain).
pub const SESSION_COOKIE: &str = "__Host-mage_sid";
/// Pre-login cookie holding the OAuth `state` between the redirect and the callback.
pub const OAUTH_COOKIE: &str = "__Host-mage_oauth";
/// Header carrying the CSRF token on mutations.
pub const CSRF_HEADER: &str = "x-csrf-token";

/// Plaintext secrets are cached at most this long per instance (server.md "Plaintext caches").
const CSRF_KEY_CACHE: Duration = Duration::from_secs(60);

/// SHA-256 of a session cookie value: the only form the server keeps.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionId(pub [u8; 32]);

impl SessionId {
    pub fn of_cookie(value: &str) -> SessionId {
        SessionId(Sha256::digest(value.as_bytes()).into())
    }
}

impl fmt::Debug for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SessionId({}…)", hex::encode(&self.0[..4]))
    }
}

/// Session lifetime (`[server.session]`, seconds).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SessionPolicy {
    pub idle_secs: u64,
    pub absolute_secs: u64,
    /// `last_seen` / idle expiry are written at most this often.
    pub touch_secs: u64,
}

impl Default for SessionPolicy {
    fn default() -> Self {
        SessionPolicy { idle_secs: 12 * 3600, absolute_secs: 7 * 24 * 3600, touch_secs: 5 * 60 }
    }
}

impl SessionPolicy {
    fn idle(&self) -> time::Duration {
        time::Duration::seconds(i64::try_from(self.idle_secs).unwrap_or(i64::MAX / 4))
    }

    fn absolute(&self) -> time::Duration {
        time::Duration::seconds(i64::try_from(self.absolute_secs).unwrap_or(i64::MAX / 4))
    }

    fn touch(&self) -> time::Duration {
        time::Duration::seconds(i64::try_from(self.touch_secs).unwrap_or(i64::MAX / 4))
    }
}

/// An authenticated session (per request).
#[derive(Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub id: SessionId,
    pub user_id: UserId,
    pub created_at: OffsetDateTime,
    pub expires_at: OffsetDateTime,
    csrf_secret: Vec<u8>,
}

impl fmt::Debug for SessionInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionInfo").field("id", &self.id).field("user_id", &self.user_id).field("expires_at", &self.expires_at).finish()
    }
}

/// A new session's cookie value (shown to the browser once).
#[derive(Clone, PartialEq, Eq)]
pub struct SessionCookie {
    pub value: String,
    pub id: SessionId,
    /// Absolute expiry: the cookie's `Max-Age`.
    pub expires_at: OffsetDateTime,
}

impl fmt::Debug for SessionCookie {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionCookie").field("id", &self.id).field("expires_at", &self.expires_at).finish_non_exhaustive()
    }
}

pub struct Sessions {
    tokens: Arc<dyn TokenStore>,
    vault: Arc<dyn SecretVault>,
    clock: Arc<dyn Clock>,
    policy: SessionPolicy,
    csrf_key: Mutex<Option<(Instant, Arc<SecretBytes>)>>,
}

impl Sessions {
    pub fn new(tokens: Arc<dyn TokenStore>, vault: Arc<dyn SecretVault>, clock: Arc<dyn Clock>, policy: SessionPolicy) -> Sessions {
        Sessions { tokens, vault, clock, policy, csrf_key: Mutex::new(None) }
    }

    pub fn policy(&self) -> &SessionPolicy {
        &self.policy
    }

    /// New session for `user` (a fresh id on every login: rotation).
    pub async fn create(&self, user: UserId, ip: Option<IpAddr>, user_agent: Option<String>) -> Result<SessionCookie, AppError> {
        let value = random_b64(32);
        let id = SessionId::of_cookie(&value);
        let now = self.clock.now();
        let idle_expiry = (now + self.policy.idle()).min(now + self.policy.absolute());
        let csrf_secret = random_bytes::<16>().to_vec();
        let user_agent = user_agent.map(|ua| ua.chars().take(512).collect());
        self.tokens.create_session(&NewSession { id_hash: id.0, user_id: user, csrf_secret, expires_at: idle_expiry, ip, user_agent }).await?;
        Ok(SessionCookie { value, id, expires_at: now + self.policy.absolute() })
    }

    /// The live session behind a cookie value, sliding its idle expiry. `None`: unknown,
    /// expired (idle or absolute) or revoked.
    pub async fn authenticate(&self, cookie_value: &str) -> Result<Option<SessionInfo>, AppError> {
        if !looks_like_session_value(cookie_value) {
            return Ok(None);
        }
        let id = SessionId::of_cookie(cookie_value);
        let Some(session) = self.tokens.session(&id.0).await? else {
            return Ok(None);
        };
        let now = self.clock.now();
        let absolute_end = session.created_at + self.policy.absolute();
        if session.expires_at <= now || absolute_end <= now {
            if let Err(e) = self.tokens.delete_session(&id.0).await {
                tracing::warn!(error = %e, "deleting an expired session failed");
            }
            return Ok(None);
        }
        let mut expires_at = session.expires_at;
        if now - session.last_seen_at >= self.policy.touch() {
            expires_at = (now + self.policy.idle()).min(absolute_end);
            self.tokens.touch_session(&id.0, expires_at).await?;
        }
        Ok(Some(SessionInfo { id, user_id: session.user_id, created_at: session.created_at, expires_at, csrf_secret: session.csrf_secret }))
    }

    /// Whether the session still exists (SSE re-check every 60 s).
    pub async fn is_active(&self, id: SessionId) -> Result<bool, AppError> {
        let now = self.clock.now();
        Ok(self
            .tokens
            .session(&id.0)
            .await?
            .is_some_and(|s| s.expires_at > now && s.created_at + self.policy.absolute() > now))
    }

    pub async fn end(&self, id: SessionId) -> Result<(), AppError> {
        Ok(self.tokens.delete_session(&id.0).await?)
    }

    pub async fn end_all(&self, user: UserId) -> Result<u64, AppError> {
        Ok(self.tokens.delete_user_sessions(user).await?)
    }

    /// The session's CSRF token (`GET /api/v1/session`).
    pub async fn csrf_token(&self, session: &SessionInfo) -> Result<String, AppError> {
        let key = self.csrf_key().await?;
        Ok(URL_SAFE_NO_PAD.encode(csrf_mac(key.expose(), session)))
    }

    /// Constant-time check of a presented `X-CSRF-Token`.
    pub async fn verify_csrf(&self, session: &SessionInfo, presented: &str) -> Result<bool, AppError> {
        let Ok(presented) = URL_SAFE_NO_PAD.decode(presented.trim()) else {
            return Ok(false);
        };
        let key = self.csrf_key().await?;
        Ok(constant_time_eq(&csrf_mac(key.expose(), session), &presented))
    }

    async fn csrf_key(&self) -> Result<Arc<SecretBytes>, AppError> {
        if let Some((loaded, key)) = self.csrf_key.lock().as_ref() {
            if loaded.elapsed() < CSRF_KEY_CACHE {
                return Ok(key.clone());
            }
        }
        let name = SecretName::new(names::CSRF_KEY)?;
        let (_, key) = self.vault.load(&SecretScope::Platform, &name).await.map_err(|e| match e {
            VaultError::NotFound => AppError::Unavailable("platform secret auth/csrf_key is not provisioned".into()),
            other => other.into(),
        })?;
        let key = Arc::new(key);
        *self.csrf_key.lock() = Some((Instant::now(), key.clone()));
        Ok(key)
    }
}

fn csrf_mac(key: &[u8], session: &SessionInfo) -> Vec<u8> {
    // HMAC accepts keys of any length; `new_from_slice` cannot fail for HMAC.
    let Ok(mut mac) = <Hmac<Sha256> as Mac>::new_from_slice(key) else {
        return Vec::new();
    };
    mac.update(&session.id.0);
    mac.update(&session.csrf_secret);
    mac.finalize().into_bytes().to_vec()
}

/// 32 random bytes in base64url: 43 characters of `[A-Za-z0-9_-]`.
fn looks_like_session_value(value: &str) -> bool {
    value.len() == 43 && value.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub(crate) fn random_bytes<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    OsRng.fill_bytes(&mut out);
    out
}

fn random_b64(len: usize) -> String {
    let mut bytes = vec![0u8; len];
    OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// SHA-256 of an opaque one-time value (OAuth state, install state).
pub(crate) fn sha256(value: &str) -> [u8; 32] {
    Sha256::digest(value.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use mage_core::testing::FakeClock;

    use super::*;
    use crate::ports::audit::Actor;
    use crate::ports::vault::Activation;
    use crate::testing::{MemSecretVault, MemTokenStore};

    struct Fixture {
        clock: Arc<FakeClock>,
        sessions: Sessions,
        vault: Arc<MemSecretVault>,
    }

    async fn fixture() -> Fixture {
        let clock = Arc::new(FakeClock::new());
        let tokens = Arc::new(MemTokenStore::new(clock.clone()));
        let vault = Arc::new(MemSecretVault::new(clock.clone()));
        let name = SecretName::new(names::CSRF_KEY).expect("name");
        vault.put(&SecretScope::Platform, &name, SecretBytes::new(vec![42; 32]), Activation::Now, &Actor::System).await.expect("csrf key");
        let sessions = Sessions::new(tokens, vault.clone(), clock.clone(), SessionPolicy::default());
        Fixture { clock, sessions, vault }
    }

    fn user() -> UserId {
        UserId::from(uuid::Uuid::from_u128(7))
    }

    #[tokio::test]
    async fn idle_sessions_expire_after_twelve_hours_without_activity() {
        let f = fixture().await;
        let cookie = f.sessions.create(user(), None, None).await.expect("create");
        assert!(f.sessions.authenticate(&cookie.value).await.expect("auth").is_some());
        f.clock.advance(Duration::from_secs(12 * 3600 + 1));
        assert!(f.sessions.authenticate(&cookie.value).await.expect("auth").is_none());
    }

    #[tokio::test]
    async fn activity_slides_the_idle_expiry_but_not_past_seven_days() {
        let f = fixture().await;
        let cookie = f.sessions.create(user(), None, None).await.expect("create");
        // A request every 6 h keeps the session alive...
        for _ in 0..27 {
            f.clock.advance(Duration::from_secs(6 * 3600));
            assert!(f.sessions.authenticate(&cookie.value).await.expect("auth").is_some());
        }
        // ...until the absolute 7-day limit (28 × 6 h = 7 d).
        f.clock.advance(Duration::from_secs(6 * 3600));
        assert!(f.sessions.authenticate(&cookie.value).await.expect("auth").is_none());
    }

    #[tokio::test]
    async fn ended_sessions_and_garbage_cookies_do_not_authenticate() {
        let f = fixture().await;
        let a = f.sessions.create(user(), None, None).await.expect("create");
        let b = f.sessions.create(user(), None, None).await.expect("create");
        assert_ne!(a.value, b.value, "every login gets a fresh id");
        f.sessions.end(a.id).await.expect("end");
        assert!(f.sessions.authenticate(&a.value).await.expect("auth").is_none());
        assert!(f.sessions.authenticate(&b.value).await.expect("auth").is_some());
        assert_eq!(f.sessions.end_all(user()).await.expect("end all"), 1);
        assert!(f.sessions.authenticate(&b.value).await.expect("auth").is_none());
        assert!(f.sessions.authenticate("not-a-session").await.expect("auth").is_none());
    }

    #[tokio::test]
    async fn csrf_tokens_are_per_session_and_follow_the_key() {
        let f = fixture().await;
        let a = f.sessions.authenticate(&f.sessions.create(user(), None, None).await.expect("create").value).await.expect("auth").expect("live");
        let b = f.sessions.authenticate(&f.sessions.create(user(), None, None).await.expect("create").value).await.expect("auth").expect("live");
        let token_a = f.sessions.csrf_token(&a).await.expect("token");
        assert!(f.sessions.verify_csrf(&a, &token_a).await.expect("verify"));
        assert!(!f.sessions.verify_csrf(&b, &token_a).await.expect("verify"), "another session's token");
        assert!(!f.sessions.verify_csrf(&a, "").await.expect("verify"));
        assert!(!f.sessions.verify_csrf(&a, "!!!").await.expect("verify"));
        // A rotated key invalidates old tokens once the 60 s cache lapses.
        let name = SecretName::new(names::CSRF_KEY).expect("name");
        f.vault.put(&SecretScope::Platform, &name, SecretBytes::new(vec![43; 32]), Activation::Now, &Actor::System).await.expect("rotate");
        *f.sessions.csrf_key.lock() = None;
        assert!(!f.sessions.verify_csrf(&a, &token_a).await.expect("verify"));
    }
}
