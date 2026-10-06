//! `TokenSigner` and `IdentityProvider` ports (server.md "`TokenSigner`, `IdentityProvider`,
//! `ChangeFeed`, `AuditLog` (owned, Additions)").
//!
//! Token kinds, scopes, claims and the runner assertion are the codec types of
//! `mage_core::wire::token`, re-exported here so the port and the codec cannot drift.

use std::fmt;

use async_trait::async_trait;
use time::OffsetDateTime;

pub use mage_core::wire::token::{MemoryCutoff, RunnerAssertion, TokenClaims, TokenKind, TokenScope};

use crate::error::AppError;
use crate::ports::vault::SecretState;

/// `"mg?_" + compact JWS`; charset `[A-Za-z0-9_.-]`. `Debug` shows the kind prefix only.
#[derive(Clone, PartialEq, Eq)]
pub struct SignedToken(String);

impl SignedToken {
    pub fn new(token: String) -> SignedToken {
        SignedToken(token)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Debug for SignedToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let prefix = self.0.get(..4).unwrap_or("");
        write!(f, "SignedToken({prefix}…)")
    }
}

/// One verifying key of the ring (for `/.well-known/mage/keys.json`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicKeyInfo {
    pub kid: String,
    /// Staged, Active or Retired (Destroyed keys are not published).
    pub state: SecretState,
    pub public_key: [u8; 32],
}

#[async_trait]
pub trait TokenSigner: Send + Sync {
    /// Signs with the single Active key; `Unavailable` when the ring has none.
    fn sign(&self, claims: &TokenClaims) -> Result<SignedToken, TokenError>;
    /// Stateless checks only (format, alg, kid, signature, iss/aud/typ, time).
    /// Revocation and run state are checked by the calling use case.
    fn verify(&self, token: &str, expected: TokenKind, now: OffsetDateTime) -> Result<TokenClaims, TokenError>;
    /// Runner-signed assertion verified against the runner's registered public key.
    fn verify_assertion(&self, assertion: &str, runner_public_key: &[u8; 32], now: OffsetDateTime) -> Result<RunnerAssertion, TokenError>;
    /// Reload the key ring from SecretVault.
    async fn reload(&self) -> Result<(), TokenError>;
    /// Staged, Active and Retired public keys (for /.well-known/mage/keys.json).
    fn public_keys(&self) -> Vec<PublicKeyInfo>;
}

/// Why a token was refused, or why the signer could not act. The rejection variants mirror
/// `mage_core::wire::token::TokenError` one to one (metrics label: [`TokenError::reason`]).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TokenError {
    #[error("malformed token")]
    Malformed,
    #[error("wrong token kind")]
    WrongKind,
    #[error("unsupported algorithm")]
    UnsupportedAlgorithm,
    #[error("unknown signing key")]
    UnknownKey,
    #[error("bad signature")]
    BadSignature,
    #[error("wrong issuer")]
    WrongIssuer,
    #[error("wrong audience")]
    WrongAudience,
    #[error("token expired")]
    Expired,
    #[error("token not yet valid")]
    NotYetValid,
    #[error("token lifetime too long")]
    LifetimeTooLong,
    /// Key ring not loaded, no Active key, or the vault failed.
    #[error("token signer unavailable: {0}")]
    Unavailable(String),
}

impl TokenError {
    /// Stable label for verification-failure metrics and logs.
    pub const fn reason(&self) -> &'static str {
        match self {
            TokenError::Malformed => "malformed",
            TokenError::WrongKind => "wrong_kind",
            TokenError::UnsupportedAlgorithm => "unsupported_alg",
            TokenError::UnknownKey => "unknown_key",
            TokenError::BadSignature => "bad_signature",
            TokenError::WrongIssuer => "wrong_issuer",
            TokenError::WrongAudience => "wrong_audience",
            TokenError::Expired => "expired",
            TokenError::NotYetValid => "not_yet_valid",
            TokenError::LifetimeTooLong => "lifetime_too_long",
            TokenError::Unavailable(_) => "unavailable",
        }
    }
}

