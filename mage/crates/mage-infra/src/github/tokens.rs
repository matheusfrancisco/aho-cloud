//! App JWT signing and the installation-token cache (intake.md "App JWT", "Installation tokens").

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use jsonwebtoken::{Algorithm, EncodingKey, Header};
use mage_core::domain::ids::InstallationId;
use mage_core::ports::source_host::{AccessToken, InstallationToken, MIN_TOKEN_VALIDITY, SourceHostError, TokenScope};
use parking_lot::Mutex;
use serde::Serialize;
use time::OffsetDateTime;

use super::AppCredentials;
use super::wire::access_to_github;

/// `iat` is backdated by this much against clock drift (GitHub's recommendation).
const JWT_BACKDATE: Duration = Duration::from_secs(60);
/// `exp - now`; GitHub rejects more than 10 minutes.
const JWT_LIFETIME: Duration = Duration::from_secs(9 * 60);
/// A signed JWT is reused this long after it was signed.
const JWT_REUSE: Duration = Duration::from_secs(8 * 60);

#[derive(Serialize)]
struct Claims<'a> {
    iat: i64,
    exp: i64,
    iss: &'a str,
}

/// Signs and caches the app JWT (RS256, `iss` = client id).
pub(crate) struct AppSigner {
    client_id: String,
    key: EncodingKey,
    cached: Mutex<Option<(AccessToken, OffsetDateTime)>>,
}

impl AppSigner {
    /// Fails with `Invalid` when the key is not a usable RSA PEM (checked by signing once).
    pub fn new(app: &AppCredentials, now: OffsetDateTime) -> Result<Self, SourceHostError> {
        let key = EncodingKey::from_rsa_pem(app.private_key_pem.as_bytes())
            .map_err(|e| SourceHostError::Invalid(format!("GitHub App private key: {e}")))?;
        let signer = Self { client_id: app.client_id.clone(), key, cached: Mutex::new(None) };
        signer.sign(now)?;
        Ok(signer)
    }

    pub fn token(&self, now: OffsetDateTime) -> Result<AccessToken, SourceHostError> {
        let mut cached = self.cached.lock();
        if let Some((token, signed_at)) = cached.as_ref()
            && *signed_at <= now
            && now < *signed_at + JWT_REUSE
        {
            return Ok(token.clone());
        }
        let token = self.sign(now)?;
        *cached = Some((token.clone(), now));
        Ok(token)
    }

    fn sign(&self, now: OffsetDateTime) -> Result<AccessToken, SourceHostError> {
        let claims = Claims {
            iat: (now - JWT_BACKDATE).unix_timestamp(),
            exp: (now + JWT_LIFETIME).unix_timestamp(),
            iss: &self.client_id,
        };
        jsonwebtoken::encode(&Header::new(Algorithm::RS256), &claims, &self.key)
            .map(AccessToken::new)
            .map_err(|e| SourceHostError::Invalid(format!("GitHub App private key cannot sign: {e}")))
    }
}

/// Canonical form of a scope: repository order and duplicates do not matter; `None` (every repo)
/// differs from an empty list.
pub(crate) fn scope_key(scope: &TokenScope) -> String {
    let repos = match &scope.repositories {
        None => "*".to_owned(),
        Some(names) => {
            let mut names: Vec<&str> = names.iter().map(String::as_str).collect();
            names.sort_unstable();
            names.dedup();
            format!("[{}]", names.join(","))
        }
    };
    let permissions: Vec<String> = scope.permissions.iter().map(|(name, access)| format!("{name}:{}", access_to_github(*access))).collect();
    format!("repos={repos};permissions={}", permissions.join(","))
}

/// A cached token may be handed out only with at least [`MIN_TOKEN_VALIDITY`] left.
pub(crate) fn is_fresh(token: &InstallationToken, now: OffsetDateTime) -> bool {
    token.expires_at >= now + MIN_TOKEN_VALIDITY
}

/// One async mutex per (installation, scope): holding it while minting makes a burst mint once.
pub(crate) type Slot = Arc<tokio::sync::Mutex<Option<InstallationToken>>>;

#[derive(Default)]
pub(crate) struct TokenCache {
    slots: Mutex<HashMap<(InstallationId, String), Slot>>,
}

