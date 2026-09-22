//! WebAuthn for ES256 passkeys: registration with `none` attestation and assertion
//! verification, checking type, challenge, origin, rpIdHash, the UP/UV flags and the signature
//! counter. Implemented on `p256` + `ciborium` (no OpenSSL).

use ciborium::value::Value as Cbor;
use p256::ecdsa::signature::Verifier as _;
use p256::ecdsa::{Signature, VerifyingKey};
use serde_json::{Value, json};

use crate::crypto::{b64url, b64url_decode, ct_eq, sha256};

const FLAG_UP: u8 = 0x01;
const FLAG_UV: u8 = 0x04;
const FLAG_AT: u8 = 0x40;
/// COSE algorithm id of ES256.
pub const ES256: i64 = -7;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelyingParty {
    pub id: String,
    pub origin: String,
    pub name: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum WebauthnError {
    #[error("credential JSON is missing or has a malformed `{0}`")]
    Field(&'static str),
    #[error("clientDataJSON type is `{0}`, expected `{1}`")]
    Type(String, &'static str),
    #[error("challenge does not match the one issued")]
    Challenge,
    #[error("origin `{0}` is not the console origin")]
    Origin(String),
    #[error("cross-origin ceremonies are refused")]
    CrossOrigin,
    #[error("authenticator data is for another relying party")]
    RpIdHash,
    #[error("the user was not present (UP flag clear)")]
    UserPresence,
    #[error("the user was not verified (UV flag clear)")]
    UserVerification,
    #[error("authenticator data is malformed: {0}")]
    AuthData(&'static str),
    #[error("attestation format `{0}` is not accepted (only `none`)")]
    AttestationFormat(String),
    #[error("credential public key is not an ES256 P-256 key: {0}")]
    Key(&'static str),
    #[error("credential id in the response does not match the authenticator data")]
    CredentialId,
    #[error("assertion signature does not verify")]
    Signature,
    #[error("signature counter did not increase ({stored} → {received}); the authenticator may be cloned")]
    Counter { stored: u32, received: u32 },
}

/// `PublicKeyCredentialCreationOptions` as the browser JSON (binary fields base64url).
pub fn creation_options(
    rp: &RelyingParty,
    user_handle: &[u8],
    email: &str,
    name: &str,
    challenge: &[u8],
    exclude: &[String],
    timeout_ms: u64,
) -> Value {
    json!({
        "publicKey": {
            "rp": {"id": rp.id, "name": rp.name},
            "user": {"id": b64url(user_handle), "name": email, "displayName": name},
            "challenge": b64url(challenge),
            "pubKeyCredParams": [{"type": "public-key", "alg": ES256}],
            "timeout": timeout_ms,
            "attestation": "none",
            "authenticatorSelection": {"residentKey": "preferred", "userVerification": "required"},
            "excludeCredentials": exclude.iter().map(|id| json!({"type": "public-key", "id": id})).collect::<Vec<_>>(),
        }
    })
}

/// `PublicKeyCredentialRequestOptions` as the browser JSON.
pub fn request_options(rp: &RelyingParty, challenge: &[u8], allow: &[String], timeout_ms: u64) -> Value {
    json!({
        "publicKey": {
            "rpId": rp.id,
            "challenge": b64url(challenge),
            "timeout": timeout_ms,
            "userVerification": "required",
            "allowCredentials": allow.iter().map(|id| json!({"type": "public-key", "id": id})).collect::<Vec<_>>(),
        }
    })
}

/// A credential registration verified; store it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewCredential {
    pub id: Vec<u8>,
    /// SEC1 uncompressed point.
    pub public_key: Vec<u8>,
    pub sign_count: u32,
}

/// The parts of an assertion, decoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assertion {
    pub credential_id: Vec<u8>,
    pub authenticator_data: Vec<u8>,
    pub client_data_json: Vec<u8>,
    pub signature: Vec<u8>,
    pub user_handle: Option<Vec<u8>>,
}

impl Assertion {
    /// A compact, self-describing record of the assertion for the ledger (`Approver.signature`),
    /// enough to re-verify it later against the stored public key.
    pub fn encode(&self) -> String {
        let doc = json!({
            "id": b64url(&self.credential_id),
            "authenticatorData": b64url(&self.authenticator_data),
            "clientDataJSON": b64url(&self.client_data_json),
            "signature": b64url(&self.signature),
        });
        b64url(&cloud_contract::canonical::to_bytes(&doc))
    }

    /// Inverse of [`Assertion::encode`].
    pub fn decode(text: &str) -> Result<Self, WebauthnError> {
        let bytes = b64url_decode(text).ok_or(WebauthnError::Field("signature"))?;
        let doc: Value = serde_json::from_slice(&bytes).map_err(|_| WebauthnError::Field("signature"))?;
        Ok(Self {
            credential_id: b64_field(&doc, "id")?,
            authenticator_data: b64_field(&doc, "authenticatorData")?,
            client_data_json: b64_field(&doc, "clientDataJSON")?,
            signature: b64_field(&doc, "signature")?,
            user_handle: None,
        })
    }
}

fn b64_field(v: &Value, name: &'static str) -> Result<Vec<u8>, WebauthnError> {
    v.get(name).and_then(Value::as_str).and_then(b64url_decode).ok_or(WebauthnError::Field(name))
}

fn credential_id(credential: &Value) -> Result<Vec<u8>, WebauthnError> {
    let raw = credential.get("rawId").and_then(Value::as_str).map(|s| b64url_decode(s).ok_or(WebauthnError::Field("rawId"))).transpose()?;
    let id = credential.get("id").and_then(Value::as_str).map(|s| b64url_decode(s).ok_or(WebauthnError::Field("id"))).transpose()?;
    if credential.get("type").and_then(Value::as_str).is_some_and(|t| t != "public-key") {
        return Err(WebauthnError::Field("type"));
    }
    match (raw, id) {
        (Some(a), Some(b)) if a != b => Err(WebauthnError::CredentialId),
        (Some(a), _) | (None, Some(a)) => Ok(a),
        (None, None) => Err(WebauthnError::Field("rawId")),
    }
}

fn response(credential: &Value) -> Result<&Value, WebauthnError> {
    credential.get("response").filter(|r| r.is_object()).ok_or(WebauthnError::Field("response"))
}

/// Checks `clientDataJSON`: type, challenge, origin, not cross-origin.
fn check_client_data(rp: &RelyingParty, cdj: &[u8], expected_type: &'static str, challenge: &[u8]) -> Result<(), WebauthnError> {
    let cd: Value = serde_json::from_slice(cdj).map_err(|_| WebauthnError::Field("clientDataJSON"))?;
    let ty = cd.get("type").and_then(Value::as_str).ok_or(WebauthnError::Field("clientDataJSON.type"))?;
    if ty != expected_type {
        return Err(WebauthnError::Type(ty.to_owned(), expected_type));
    }
    let got =
        cd.get("challenge").and_then(Value::as_str).and_then(b64url_decode).ok_or(WebauthnError::Field("clientDataJSON.challenge"))?;
    if !ct_eq(&got, challenge) {
        return Err(WebauthnError::Challenge);
    }
    let origin = cd.get("origin").and_then(Value::as_str).ok_or(WebauthnError::Field("clientDataJSON.origin"))?;
    if origin != rp.origin {
        return Err(WebauthnError::Origin(origin.to_owned()));
    }
    if cd.get("crossOrigin").and_then(Value::as_bool).unwrap_or(false) {
        return Err(WebauthnError::CrossOrigin);
    }
    Ok(())
}

struct AuthData<'a> {
    flags: u8,
    sign_count: u32,
    attested: &'a [u8],
}

/// Checks rpIdHash and the UP + UV flags.
fn parse_auth_data<'a>(rp: &RelyingParty, data: &'a [u8]) -> Result<AuthData<'a>, WebauthnError> {
    if data.len() < 37 {
        return Err(WebauthnError::AuthData("shorter than 37 bytes"));
    }
    if !ct_eq(&data[..32], &sha256(rp.id.as_bytes())) {
        return Err(WebauthnError::RpIdHash);
    }
    let flags = data[32];
    if flags & FLAG_UP == 0 {
        return Err(WebauthnError::UserPresence);
    }
    if flags & FLAG_UV == 0 {
        return Err(WebauthnError::UserVerification);
    }
    let sign_count = u32::from_be_bytes([data[33], data[34], data[35], data[36]]);
    Ok(AuthData { flags, sign_count, attested: &data[37..] })
}

