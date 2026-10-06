//! `SecretVault` port (server.md "`SecretVault` (owned)", "Envelope design"): envelope encryption
//! plus versioned storage of platform and tenant secrets. The adapter persists through
//! `SecretStore` (storage.md) and never opens its own database connection.

use std::fmt;

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

use mage_core::domain::OrgId;

use crate::error::AppError;
use crate::ports::audit::Actor;

/// Who a secret belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SecretScope {
    Platform,
    Org(OrgId),
}

impl SecretScope {
    pub fn org(&self) -> Option<OrgId> {
        match self {
            SecretScope::Platform => None,
            SecretScope::Org(org) => Some(*org),
        }
    }
}

/// `platform` or `org:{uuid}`: the scope's spelling inside AADs and audit details.
impl fmt::Display for SecretScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SecretScope::Platform => f.write_str("platform"),
            SecretScope::Org(org) => write!(f, "org:{}", org.as_uuid()),
        }
    }
}

/// `"<area>/<name>"`, `[a-z0-9_./-]{1,128}`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SecretName(String);

impl SecretName {
    pub const MAX_LEN: usize = 128;

    pub fn new(name: impl Into<String>) -> Result<SecretName, VaultError> {
        let name = name.into();
        let charset_ok = name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'/' | b'-'));
        if name.is_empty() || name.len() > Self::MAX_LEN || !charset_ok {
            return Err(VaultError::Invalid(format!("secret name must match [a-z0-9_./-]{{1,128}}: {name:?}")));
        }
        Ok(SecretName(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SecretName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Names of the secrets the platform part of the server reads (server.md "Secret inventory").
pub mod names {
    /// Ed25519 key ring (32-byte seeds), `Platform`.
    pub const TOKEN_SIGNING_KEY: &str = "auth/token_signing_key";
    /// CSRF HMAC key, `Platform`.
    pub const CSRF_KEY: &str = "auth/csrf_key";
    /// Login code exchange, `Platform`.
    pub const GITHUB_OAUTH_CLIENT_SECRET: &str = "github/oauth_client_secret";
    /// Google login code exchange, `Platform`.
    pub const GOOGLE_OAUTH_CLIENT_SECRET: &str = "google/oauth_client_secret";
    /// BYOK Anthropic key, `Org`.
    pub const MODEL_ANTHROPIC_API_KEY: &str = "model/anthropic_api_key";
}

/// 1-based version of a secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SecretVersion(pub u32);

impl fmt::Display for SecretVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Lifecycle of one secret version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SecretState {
    Staged,
    Active,
    Retired,
    Destroyed,
}

impl SecretState {
    /// `secrets.state` value.
    pub const fn as_str(self) -> &'static str {
        match self {
            SecretState::Staged => "staged",
            SecretState::Active => "active",
            SecretState::Retired => "retired",
            SecretState::Destroyed => "destroyed",
        }
    }

    pub fn parse(s: &str) -> Option<SecretState> {
        [SecretState::Staged, SecretState::Active, SecretState::Retired, SecretState::Destroyed].into_iter().find(|v| v.as_str() == s)
    }

    /// Staged→Active, Active→Retired, Retired→Destroyed, Staged→Destroyed, and the emergency
    /// Active→Destroyed (server.md key rotation diagram).
    pub const fn can_become(self, next: SecretState) -> bool {
        matches!(
            (self, next),
            (SecretState::Staged, SecretState::Active)
                | (SecretState::Active, SecretState::Retired)
                | (SecretState::Retired, SecretState::Destroyed)
                | (SecretState::Staged, SecretState::Destroyed)
                | (SecretState::Active, SecretState::Destroyed)
        )
    }
}

impl fmt::Display for SecretState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How a new version enters the lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Activation {
    /// Becomes Active; the previous Active becomes Retired in the same transaction.
    Now,
    /// Becomes Staged (signing-key rotation).
    Staged,
}

/// Plaintext secret. No `Debug`/`Display`/`Serialize`; zeroed on drop with volatile writes.
pub struct SecretBytes(Vec<u8>);

impl SecretBytes {
    pub fn new(bytes: Vec<u8>) -> SecretBytes {
        SecretBytes(bytes)
    }

    pub fn expose(&self) -> &[u8] {
        &self.0
    }

    /// The value as UTF-8 (API keys, tokens), if it is.
    pub fn expose_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.0).ok()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Display-safe fingerprint: `sk-ant-…a1b2` for API keys with a recognizable prefix
    /// (`sk-ant-`, `lin_api_`, `xoxb-`, `ghs_`), else `sha256:` + 12 hex digits.
    pub fn fingerprint(&self) -> String {
        const PREFIXES: [&str; 6] = ["sk-ant-", "lin_api_", "lin_oauth_", "xoxb-", "xoxp-", "ghs_"];
        if let Some(text) = self.expose_str() {
            if let Some(prefix) = PREFIXES.iter().find(|p| text.starts_with(**p)) {
                let tail: String = text.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
                if text.len() >= prefix.len() + 12 {
                    return format!("{prefix}…{tail}");
                }
            }
        }
        format!("sha256:{}", &hex::encode(Sha256::digest(&self.0))[..12])
    }
}

