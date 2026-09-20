//! Device-key approvals: Ed25519 over the exact bytes the ledger verifies, base64url-no-pad.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use cloud_api::ApprovalProof;
use cloud_contract::ledger::{Stage, approval_message};
use cloud_contract::policy::StandingOrder;
use ed25519_dalek::{Signer, SigningKey};
use rand_core::OsRng;

use crate::creds::Credentials;
use crate::error::Result;

/// A fresh device key from the OS RNG.
pub fn generate() -> SigningKey {
    SigningKey::generate(&mut OsRng)
}

/// The 32-byte public key, base64url, as `DeviceStart.public_key` carries it.
pub fn public_key(key: &SigningKey) -> String {
    URL_SAFE_NO_PAD.encode(key.verifying_key().as_bytes())
}

fn proof(creds: &Credentials, message: &[u8]) -> Result<ApprovalProof> {
    let key = creds.signing_key()?;
    Ok(ApprovalProof::DeviceKey { key_id: creds.key_id.clone(), signature: URL_SAFE_NO_PAD.encode(key.sign(message).to_bytes()) })
}

/// Approval of `release` (whose manifest hashes to `manifest_hash`) for `stage`.
pub fn approval(creds: &Credentials, project: &str, release: &str, manifest_hash: &str, stage: Stage) -> Result<ApprovalProof> {
    proof(creds, &approval_message(project, release, manifest_hash, stage))
}

/// Standing orders bind the project and the orders; the server caps the lifetime.
pub fn standing_orders(creds: &Credentials, project: &str, orders: &[StandingOrder]) -> Result<ApprovalProof> {
    let hash = cloud_contract::canonical::hash(&orders);
    proof(creds, &approval_message(project, "standing-orders", &hash, Stage::Live))
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::{Signature, Verifier};

    use super::*;

    #[test]
    fn signatures_verify_over_the_approval_message_only() {
        let key = SigningKey::from_bytes(&[9; 32]);
        let creds = Credentials::new("http://x", "t".into(), "k1".into(), &key);
        let ApprovalProof::DeviceKey { key_id, signature } = approval(&creds, "demo", "r1", "blake3:ab", Stage::Staging).unwrap() else {
            panic!("device key proof expected")
        };
        assert_eq!(key_id, "k1");
        let sig = Signature::from_slice(&URL_SAFE_NO_PAD.decode(signature).unwrap()).unwrap();
        let pk = key.verifying_key();
        assert!(pk.verify(&approval_message("demo", "r1", "blake3:ab", Stage::Staging), &sig).is_ok());
        assert!(pk.verify(&approval_message("demo", "r1", "blake3:ab", Stage::Live), &sig).is_err());
        assert!(pk.verify(&approval_message("demo", "r2", "blake3:ab", Stage::Staging), &sig).is_err());
    }
}
