//! Turning an [`ApprovalProof`] into a ledger [`Approver`]: a device-key signature or a passkey
//! assertion over the exact approval message, made by the calling user's own credential.

use cloud_api::ApprovalProof;
use cloud_contract::ledger::{Approver, ProofMethod, Stage, approval_message};
use cloud_contract::policy::{ReleasePolicy, StandingOrder};
use ed25519_dalek::{Signature, Verifier as _, VerifyingKey};

use crate::crypto::{b64url, b64url_decode, sha256};
use crate::error::AppError;
use crate::state::AppState;
use crate::store::{DeviceKeyRec, PasskeyRec, UserRec};
use crate::webauthn::{self, Assertion, RelyingParty};

/// What an org owner signs to change the org's release policy. Org-scoped: the ledger records
/// the change per project, and [`audit_verifier`] maps its per-project message back to this.
pub fn policy_message(org: &str, policy: &ReleasePolicy) -> Vec<u8> {
    approval_message(org, cloud_ledger::POLICY_SUBJECT, &cloud_contract::canonical::hash(policy), Stage::Live)
}

/// What an owner signs to grant standing orders on `project`.
pub fn standing_orders_message(project: &str, orders: &[StandingOrder]) -> Vec<u8> {
    cloud_ledger::standing_orders_message(project, orders)
}

/// Ed25519 key from base64url.
pub fn device_public_key(b64: &str) -> Option<VerifyingKey> {
    let bytes = b64url_decode(b64)?;
    VerifyingKey::from_bytes(&<[u8; 32]>::try_from(bytes.as_slice()).ok()?).ok()
}

fn verify_device_signature(key: &DeviceKeyRec, message: &[u8], signature: &str) -> bool {
    let Some(vk) = device_public_key(&key.public_key) else {
        return false;
    };
    let Some(sig) = b64url_decode(signature).and_then(|b| <[u8; 64]>::try_from(b.as_slice()).ok()) else {
        return false;
    };
    vk.verify(message, &Signature::from_bytes(&sig)).is_ok()
}

/// Verifies `proof` over `message` for `user` and returns the approver to record. `owner` is the
/// user's standing in the org that owns the object being approved.
pub async fn approver(state: &AppState, user: &UserRec, owner: bool, message: &[u8], proof: &ApprovalProof) -> Result<Approver, AppError> {
    let (method, signature) = match proof {
        ApprovalProof::DeviceKey { key_id, signature } => {
            let key = state
                .store
                .device_key(key_id)
                .await?
                .filter(|k| k.user_id == user.id)
                .ok_or_else(|| AppError::forbidden("unknown device key"))?;
            if !verify_device_signature(&key, message, signature) {
                return Err(AppError::forbidden("device-key signature does not verify over the approval message"));
            }
            (ProofMethod::DeviceKey, signature.clone())
        }
        ApprovalProof::Passkey { credential } => {
            let a = webauthn::parse_assertion(credential).map_err(|e| AppError::invalid(e.to_string()))?;
            let cred_id = b64url(&a.credential_id);
            let pk = state
                .store
                .passkey(&cred_id)
                .await?
                .filter(|p| p.user_id == user.id)
                .ok_or_else(|| AppError::forbidden("unknown passkey"))?;
            let count = webauthn::verify_assertion(&state.rp, &sha256(message), &a, &pk.public_key, pk.sign_count)
                .map_err(|e| AppError::forbidden(e.to_string()))?;
            if !state.store.update_sign_count(&cred_id, pk.sign_count, count).await? {
                return Err(AppError::conflict("passkey used concurrently; retry"));
            }
            (ProofMethod::Passkey, a.encode())
        }
    };
    Ok(Approver { user_id: user.id.clone(), email: user.email.clone(), owner, method, signature })
}

/// The credentials an audit checks recorded approvals against.
pub struct AuditKeys {
    pub device_keys: Vec<DeviceKeyRec>,
    pub passkeys: Vec<PasskeyRec>,
}

/// A verifier for [`cloud_ledger::Ledger::audit`]: re-checks each recorded signature against
/// the approver's registered credentials. Passkey assertions are checked for signature,
/// challenge, origin, rpIdHash and flags (the counter is only meaningful at the time of use).
/// Policy changes are signed org-scoped, so the ledger's per-project policy message is mapped
/// back to `org` before checking.
pub fn audit_verifier(keys: AuditKeys, rp: RelyingParty, project: String, org: String) -> impl Fn(&Approver, &[u8]) -> bool {
    move |approver, message| {
        let org_form;
        let message = match policy_scope(message, &project) {
            Some(rest) => {
                org_form = format!("aho-cloud/approve/v1\n{org}\n{rest}").into_bytes();
                org_form.as_slice()
            }
            None => message,
        };
        match approver.method {
            ProofMethod::DeviceKey => keys
                .device_keys
                .iter()
                .filter(|k| k.user_id == approver.user_id)
                .any(|k| verify_device_signature(k, message, &approver.signature)),
            ProofMethod::Passkey => {
                let Ok(a) = Assertion::decode(&approver.signature) else {
                    return false;
                };
                let id = b64url(&a.credential_id);
                keys.passkeys
                    .iter()
                    .filter(|p| p.user_id == approver.user_id && p.credential_id == id)
                    .any(|p| webauthn::verify_assertion(&rp, &sha256(message), &a, &p.public_key, 0).is_ok())
            }
        }
    }
}

/// For a per-project policy message of `project`, the part after the project line.
fn policy_scope<'m>(message: &'m [u8], project: &str) -> Option<&'m str> {
    let text = std::str::from_utf8(message).ok()?;
    let rest = text.strip_prefix("aho-cloud/approve/v1\n")?.strip_prefix(project)?.strip_prefix('\n')?;
    rest.starts_with(&format!("{}\n", cloud_ledger::POLICY_SUBJECT)).then_some(rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_messages_are_mapped_to_the_org_scope_only() {
        let policy = ReleasePolicy::default();
        let project_form = cloud_ledger::policy_message("shop", &policy);
        let rest = policy_scope(&project_form, "shop").unwrap();
        assert_eq!(format!("aho-cloud/approve/v1\nacme\n{rest}").into_bytes(), policy_message("acme", &policy));
        let release = approval_message("shop", "r1", "blake3:00", Stage::Live);
        assert!(policy_scope(&release, "shop").is_none());
        assert!(policy_scope(&project_form, "other").is_none());
    }
}
