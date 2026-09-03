//! Verification tokens: `hex(HMAC-SHA256(secret, project \0 hostname \0 nonce))`.

use hmac::digest::KeyInit;
use hmac::{Hmac, Mac};
use sha2::Sha256;

/// The verification token for one binding. Deterministic in its inputs; the per-binding `nonce`
/// makes every new binding of the same hostname (even by the same project) need a fresh TXT.
pub fn token(secret: &[u8; 32], project: &str, hostname: &str, nonce: &str) -> String {
    // HMAC pads keys shorter than the hash block (64 bytes) with zeros, so this is exactly
    // HMAC-SHA256 keyed with the 32-byte secret, built without a fallible constructor.
    let mut key = [0u8; 64];
    key[..32].copy_from_slice(secret);
    let mut mac = <Hmac<Sha256> as KeyInit>::new(&key.into());
    mac.update(project.as_bytes());
    mac.update(&[0]);
    mac.update(hostname.as_bytes());
    mac.update(&[0]);
    mac.update(nonce.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// A fresh random nonce for a new binding (122 random bits, hex).
pub(crate) fn nonce() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_hmac_sha256_of_nul_separated_fields() {
        let secret = [7u8; 32];
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&secret).unwrap();
        mac.update(b"acme-shop\0shop.acme.com\0n1");
        let want = hex::encode(mac.finalize().into_bytes());
        assert_eq!(token(&secret, "acme-shop", "shop.acme.com", "n1"), want);
        assert_eq!(want.len(), 64);
    }

    #[test]
    fn token_is_deterministic_and_unique_per_input() {
        let s = [1u8; 32];
        let base = token(&s, "p", "a.example.com", "n");
        assert_eq!(base, token(&s, "p", "a.example.com", "n"));
        let variants = [
            token(&[2u8; 32], "p", "a.example.com", "n"),
            token(&s, "q", "a.example.com", "n"),
            token(&s, "p", "b.example.com", "n"),
            token(&s, "p", "a.example.com", "m"),
            // Field boundaries matter: moving bytes between fields changes the token.
            token(&s, "pa", ".example.com", "n"),
        ];
        for v in &variants {
            assert_ne!(&base, v);
        }
    }

    #[test]
    fn nonces_differ() {
        assert_ne!(nonce(), nonce());
        assert_eq!(nonce().len(), 32);
    }
}