impl From<Vec<u8>> for SecretBytes {
    fn from(bytes: Vec<u8>) -> Self {
        SecretBytes(bytes)
    }
}

impl From<String> for SecretBytes {
    fn from(text: String) -> Self {
        SecretBytes(text.into_bytes())
    }
}

/// A fresh copy (which zeroes itself on drop too).
impl Clone for SecretBytes {
    fn clone(&self) -> Self {
        SecretBytes(self.0.clone())
    }
}

impl PartialEq for SecretBytes {
    /// Constant-time in the length of the shorter value.
    fn eq(&self, other: &Self) -> bool {
        constant_time_eq(&self.0, &other.0)
    }
}

impl Eq for SecretBytes {}

impl Drop for SecretBytes {
    fn drop(&mut self) {
        zero(&mut self.0);
    }
}

/// Overwrites `bytes` with zeros through volatile writes the optimizer cannot elide.
pub fn zero(bytes: &mut [u8]) {
    for b in bytes.iter_mut() {
        // SAFETY: `b` is a valid, aligned, exclusive reference into the slice.
        unsafe { std::ptr::write_volatile(b, 0) };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}

/// Equality without early exit on the first differing byte.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Tenant data sealed with an org data key: `[fmt=1][dek_version u32 BE][nonce 12][ct+tag]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ciphertext(Vec<u8>);

impl Ciphertext {
    pub const FORMAT_V1: u8 = 1;
    /// `fmt` + `dek_version` + nonce.
    pub const HEADER_LEN: usize = 1 + 4 + 12;

    pub fn from_bytes(bytes: Vec<u8>) -> Ciphertext {
        Ciphertext(bytes)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }

    /// `(dek_version, nonce, ct+tag)` of a well-formed v1 ciphertext.
    pub fn parts(&self) -> Result<(u32, [u8; 12], &[u8]), VaultError> {
        let bytes = &self.0;
        if bytes.len() < Self::HEADER_LEN + 16 || bytes[0] != Self::FORMAT_V1 {
            return Err(VaultError::Invalid("malformed ciphertext".into()));
        }
        let version = u32::from_be_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]);
        let mut nonce = [0u8; 12];
        nonce.copy_from_slice(&bytes[5..17]);
        Ok((version, nonce, &bytes[Self::HEADER_LEN..]))
    }

    pub fn assemble(dek_version: u32, nonce: &[u8; 12], sealed: &[u8]) -> Ciphertext {
        let mut out = Vec::with_capacity(Self::HEADER_LEN + sealed.len());
        out.push(Self::FORMAT_V1);
        out.extend_from_slice(&dek_version.to_be_bytes());
        out.extend_from_slice(nonce);
        out.extend_from_slice(sealed);
        Ciphertext(out)
    }
}

/// Display-safe metadata of one secret version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretMeta {
    pub scope: SecretScope,
    pub name: SecretName,
    pub version: SecretVersion,
    pub state: SecretState,
    /// e.g. `sk-ant-…a1b2` or `sha256:` prefix.
    pub fingerprint: String,
    pub created_by: Actor,
    pub created_at: OffsetDateTime,
    pub state_changed_at: OffsetDateTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RewrapReport {
    pub scanned: u32,
    pub rewrapped: u32,
    pub remaining: u32,
}