impl TokenCache {
    /// The slot of `(inst, key)`, created on first use. Creating one also drops idle slots whose
    /// token has expired (or was never minted), so the map tracks live scopes only.
    pub fn slot(&self, inst: InstallationId, key: &str, now: OffsetDateTime) -> Slot {
        let mut slots = self.slots.lock();
        if let Some(slot) = slots.get(&(inst, key.to_owned())) {
            return slot.clone();
        }
        // Only slots nobody holds (the map's reference is the last) can go: a caller between
        // `slot()` and `lock()` keeps sharing its slot with later callers.
        slots.retain(|_, slot| {
            Arc::strong_count(slot) > 1 || slot.try_lock().map_or(true, |token| token.as_ref().is_some_and(|t| t.expires_at > now))
        });
        let slot = Slot::default();
        slots.insert((inst, key.to_owned()), slot.clone());
        slot
    }

    pub fn evict_installation(&self, inst: InstallationId) {
        self.slots.lock().retain(|(slot_inst, _), _| *slot_inst != inst);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use mage_core::ports::source_host::Access;

    use super::*;

    const NOW: OffsetDateTime = time::macros::datetime!(2026-01-01 00:00:00 UTC);

    #[test]
    fn scope_key_is_order_insensitive_and_distinguishes_scopes() {
        let scope = |repos: Option<&[&str]>, perms: &[(&str, Access)]| TokenScope {
            repositories: repos.map(|r| r.iter().map(|s| (*s).to_owned()).collect()),
            permissions: perms.iter().map(|(n, a)| ((*n).to_owned(), *a)).collect::<BTreeMap<_, _>>(),
        };
        let a = scope(Some(&["web", "api"]), &[("contents", Access::Read)]);
        let b = scope(Some(&["api", "web", "api"]), &[("contents", Access::Read)]);
        assert_eq!(scope_key(&a), scope_key(&b));
        assert_eq!(scope_key(&a), "repos=[api,web];permissions=contents:read");
        assert_ne!(scope_key(&a), scope_key(&scope(Some(&["web", "api"]), &[("contents", Access::Write)])));
        assert_ne!(scope_key(&scope(None, &[])), scope_key(&scope(Some(&[]), &[])));
        assert_eq!(scope_key(&TokenScope::default()), "repos=*;permissions=");
    }

    #[test]
    fn freshness_boundary_is_inclusive() {
        let token = |left: Duration| InstallationToken {
            token: AccessToken::new("t"),
            expires_at: NOW + left,
            permissions: BTreeMap::new(),
            repositories: None,
        };
        assert!(is_fresh(&token(MIN_TOKEN_VALIDITY), NOW));
        assert!(!is_fresh(&token(MIN_TOKEN_VALIDITY - Duration::from_secs(1)), NOW));
    }

    #[test]
    fn bad_keys_are_invalid() {
        let app = AppCredentials { client_id: "Iv1".into(), private_key_pem: "not a key".into() };
        assert!(matches!(AppSigner::new(&app, NOW).err(), Some(SourceHostError::Invalid(_))));
    }

    #[test]
    fn jwt_is_reused_for_eight_minutes() {
        let app = AppCredentials { client_id: "Iv1".into(), private_key_pem: include_str!("../../tests/fixtures/github_app_key.pem").into() };
        let Ok(signer) = AppSigner::new(&app, NOW) else { panic!("fixture key") };
        let first = signer.token(NOW).map(|t| t.expose().to_owned());
        assert_eq!(signer.token(NOW + Duration::from_secs(479)).map(|t| t.expose().to_owned()), first);
        let later = signer.token(NOW + JWT_REUSE).map(|t| t.expose().to_owned());
        assert!(later.is_ok() && later != first);
    }

    #[test]
    fn idle_expired_slots_are_pruned_on_insert() {
        let cache = TokenCache::default();
        let slot = cache.slot(InstallationId(1), "a", NOW);
        // Never filled: pruned when another key is created.
        drop(slot);
        let _b = cache.slot(InstallationId(1), "b", NOW);
        assert_eq!(cache.slots.lock().len(), 1);
        cache.evict_installation(InstallationId(1));
        assert!(cache.slots.lock().is_empty());
    }
}