fn cbor_int(v: &Cbor) -> Option<i128> {
    v.as_integer().map(i128::from)
}

fn cose_get(map: &[(Cbor, Cbor)], key: i128) -> Option<&Cbor> {
    map.iter().find(|(k, _)| cbor_int(k) == Some(key)).map(|(_, v)| v)
}

/// COSE_Key (EC2, P-256, ES256) → SEC1 uncompressed point.
fn cose_es256(key: &Cbor) -> Result<Vec<u8>, WebauthnError> {
    let map = key.as_map().ok_or(WebauthnError::Key("not a map"))?;
    if cose_get(map, 1).and_then(cbor_int) != Some(2) {
        return Err(WebauthnError::Key("kty is not EC2"));
    }
    if cose_get(map, 3).and_then(cbor_int) != Some(i128::from(ES256)) {
        return Err(WebauthnError::Key("alg is not ES256"));
    }
    if cose_get(map, -1).and_then(cbor_int) != Some(1) {
        return Err(WebauthnError::Key("curve is not P-256"));
    }
    let x = cose_get(map, -2).and_then(Cbor::as_bytes).filter(|b| b.len() == 32).ok_or(WebauthnError::Key("x"))?;
    let y = cose_get(map, -3).and_then(Cbor::as_bytes).filter(|b| b.len() == 32).ok_or(WebauthnError::Key("y"))?;
    let mut sec1 = Vec::with_capacity(65);
    sec1.push(0x04);
    sec1.extend_from_slice(x);
    sec1.extend_from_slice(y);
    VerifyingKey::from_sec1_bytes(&sec1).map_err(|_| WebauthnError::Key("point is not on the curve"))?;
    Ok(sec1)
}

