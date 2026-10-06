//! PKCE (RFC 7636, S256 only) and random `state` / `nonce` values.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore;
use rand::rngs::OsRng;
use sha2::{Digest, Sha256};

/// Bytes of entropy in generated verifiers, states and nonces (256 bit).
const RANDOM_BYTES: usize = 32;

/// RFC 7636 §4.1 verifier length bounds.
const VERIFIER_MIN: usize = 43;
const VERIFIER_MAX: usize = 128;

/// The `code_challenge_method` this crate sends.
pub const CHALLENGE_METHOD: &str = "S256";

/// A PKCE verifier and its S256 challenge. Keep `verifier` server-side; send `challenge`.
#[derive(Clone, PartialEq, Eq)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    /// A fresh 256-bit verifier (43 base64url characters) and its S256 challenge.
    pub fn generate() -> Self {
        let verifier = random_token();
        let challenge = challenge_s256(&verifier);
        Self { verifier, challenge }
    }
}

impl std::fmt::Debug for Pkce {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pkce")
            .field("verifier", &"<redacted>")
            .field("challenge", &self.challenge)
            .finish()
    }
}

/// `BASE64URL(SHA256(ASCII(verifier)))` without padding (RFC 7636 §4.2).
pub fn challenge_s256(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// RFC 7636 §4.1: 43..=128 characters from `[A-Z] / [a-z] / [0-9] / "-" / "." / "_" / "~"`.
pub fn is_valid_verifier(verifier: &str) -> bool {
    (VERIFIER_MIN..=VERIFIER_MAX).contains(&verifier.len())
        && verifier
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
}

/// A 256-bit random OAuth `state`, base64url without padding.
pub fn random_state() -> String {
    random_token()
}

/// A 256-bit random OIDC `nonce`, base64url without padding.
pub fn random_nonce() -> String {
    random_token()
}

/// An OIDC `nonce` derived from a PKCE verifier, for callers that persist only the verifier
/// between the authorize redirect and the callback. The verifier is 256-bit secret and one-time,
/// so the derived nonce is as unpredictable and as single-use as a random one; it is sent to the
/// provider before the verifier is.
pub fn nonce_for_verifier(verifier: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"shared-auth/oidc-nonce\0");
    hasher.update(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(hasher.finalize())
}

fn random_token() -> String {
    let mut bytes = [0u8; RANDOM_BYTES];
    OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge_matches_rfc7636_appendix_b() {
        assert_eq!(
            challenge_s256("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn generated_pair_is_valid_and_consistent() {
        let pkce = Pkce::generate();
        assert_eq!(pkce.verifier.len(), 43);
        assert!(is_valid_verifier(&pkce.verifier));
        assert_eq!(pkce.challenge, challenge_s256(&pkce.verifier));
        assert_ne!(pkce.verifier, Pkce::generate().verifier);
    }

    #[test]
    fn verifier_validation_enforces_length_and_alphabet() {
        assert!(!is_valid_verifier(&"a".repeat(42)));
        assert!(is_valid_verifier(&"a".repeat(43)));
        assert!(is_valid_verifier(&"a".repeat(128)));
        assert!(!is_valid_verifier(&"a".repeat(129)));
        assert!(is_valid_verifier(&format!("{}-._~", "A0".repeat(20))));
        assert!(!is_valid_verifier(&format!("{}+", "a".repeat(43))));
        assert!(!is_valid_verifier(&format!("{}=", "a".repeat(43))));
        assert!(!is_valid_verifier(&format!("{}é", "a".repeat(43))));
    }

    #[test]
    fn states_and_nonces_are_unique_url_safe_256_bit() {
        let a = random_state();
        let b = random_state();
        assert_ne!(a, b);
        assert_eq!(URL_SAFE_NO_PAD.decode(&a).map(|v| v.len()), Ok(32));
        let n = random_nonce();
        assert_ne!(n, random_nonce());
        assert_eq!(URL_SAFE_NO_PAD.decode(&n).map(|v| v.len()), Ok(32));
    }

    #[test]
    fn derived_nonce_is_deterministic_and_distinct_from_challenge() {
        let pkce = Pkce::generate();
        assert_eq!(nonce_for_verifier(&pkce.verifier), nonce_for_verifier(&pkce.verifier));
        assert_ne!(nonce_for_verifier(&pkce.verifier), pkce.challenge);
        assert_ne!(
            nonce_for_verifier(&pkce.verifier),
            nonce_for_verifier(&Pkce::generate().verifier)
        );
    }

    #[test]
    fn debug_does_not_leak_verifier() {
        let pkce = Pkce::generate();
        assert!(!format!("{pkce:?}").contains(&pkce.verifier));
    }
}
