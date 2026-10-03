//! Small cryptographic helpers: random tokens, token hashing, constant-time comparison, and the
//! secret box (ChaCha20-Poly1305 at rest, HMAC-SHA256 fingerprints).

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use cloud_contract::ledger::Stage;
use rand::RngCore;
use rand::rngs::OsRng;
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::config::MasterKey;

/// `n` bytes from the OS CSPRNG.
pub fn random_bytes(n: usize) -> Vec<u8> {
    let mut out = vec![0u8; n];
    OsRng.fill_bytes(&mut out);
    out
}

/// A random opaque token: `<prefix>_<base64url(32 bytes)>`.
pub fn random_token(prefix: &str) -> String {
    format!("{prefix}_{}", b64url(&random_bytes(32)))
}

/// Base64url without padding.
pub fn b64url(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Decodes base64url, tolerating trailing `=` padding.
pub fn b64url_decode(text: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(text.trim_end_matches('=')).ok()
}

/// What the store keeps for a bearer token: blake3 hex, never the token.
pub fn token_hash(token: &str) -> String {
    blake3::hash(token.as_bytes()).to_hex().to_string()
}

/// Constant-time equality of two byte strings (length is not secret).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && bool::from(a.ct_eq(b))
}

/// Lowercase hex.
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// SHA-256 digest.
pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::Digest as _;
    Sha256::digest(bytes).into()
}

#[derive(Debug, thiserror::Error)]
pub enum SecretBoxError {
    #[error("ciphertext is malformed")]
    Malformed,
    #[error("ciphertext failed authentication (wrong master key or tampered row)")]
    Authentication,
    #[error("value could not be encrypted")]
    Encrypt,
}

/// Encrypts secret values at rest and fingerprints them. Keys derive from the master key.
#[derive(Clone)]
pub struct SecretBox {
    aead: [u8; 32],
    fingerprint: [u8; 32],
}

const NONCE_LEN: usize = 12;

impl SecretBox {
    pub fn new(master: &MasterKey) -> Self {
        Self { aead: master.derive("aho-cloud secret aead v1"), fingerprint: master.derive("aho-cloud secret fingerprint v1") }
    }

    /// `nonce || ciphertext`, authenticated with `(project, name, stage)` so a row copied to
    /// another project, name or stage fails to open.
    pub fn seal(&self, project: &str, name: &str, stage: Stage, value: &str) -> Result<Vec<u8>, SecretBoxError> {
        self.seal_with(&Self::aad(project, name, stage), value.as_bytes())
    }

    pub fn open(&self, project: &str, name: &str, stage: Stage, sealed: &[u8]) -> Result<String, SecretBoxError> {
        let plain = self.open_with(&Self::aad(project, name, stage), sealed)?;
        String::from_utf8(plain).map_err(|_| SecretBoxError::Malformed)
    }

    /// Seals a sandbox environment value, authenticated with `(sandbox, name)` in a domain of its
    /// own: a row never opens as another sandbox's, another variable's or a project secret.
    pub fn seal_env(&self, sandbox: &str, name: &str, value: &str) -> Result<Vec<u8>, SecretBoxError> {
        self.seal_with(format!("aho-cloud/sandbox-env/v1\n{sandbox}\n{name}").as_bytes(), value.as_bytes())
    }

    pub fn open_env(&self, sandbox: &str, name: &str, sealed: &[u8]) -> Result<String, SecretBoxError> {
        let plain = self.open_with(format!("aho-cloud/sandbox-env/v1\n{sandbox}\n{name}").as_bytes(), sealed)?;
        String::from_utf8(plain).map_err(|_| SecretBoxError::Malformed)
    }

    /// Seals an internal document (e.g. the domain registry snapshot) bound to `label`.
    pub fn seal_bytes(&self, label: &str, bytes: &[u8]) -> Result<Vec<u8>, SecretBoxError> {
        self.seal_with(format!("aho-cloud/doc/v1\n{label}").as_bytes(), bytes)
    }

    pub fn open_bytes(&self, label: &str, sealed: &[u8]) -> Result<Vec<u8>, SecretBoxError> {
        self.open_with(format!("aho-cloud/doc/v1\n{label}").as_bytes(), sealed)
    }

    fn seal_with(&self, aad: &[u8], msg: &[u8]) -> Result<Vec<u8>, SecretBoxError> {
        let cipher = ChaCha20Poly1305::new(Key::from_slice(&self.aead));
        let nonce = random_bytes(NONCE_LEN);
        let ct = cipher.encrypt(Nonce::from_slice(&nonce), Payload { msg, aad }).map_err(|_| SecretBoxError::Encrypt)?;
        let mut out = nonce;
        out.extend_from_slice(&ct);
        Ok(out)
    }

    fn open_with(&self, aad: &[u8], sealed: &[u8]) -> Result<Vec<u8>, SecretBoxError> {
        if sealed.len() <= NONCE_LEN {
            return Err(SecretBoxError::Malformed);
        }
        let (nonce, ct) = sealed.split_at(NONCE_LEN);
        let cipher = ChaCha20Poly1305::new(Key::from_slice(&self.aead));
        cipher.decrypt(Nonce::from_slice(nonce), Payload { msg: ct, aad }).map_err(|_| SecretBoxError::Authentication)
    }