/// Verifies a `navigator.credentials.create()` result against the issued challenge.
pub fn verify_registration(rp: &RelyingParty, challenge: &[u8], credential: &Value) -> Result<NewCredential, WebauthnError> {
    let id = credential_id(credential)?;
    let resp = response(credential)?;
    let cdj = b64_field(resp, "clientDataJSON")?;
    check_client_data(rp, &cdj, "webauthn.create", challenge)?;
    let att_bytes = b64_field(resp, "attestationObject")?;
    let att: Cbor = ciborium::from_reader(att_bytes.as_slice()).map_err(|_| WebauthnError::Field("attestationObject"))?;
    let att = att.as_map().ok_or(WebauthnError::Field("attestationObject"))?;
    let text_key = |name: &str| att.iter().find(|(k, _)| k.as_text() == Some(name)).map(|(_, v)| v);
    let fmt = text_key("fmt").and_then(Cbor::as_text).ok_or(WebauthnError::Field("attestationObject.fmt"))?;
    if fmt != "none" {
        return Err(WebauthnError::AttestationFormat(fmt.to_owned()));
    }
    if !text_key("attStmt").and_then(Cbor::as_map).is_some_and(Vec::is_empty) {
        return Err(WebauthnError::AttestationFormat("none with a non-empty attStmt".into()));
    }
    let auth_data = text_key("authData").and_then(Cbor::as_bytes).ok_or(WebauthnError::Field("attestationObject.authData"))?;
    let ad = parse_auth_data(rp, auth_data)?;
    if ad.flags & FLAG_AT == 0 {
        return Err(WebauthnError::AuthData("no attested credential data"));
    }
    let rest = ad.attested;
    if rest.len() < 18 {
        return Err(WebauthnError::AuthData("attested credential data truncated"));
    }
    let id_len = usize::from(u16::from_be_bytes([rest[16], rest[17]]));
    let rest = &rest[18..];
    if rest.len() < id_len || id_len == 0 || id_len > 1023 {
        return Err(WebauthnError::AuthData("credential id length"));
    }
    let (cred_id, key_bytes) = rest.split_at(id_len);
    if cred_id != id.as_slice() {
        return Err(WebauthnError::CredentialId);
    }
    let key: Cbor = ciborium::from_reader(key_bytes).map_err(|_| WebauthnError::Key("not CBOR"))?;
    let public_key = cose_es256(&key)?;
    Ok(NewCredential { id, public_key, sign_count: ad.sign_count })
}

/// Decodes a `navigator.credentials.get()` result.
pub fn parse_assertion(credential: &Value) -> Result<Assertion, WebauthnError> {
    let credential_id = credential_id(credential)?;
    let resp = response(credential)?;
    let user_handle = match resp.get("userHandle") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if s.is_empty() => None,
        Some(Value::String(s)) => Some(b64url_decode(s).ok_or(WebauthnError::Field("userHandle"))?),
        Some(_) => return Err(WebauthnError::Field("userHandle")),
    };
    Ok(Assertion {
        credential_id,
        authenticator_data: b64_field(resp, "authenticatorData")?,
        client_data_json: b64_field(resp, "clientDataJSON")?,
        signature: b64_field(resp, "signature")?,
        user_handle,
    })
}

