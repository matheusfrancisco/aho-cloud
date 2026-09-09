//! Edge sessions, pending OIDC logins and magic-link tokens: bounded in-memory maps keyed by
//! the BLAKE3 digest of a random secret, so the secret itself is never stored or compared.

use std::collections::HashMap;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use cloud_api::internal::Site;
use cloud_contract::ledger::Stage;
use cloud_contract::principal::{PrincipalClaims, TTL_SECS, audience, issuer};
use parking_lot::{Mutex, MutexGuard};
use rand_core::{OsRng, RngCore};

/// Who a session (or bearer token) authenticated as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Identity {
    /// `<idp>:<upstream subject>`.
    pub sub: String,
    pub email: Option<String>,
    pub email_verified: bool,
    pub groups: Vec<String>,
    pub amr: Vec<String>,
}

/// A logged-in browser on one hostname of one (project, stage).
#[derive(Clone, Debug)]
pub(crate) struct Session {
    /// Public id (the principal's `sid`); unrelated to the cookie secret.
    pub id: String,
    pub host: String,
    pub project: String,
    pub stage: Stage,
    pub identity: Identity,
}

impl Session {
    /// Whether this session may be used on `site` served at `host`.
    pub fn belongs_to(&self, site: &Site, host: &str) -> bool {
        self.host == host && self.project == site.project && self.stage == site.stage
    }
}

/// An OIDC authorization request in flight, keyed by `state`.
#[derive(Clone, Debug)]
pub(crate) struct PendingLogin {
    pub host: String,
    pub project: String,
    pub stage: Stage,
    pub nonce: String,
    pub verifier: String,
    pub rd: String,
}

/// An unused magic link.
#[derive(Clone, Debug)]
pub(crate) struct MagicToken {
    pub host: String,
    pub project: String,
    pub stage: Stage,
    pub email: String,
    pub rd: String,
}

/// The map is at capacity even after dropping expired entries.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Full;

/// Expiring map keyed by BLAKE3(secret).
pub(crate) struct TtlMap<V> {
    entries: Mutex<HashMap<[u8; 32], (u64, V)>>,
    cap: usize,
}