    /// `hmac-sha256:<hex>` of the value under the fingerprint key.
    pub fn fingerprint(&self, value: &str) -> String {
        format!("hmac-sha256:{}", hex(&hmac_sha256(&self.fingerprint, value.as_bytes())))
    }

    fn aad(project: &str, name: &str, stage: Stage) -> Vec<u8> {
        format!("aho-cloud/secret/v1\n{project}\n{name}\n{}", stage.as_str()).into_bytes()
    }
}

/// HMAC-SHA256.
pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    use sha2::Digest as _;
    // RFC 2104 by hand: keys longer than the 64-byte block are hashed first. Infallible, unlike
    // `Mac::new_from_slice`.
    let mut block = [0u8; 64];
    if key.len() > 64 {
        block[..32].copy_from_slice(&sha256(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(block.map(|b| b ^ 0x36));
    inner.update(msg);
    let mut outer = Sha256::new();
    outer.update(block.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret_box() -> SecretBox {
        SecretBox::new(&MasterKey::new([7; 32]))
    }

    #[test]
    fn sealed_values_open_only_in_their_own_slot() {
        let b = secret_box();
        let sealed = b.seal("shop", "STRIPE_KEY", Stage::Live, "sk_live_123").unwrap();
        assert!(!sealed.windows(11).any(|w| w == b"sk_live_123"), "plaintext must not appear in the row");
        assert_eq!(b.open("shop", "STRIPE_KEY", Stage::Live, &sealed).unwrap(), "sk_live_123");
        assert!(matches!(b.open("other", "STRIPE_KEY", Stage::Live, &sealed), Err(SecretBoxError::Authentication)));
        assert!(matches!(b.open("shop", "OTHER", Stage::Live, &sealed), Err(SecretBoxError::Authentication)));
        assert!(matches!(b.open("shop", "STRIPE_KEY", Stage::Staging, &sealed), Err(SecretBoxError::Authentication)));
        let other_key = SecretBox::new(&MasterKey::new([8; 32]));
        assert!(other_key.open("shop", "STRIPE_KEY", Stage::Live, &sealed).is_err());
    }

    #[test]
    fn sandbox_env_values_open_only_as_themselves() {
        let b = secret_box();
        let sealed = b.seal_env("dev", "ANTHROPIC_API_KEY", "sk-ant-123").unwrap();
        assert!(!sealed.windows(10).any(|w| w == b"sk-ant-123"), "plaintext must not appear in the row");
        assert_eq!(b.open_env("dev", "ANTHROPIC_API_KEY", &sealed).unwrap(), "sk-ant-123");
        assert!(matches!(b.open_env("web", "ANTHROPIC_API_KEY", &sealed), Err(SecretBoxError::Authentication)));
        assert!(matches!(b.open_env("dev", "OTHER", &sealed), Err(SecretBoxError::Authentication)));
        for stage in [Stage::Preview, Stage::Staging, Stage::Live] {
            assert!(b.open("dev", "ANTHROPIC_API_KEY", stage, &sealed).is_err(), "never a project secret");
        }
        let secret = b.seal("dev", "ANTHROPIC_API_KEY", Stage::Live, "sk-ant-123").unwrap();
        assert!(b.open_env("dev", "ANTHROPIC_API_KEY", &secret).is_err(), "and no project secret is an env value");
    }

    #[test]
    fn tampered_ciphertext_is_refused() {
        let b = secret_box();
        let mut sealed = b.seal("shop", "K", Stage::Live, "value").unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 1;
        assert!(b.open("shop", "K", Stage::Live, &sealed).is_err());
        assert!(matches!(b.open("shop", "K", Stage::Live, &sealed[..4]), Err(SecretBoxError::Malformed)));
    }

    #[test]
    fn fingerprints_are_keyed_and_stable() {
        let b = secret_box();
        assert_eq!(b.fingerprint("v"), b.fingerprint("v"));
        assert_ne!(b.fingerprint("v"), b.fingerprint("w"));
        assert_ne!(b.fingerprint("v"), SecretBox::new(&MasterKey::new([9; 32])).fingerprint("v"));
        assert!(!b.fingerprint("v").contains(&hex(&sha256(b"v"))));
    }

    #[test]
    fn hmac_matches_rfc4231() {
        // RFC 4231 test cases 2 (short key) and 6 (key longer than the block).
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        assert_eq!(
            hex(&hmac_sha256(&[0xaa; 131], b"Test Using Larger Than Block-Size Key - Hash Key First")),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn master_key_parses_hex_and_base64url() {
        let hex_key = "00".repeat(31) + "ff";
        let k = MasterKey::parse(&hex_key).unwrap();
        assert_eq!(
            k.derive("x"),
            MasterKey::new({
                let mut b = [0u8; 32];
                b[31] = 0xff;
                b
            })
            .derive("x")
        );
        assert!(MasterKey::parse(&b64url(&[1u8; 32])).is_some());
        assert!(MasterKey::parse("short").is_none());
        assert!(MasterKey::parse(&"zz".repeat(32)).is_none());
    }
}