#[async_trait]
pub trait SecretVault: Send + Sync {
    /// New version. `Now`: becomes Active, previous Active becomes Retired (one transaction).
    async fn put(
        &self,
        scope: &SecretScope,
        name: &SecretName,
        value: SecretBytes,
        activation: Activation,
        actor: &Actor,
    ) -> Result<SecretMeta, VaultError>;
    /// Active version.
    async fn load(&self, scope: &SecretScope, name: &SecretName) -> Result<(SecretMeta, SecretBytes), VaultError>;
    /// Staged, Active or Retired version (signing-key verification needs non-active versions).
    async fn load_version(&self, scope: &SecretScope, name: &SecretName, version: SecretVersion) -> Result<SecretBytes, VaultError>;
    async fn versions(&self, scope: &SecretScope, name: &SecretName) -> Result<Vec<SecretMeta>, VaultError>;
    async fn list(&self, scope: &SecretScope) -> Result<Vec<SecretMeta>, VaultError>;
    /// Allowed: Staged→Active (old Active→Retired), Active→Retired, Retired→Destroyed,
    /// Staged→Destroyed (and emergency Active→Destroyed). Destroyed deletes the wrapped DEK and
    /// ciphertext (crypto-shred).
    async fn set_state(
        &self,
        scope: &SecretScope,
        name: &SecretName,
        version: SecretVersion,
        state: SecretState,
        actor: &Actor,
    ) -> Result<SecretMeta, VaultError>;
    /// Encrypt tenant data with the org's active data key; AAD binds the ciphertext to its use.
    async fn encrypt(&self, org: OrgId, aad: &[u8], plaintext: &[u8]) -> Result<Ciphertext, VaultError>;
    async fn decrypt(&self, org: OrgId, aad: &[u8], ciphertext: &Ciphertext) -> Result<SecretBytes, VaultError>;
    /// Re-wrap DEKs whose KEK version is not the KEK's primary version.
    async fn rewrap(&self, batch: u32) -> Result<RewrapReport, VaultError>;
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VaultError {
    #[error("secret not found")]
    NotFound,
    #[error("secret conflict")]
    Conflict,
    #[error("invalid secret request: {0}")]
    Invalid(String),
    #[error("vault unavailable: {0}")]
    Unavailable(String),
    /// AEAD tag or AAD mismatch: tampered or moved row.
    #[error("secret integrity check failed")]
    Integrity,
    /// KEK permission denied.
    #[error("key encryption key access denied")]
    Forbidden,
}

impl From<VaultError> for AppError {
    fn from(e: VaultError) -> Self {
        match e {
            VaultError::NotFound => AppError::NotFound("secret".into()),
            VaultError::Conflict => AppError::Conflict("secret version".into()),
            VaultError::Invalid(m) => AppError::Invalid(m),
            VaultError::Unavailable(m) => AppError::Unavailable(m),
            // Never a client problem: a tampered row or a KEK permission is an operator incident.
            VaultError::Integrity => AppError::Internal("secret integrity check failed".into()),
            VaultError::Forbidden => AppError::Unavailable("key encryption key access denied".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_names_follow_the_charset() {
        assert!(SecretName::new("model/anthropic_api_key").is_ok());
        assert!(SecretName::new("integration/linear/ws-1.a/oauth").is_ok());
        for bad in ["", "Upper/case", "space name", "x".repeat(129).as_str(), "emoji/😀"] {
            assert!(SecretName::new(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn state_transitions_match_the_lifecycle() {
        use SecretState::*;
        assert!(Staged.can_become(Active) && Active.can_become(Retired) && Retired.can_become(Destroyed));
        assert!(Staged.can_become(Destroyed) && Active.can_become(Destroyed));
        for (from, to) in [(Retired, Active), (Destroyed, Active), (Destroyed, Retired), (Active, Staged), (Retired, Staged), (Active, Active)] {
            assert!(!from.can_become(to), "{from} -> {to}");
        }
    }

    #[test]
    fn fingerprints_never_contain_the_secret_body() {
        let key = SecretBytes::from("sk-ant-api03-abcdefghijklmnopqrstuvwxyz-a1b2".to_string());
        assert_eq!(key.fingerprint(), "sk-ant-…a1b2");
        let opaque = SecretBytes::new(vec![7; 32]);
        let fp = opaque.fingerprint();
        assert!(fp.starts_with("sha256:") && fp.len() == "sha256:".len() + 12);
        // Too short to show a tail without revealing most of the key.
        assert!(SecretBytes::from("sk-ant-short".to_string()).fingerprint().starts_with("sha256:"));
    }

    #[test]
    fn ciphertext_round_trips_its_header() {
        let ct = Ciphertext::assemble(7, &[9; 12], &[1; 20]);
        let (version, nonce, sealed) = ct.parts().expect("well formed");
        assert_eq!((version, nonce, sealed.len()), (7, [9; 12], 20));
        assert!(Ciphertext::from_bytes(vec![2; 40]).parts().is_err());
        assert!(Ciphertext::from_bytes(vec![1; 20]).parts().is_err());
    }
}