impl<V: Clone> TtlMap<V> {
    pub fn new(cap: usize) -> Self {
        Self { entries: Mutex::new(HashMap::new()), cap }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<[u8; 32], (u64, V)>> {
        self.entries.lock()
    }

    /// Stores `value` until `expires_at` (unix seconds).
    pub fn insert(&self, secret: &str, value: V, expires_at: u64, now: u64) -> Result<(), Full> {
        let mut map = self.lock();
        if map.len() >= self.cap {
            map.retain(|_, (exp, _)| *exp > now);
            if map.len() >= self.cap {
                return Err(Full);
            }
        }
        map.insert(digest(secret), (expires_at, value));
        Ok(())
    }

    /// The live value for `secret`.
    pub fn get(&self, secret: &str, now: u64) -> Option<V> {
        let key = digest(secret);
        let mut map = self.lock();
        match map.get(&key) {
            Some((exp, v)) if *exp > now => Some(v.clone()),
            Some(_) => {
                map.remove(&key);
                None
            }
            None => None,
        }
    }

    /// Removes and returns the live value: single use.
    pub fn take(&self, secret: &str, now: u64) -> Option<V> {
        match self.lock().remove(&digest(secret)) {
            Some((exp, v)) if exp > now => Some(v),
            _ => None,
        }
    }

    /// Drops `secret` whether or not it is live.
    pub fn remove(&self, secret: &str) -> bool {
        self.lock().remove(&digest(secret)).is_some()
    }
}

fn digest(secret: &str) -> [u8; 32] {
    *blake3::hash(secret.as_bytes()).as_bytes()
}

/// 256 random bits, base64url.
pub(crate) fn random_token() -> String {
    let mut buf = [0u8; 32];
    OsRng.fill_bytes(&mut buf);
    URL_SAFE_NO_PAD.encode(buf)
}

/// Preview/staging reviewers: with a non-empty `allowed_emails`, only a verified email on the
/// list passes (identities without one, such as the shared password, do not).
pub(crate) fn allowed(site: &Site, identity: &Identity) -> bool {
    if site.stage == Stage::Live || site.allowed_emails.is_empty() {
        return true;
    }
    match (&identity.email, identity.email_verified) {
        (Some(email), true) => site.allowed_emails.iter().any(|a| a.trim().eq_ignore_ascii_case(email)),
        _ => false,
    }
}

/// The principal the app receives for `identity` on `site`, valid for [`TTL_SECS`].
pub(crate) fn claims(site: &Site, identity: &Identity, sid: &str, now: u64) -> PrincipalClaims {
    PrincipalClaims {
        iss: issuer(&site.project),
        aud: audience(&site.project, site.stage),
        sub: identity.sub.clone(),
        email: identity.email.clone(),
        email_verified: identity.email_verified,
        groups: identity.groups.clone(),
        amr: identity.amr.clone(),
        sid: sid.to_owned(),
        iat: now,
        exp: now + TTL_SECS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cloud_api::internal::SiteState;
    use cloud_contract::edge::EdgeConfig;
    use cloud_contract::manifest::Auth;

    fn site(stage: Stage, allowed: &[&str]) -> Site {
        Site {
            hostname: "a.test".into(),
            project: "a".into(),
            stage,
            state: SiteState::Active,
            upstreams: vec![],
            edge: EdgeConfig::default(),
            auth: Auth::Password,
            password_hash: None,
            oidc_client_secret: None,
            verifier_secrets: Default::default(),
            service_token_hashes: vec![],
            allowed_emails: allowed.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn who(email: Option<&str>, verified: bool) -> Identity {
        Identity { sub: "x:1".into(), email: email.map(str::to_owned), email_verified: verified, groups: vec![], amr: vec!["otp".into()] }
    }

    #[test]
    fn ttl_map_expires_and_take_is_single_use() {
        let m = TtlMap::new(10);
        m.insert("s", 1u8, 100, 0).unwrap();
        assert_eq!(m.get("s", 99), Some(1));
        assert_eq!(m.get("s", 100), None, "expiry is exclusive");
        m.insert("t", 2u8, 100, 0).unwrap();
        assert_eq!(m.take("t", 50), Some(2));
        assert_eq!(m.take("t", 50), None);
        m.insert("u", 3u8, 100, 0).unwrap();
        assert_eq!(m.take("u", 100), None, "expired take still consumes");
        assert_eq!(m.get("u", 0), None);
    }

    #[test]
    fn ttl_map_capacity_reclaims_expired_then_refuses() {
        let m = TtlMap::new(2);
        m.insert("a", (), 10, 0).unwrap();
        m.insert("b", (), 20, 0).unwrap();
        assert_eq!(m.insert("c", (), 30, 5), Err(Full));
        m.insert("c", (), 30, 15).unwrap();
        assert_eq!(m.get("a", 15), None);
        assert!(m.get("b", 15).is_some() && m.get("c", 15).is_some());
    }

    #[test]
    fn allowed_emails_apply_to_preview_and_staging_only() {
        let s = site(Stage::Preview, &["Rev@Acme.com"]);
        assert!(allowed(&s, &who(Some("rev@acme.com"), true)));
        assert!(!allowed(&s, &who(Some("rev@acme.com"), false)), "unverified email is not an invitation");
        assert!(!allowed(&s, &who(Some("other@acme.com"), true)));
        assert!(!allowed(&s, &who(None, false)), "no email, no entry");
        assert!(allowed(&site(Stage::Live, &["rev@acme.com"]), &who(Some("other@acme.com"), true)));
        assert!(allowed(&site(Stage::Staging, &[]), &who(None, false)));
    }

    #[test]
    fn claims_pass_the_contract_check() {
        let s = site(Stage::Staging, &[]);
        let c = claims(&s, &who(Some("a@b.c"), true), "sid1", 1000);
        assert_eq!(c.check("a", Stage::Staging, 1000), Ok(()));
        assert_eq!(c.exp - c.iat, TTL_SECS);
    }
}
