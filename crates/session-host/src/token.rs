//! Workspace-scoped rpc tokens.
//!
//! `POST /internal/v1/workspaces` mints one; `GET /rpc/{workspace}?token=` checks it before the
//! WebSocket upgrade. A token is `base64url(claims) "." base64url(HMAC-SHA256(key, DOMAIN ‖
//! base64url(claims)))`, no padding, where `claims` is the JSON `{"w": workspace, "u": user,
//! "exp": unix seconds}`. Tokens are stateless and short-lived; only this service holds the key.

use std::fmt;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::Sha256;

/// Domain separation: a MAC made for anything else with the same key never verifies here.
const DOMAIN: &[u8] = b"aho-cloud/session-host/rpc-token/v1\0";

/// Shortest accepted signing secret, in bytes.
pub const MIN_KEY_BYTES: usize = 32;

/// The HMAC key tokens are signed with. `Debug` never prints the secret.
#[derive(Clone)]
pub struct TokenKey(Hmac<Sha256>);

impl TokenKey {
    /// Keys shorter than [`MIN_KEY_BYTES`] are refused.
    pub fn new(secret: &[u8]) -> Result<Self, TokenError> {
        if secret.len() < MIN_KEY_BYTES {
            return Err(TokenError::WeakKey { len: secret.len() });
        }
        Hmac::<Sha256>::new_from_slice(secret).map(Self).map_err(|_| TokenError::WeakKey { len: secret.len() })
    }

    fn tag(&self, payload_b64: &str) -> Hmac<Sha256> {
        let mut mac = self.0.clone();
        mac.update(DOMAIN);
        mac.update(payload_b64.as_bytes());
        mac
    }
}

impl fmt::Debug for TokenKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TokenKey(..)")
    }
}

/// What a token grants: `user` may open rpc connections to `workspace` until `exp`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Claims {
    #[serde(rename = "w")]
    pub workspace: String,
    #[serde(rename = "u")]
    pub user: String,
    /// Unix seconds; the token is valid while `now < exp`.
    pub exp: u64,
}

/// Why a token was refused (or a key rejected).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TokenError {
    #[error("token signing key is {len} bytes; at least {MIN_KEY_BYTES} are required")]
    WeakKey { len: usize },
    #[error("token is malformed")]
    Malformed,
    #[error("token signature does not verify")]
    BadSignature,
    #[error("token expired at {exp} (now {now})")]
    Expired { exp: u64, now: u64 },
    #[error("token was issued for another workspace")]
    WrongWorkspace,
}

/// Signs `claims`.
pub fn mint(key: &TokenKey, claims: &Claims) -> String {
    let json = serde_json::json!({ "w": claims.workspace, "u": claims.user, "exp": claims.exp });
    let payload = URL_SAFE_NO_PAD.encode(json.to_string());
    let tag = URL_SAFE_NO_PAD.encode(key.tag(&payload).finalize().into_bytes());
    format!("{payload}.{tag}")
}

/// Checks the signature (constant time) first, then expiry at `now`, then that the token was
/// issued for `workspace`.
pub fn verify(key: &TokenKey, token: &str, workspace: &str, now: u64) -> Result<Claims, TokenError> {
    let (payload, tag) = token.split_once('.').ok_or(TokenError::Malformed)?;
    let tag = URL_SAFE_NO_PAD.decode(tag).map_err(|_| TokenError::Malformed)?;
    key.tag(payload).verify_slice(&tag).map_err(|_| TokenError::BadSignature)?;
    let json = URL_SAFE_NO_PAD.decode(payload).map_err(|_| TokenError::Malformed)?;
    let claims: Claims = serde_json::from_slice(&json).map_err(|_| TokenError::Malformed)?;
    if now >= claims.exp {
        return Err(TokenError::Expired { exp: claims.exp, now });
    }
    if claims.workspace != workspace {
        return Err(TokenError::WrongWorkspace);
    }
    Ok(claims)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";

    fn key() -> TokenKey {
        TokenKey::new(SECRET).unwrap()
    }

    fn claims(exp: u64) -> Claims {
        Claims { workspace: "shop-0011223344556677".into(), user: "u_42".into(), exp }
    }

    #[test]
    fn round_trip_returns_the_claims_until_expiry() {
        let t = mint(&key(), &claims(1_000));
        assert_eq!(verify(&key(), &t, "shop-0011223344556677", 999), Ok(claims(1_000)));
        assert_eq!(verify(&key(), &t, "shop-0011223344556677", 1_000), Err(TokenError::Expired { exp: 1_000, now: 1_000 }));
        assert_eq!(verify(&key(), &t, "shop-0011223344556677", 5_000), Err(TokenError::Expired { exp: 1_000, now: 5_000 }));
    }

    #[test]
    fn a_token_only_opens_its_own_workspace() {
        let t = mint(&key(), &claims(1_000));
        assert_eq!(verify(&key(), &t, "shop-8899aabbccddeeff", 1), Err(TokenError::WrongWorkspace));
    }

    #[test]
    fn tampering_with_the_claims_breaks_the_signature() {
        let t = mint(&key(), &claims(1_000));
        let (_, tag) = t.split_once('.').unwrap();
        // Same MAC, claims rewritten to extend the expiry and switch the user.
        let forged = URL_SAFE_NO_PAD.encode(r#"{"exp":99999999999,"u":"admin","w":"shop-0011223344556677"}"#);
        assert_eq!(verify(&key(), &format!("{forged}.{tag}"), "shop-0011223344556677", 1), Err(TokenError::BadSignature));
        // A flipped character anywhere in the signature is also refused.
        let mut bytes = t.clone().into_bytes();
        let last = bytes.len() - 2;
        bytes[last] = if bytes[last] == b'A' { b'B' } else { b'A' };
        let flipped = String::from_utf8(bytes).unwrap();
        assert_eq!(verify(&key(), &flipped, "shop-0011223344556677", 1), Err(TokenError::BadSignature));
    }

    #[test]
    fn another_key_does_not_verify() {
        let other = TokenKey::new(b"ffffffffffffffffffffffffffffffff").unwrap();
        let t = mint(&other, &claims(1_000));
        assert_eq!(verify(&key(), &t, "shop-0011223344556677", 1), Err(TokenError::BadSignature));
    }

    #[test]
    fn garbage_is_malformed_not_a_panic() {
        for t in ["", "nodot", "a.b.c", "!!!.???", ".", "abc."] {
            assert!(matches!(verify(&key(), t, "w", 1), Err(TokenError::Malformed | TokenError::BadSignature)), "{t:?}");
        }
        // Validly signed but not our claims shape.
        let payload = URL_SAFE_NO_PAD.encode("[1,2]");
        let tag = URL_SAFE_NO_PAD.encode(key().tag(&payload).finalize().into_bytes());
        assert_eq!(verify(&key(), &format!("{payload}.{tag}"), "w", 1), Err(TokenError::Malformed));
    }

    #[test]
    fn short_keys_are_refused() {
        assert_eq!(TokenKey::new(&SECRET[..31]).err(), Some(TokenError::WeakKey { len: 31 }));
        assert_eq!(format!("{:?}", key()), "TokenKey(..)");
    }
}