/// Verifies an assertion made with the credential whose public key is `public_key` and whose
/// stored counter is `stored_count`. Returns the new counter to store.
pub fn verify_assertion(
    rp: &RelyingParty,
    challenge: &[u8],
    a: &Assertion,
    public_key: &[u8],
    stored_count: u32,
) -> Result<u32, WebauthnError> {
    check_client_data(rp, &a.client_data_json, "webauthn.get", challenge)?;
    let ad = parse_auth_data(rp, &a.authenticator_data)?;
    let key = VerifyingKey::from_sec1_bytes(public_key).map_err(|_| WebauthnError::Key("stored key"))?;
    let sig = Signature::from_der(&a.signature).map_err(|_| WebauthnError::Signature)?;
    let mut signed = a.authenticator_data.clone();
    signed.extend_from_slice(&sha256(&a.client_data_json));
    key.verify(&signed, &sig).map_err(|_| WebauthnError::Signature)?;
    // Authenticators that never count report 0 forever; any counter that moved must keep rising.
    if (ad.sign_count != 0 || stored_count != 0) && ad.sign_count <= stored_count {
        return Err(WebauthnError::Counter { stored: stored_count, received: ad.sign_count });
    }
    Ok(ad.sign_count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fakes::SoftAuthenticator;

    fn rp() -> RelyingParty {
        RelyingParty { id: "console.test".into(), origin: "https://console.test".into(), name: "test".into() }
    }

    fn registered() -> (SoftAuthenticator, NewCredential) {
        let mut auth = SoftAuthenticator::new("console.test", "https://console.test");
        let opts = creation_options(&rp(), b"user-1", "a@x.io", "A", b"challenge-1", &[], 60_000);
        let cred = auth.register(&opts);
        let new = verify_registration(&rp(), b"challenge-1", &cred).unwrap();
        (auth, new)
    }

    #[test]
    fn registration_accepts_a_none_attestation_and_extracts_the_key() {
        let (auth, new) = registered();
        assert_eq!(new.id, auth.credential_ids()[0]);
        assert_eq!(new.public_key.len(), 65);
    }

    #[test]
    fn registration_rejects_wrong_challenge_origin_type_and_rp() {
        let mut auth = SoftAuthenticator::new("console.test", "https://console.test");
        let opts = creation_options(&rp(), b"u", "a@x.io", "A", b"c", &[], 1);
        let cred = auth.register(&opts);
        assert_eq!(verify_registration(&rp(), b"other", &cred), Err(WebauthnError::Challenge));
        let other_origin = RelyingParty { origin: "https://evil.test".into(), ..rp() };
        assert!(matches!(verify_registration(&other_origin, b"c", &cred), Err(WebauthnError::Origin(_))));
        let other_rp = RelyingParty { id: "evil.test".into(), ..rp() };
        assert_eq!(verify_registration(&other_rp, b"c", &cred), Err(WebauthnError::RpIdHash));

        let req = request_options(&rp(), b"c", &[], 1);
        let assertion_as_registration = auth.assert(&req);
        assert!(verify_registration(&rp(), b"c", &assertion_as_registration).is_err());
    }

    #[test]
    fn registration_requires_user_verification() {
        let mut auth = SoftAuthenticator::new("console.test", "https://console.test");
        auth.user_verified = false;
        let cred = auth.register(&creation_options(&rp(), b"u", "a@x.io", "A", b"c", &[], 1));
        assert_eq!(verify_registration(&rp(), b"c", &cred), Err(WebauthnError::UserVerification));
    }

    #[test]
    fn assertion_verifies_and_the_counter_must_rise() {
        let (mut auth, new) = registered();
        let a1 = parse_assertion(&auth.assert(&request_options(&rp(), b"login-1", &[], 1))).unwrap();
        let c1 = verify_assertion(&rp(), b"login-1", &a1, &new.public_key, new.sign_count).unwrap();
        assert!(c1 > new.sign_count);
        // Replaying the same assertion: counter no longer rises.
        assert!(matches!(verify_assertion(&rp(), b"login-1", &a1, &new.public_key, c1), Err(WebauthnError::Counter { .. })));
        // Wrong challenge.
        let a2 = parse_assertion(&auth.assert(&request_options(&rp(), b"login-2", &[], 1))).unwrap();
        assert_eq!(verify_assertion(&rp(), b"login-3", &a2, &new.public_key, c1), Err(WebauthnError::Challenge));
    }

    #[test]
    fn assertion_with_a_tampered_signature_or_foreign_key_fails() {
        let (mut auth, new) = registered();
        let mut a = parse_assertion(&auth.assert(&request_options(&rp(), b"x", &[], 1))).unwrap();
        let (_, other) = registered();
        assert_eq!(verify_assertion(&rp(), b"x", &a, &other.public_key, 0), Err(WebauthnError::Signature));
        a.authenticator_data[36] ^= 0x10;
        assert_eq!(verify_assertion(&rp(), b"x", &a, &new.public_key, 0), Err(WebauthnError::Signature));
    }

    #[test]
    fn assertion_record_round_trips() {
        let (mut auth, _) = registered();
        let a = parse_assertion(&auth.assert(&request_options(&rp(), b"x", &[], 1))).unwrap();
        let back = Assertion::decode(&a.encode()).unwrap();
        assert_eq!(Assertion { user_handle: None, ..a }, back);
    }
}