impl From<mage_core::wire::token::TokenError> for TokenError {
    fn from(e: mage_core::wire::token::TokenError) -> Self {
        use mage_core::wire::token::TokenError as W;
        match e {
            W::Malformed => TokenError::Malformed,
            W::WrongKind => TokenError::WrongKind,
            W::UnsupportedAlgorithm => TokenError::UnsupportedAlgorithm,
            W::UnknownKey => TokenError::UnknownKey,
            W::BadSignature => TokenError::BadSignature,
            W::WrongIssuer => TokenError::WrongIssuer,
            W::WrongAudience => TokenError::WrongAudience,
            W::Expired => TokenError::Expired,
            W::NotYetValid => TokenError::NotYetValid,
            W::LifetimeTooLong => TokenError::LifetimeTooLong,
        }
    }
}

/// Every rejection is a 401 (no detail leaks to the caller); only `Unavailable` is a 503.
impl From<TokenError> for AppError {
    fn from(e: TokenError) -> Self {
        match e {
            TokenError::Unavailable(m) => AppError::Unavailable(m),
            _ => AppError::Unauthorized,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------------------------

/// Login provider of an identity; `(provider, subject)` is the identity key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IdentityProviderKind {
    GitHub,
    Google,
    AhoCloudEdge,
}

impl IdentityProviderKind {
    /// `user_identities.provider` / `invites.provider` value.
    pub const fn as_str(self) -> &'static str {
        match self {
            IdentityProviderKind::GitHub => "github",
            IdentityProviderKind::Google => "google",
            IdentityProviderKind::AhoCloudEdge => "aho_cloud_edge",
        }
    }

    pub fn parse(s: &str) -> Option<IdentityProviderKind> {
        [IdentityProviderKind::GitHub, IdentityProviderKind::Google, IdentityProviderKind::AhoCloudEdge]
            .into_iter()
            .find(|k| k.as_str() == s)
    }
}

impl fmt::Display for IdentityProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Inputs of an authorize redirect; the caller keeps `state` and the PKCE verifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizeParams {
    pub state: String,
    pub pkce_challenge: String,
    pub redirect_uri: String,
}

/// A provider-verified identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalIdentity {
    pub provider: IdentityProviderKind,
    /// GitHub numeric user id / Google `sub` / aho-cloud principal id (stable).
    pub subject: String,
    pub login: String,
    pub name: Option<String>,
    /// Primary verified email, if readable.
    pub email: Option<String>,
    pub avatar_url: Option<String>,
}

#[async_trait]
pub trait IdentityProvider: Send + Sync {
    fn kind(&self) -> IdentityProviderKind;
    fn authorize_url(&self, params: &AuthorizeParams) -> String;
    /// Exchange code (+ PKCE verifier) and fetch identity; the provider token is dropped here.
    async fn exchange(&self, code: &str, pkce_verifier: &str, redirect_uri: &str) -> Result<ExternalIdentity, IdentityError>;
    /// Resolve a login to a stable subject (invites). `Ok(None)`: no such user.
    async fn lookup_login(&self, login: &str) -> Result<Option<ExternalIdentity>, IdentityError>;
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdentityError {
    #[error("login denied")]
    Denied,
    #[error("invalid login: {0}")]
    Invalid(String),
    #[error("identity provider unavailable: {0}")]
    Unavailable(String),
}

impl From<shared_auth::AuthError> for IdentityError {
    fn from(e: shared_auth::AuthError) -> Self {
        match e {
            shared_auth::AuthError::Denied => IdentityError::Denied,
            shared_auth::AuthError::Invalid(m) => IdentityError::Invalid(m),
            shared_auth::AuthError::Unavailable(m) => IdentityError::Unavailable(m),
        }
    }
}

impl From<IdentityError> for AppError {
    fn from(e: IdentityError) -> Self {
        match e {
            IdentityError::Denied => AppError::Forbidden("login denied by the identity provider".into()),
            IdentityError::Invalid(m) => AppError::Invalid(m),
            IdentityError::Unavailable(m) => AppError::Unavailable(m),
        }
    }
}
