//! [`GcpKmsKek`]: production KEK, a Cloud KMS symmetric CryptoKey over the REST API
//! (`cryptoKeys.encrypt` / `cryptoKeys.decrypt`, `cryptoKeys.get`) with the caller's AAD as
//! `additionalAuthenticatedData` and end-to-end CRC32C checks in both directions (Google's
//! "verifying end-to-end data integrity" recipe). `kek_version` is the CryptoKeyVersion that
//! encrypted (response `name`); decrypt addresses the CryptoKey and KMS picks the version from
//! the ciphertext, so old versions keep working after rotation. Access tokens: Application
//! Default Credentials (the vault's own [`AdcTokenSource`]).

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use tracing::error;
use url::Url;

use async_trait::async_trait;

use super::Kek;
use crate::adapters::driven::llm_upstream::adc::{AdcError, AdcTokenSource};
use crate::ports::vault::{SecretBytes, VaultError};

/// Production Cloud KMS endpoint.
pub const KMS_ENDPOINT: &str = "https://cloudkms.googleapis.com/";

pub struct GcpKmsKek {
    http: reqwest::Client,
    endpoint: String,
    key: String,
    tokens: AdcTokenSource,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    Encrypt,
    Decrypt,
    GetKey,
}

impl Op {
    fn as_str(self) -> &'static str {
        match self {
            Op::Encrypt => "encrypt",
            Op::Decrypt => "decrypt",
            Op::GetKey => "get key",
        }
    }
}

impl GcpKmsKek {
    /// `key` is the CryptoKey resource name,
    /// `projects/{p}/locations/{l}/keyRings/{r}/cryptoKeys/{k}`.
    pub fn new(key: impl Into<String>, tokens: AdcTokenSource) -> Result<GcpKmsKek, VaultError> {
        let key = key.into();
        check_key_name(&key)?;
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| VaultError::Invalid(format!("http client: {e}")))?;
        Ok(GcpKmsKek { http, endpoint: KMS_ENDPOINT.trim_end_matches('/').to_owned(), key, tokens })
    }

    /// Another API base (tests); `KMS_ENDPOINT` otherwise.
    pub fn with_endpoint(mut self, endpoint: &Url) -> GcpKmsKek {
        self.endpoint = endpoint.as_str().trim_end_matches('/').to_owned();
        self
    }

    pub fn key_name(&self) -> &str {
        &self.key
    }

    fn version_prefix(&self) -> String {
        format!("{}/cryptoKeyVersions/", self.key)
    }

    async fn call<T: serde::de::DeserializeOwned>(&self, op: Op, body: Option<&impl Serialize>) -> Result<T, VaultError> {
        let token = self.tokens.access_token().await.map_err(|e| match e {
            AdcError::Config(m) => VaultError::Invalid(m),
            AdcError::Unavailable(m) => VaultError::Unavailable(m),
        })?;
        let request = match (op, body) {
            (Op::GetKey, _) | (_, None) => self.http.get(format!("{}/v1/{}", self.endpoint, self.key)),
            (Op::Encrypt | Op::Decrypt, Some(body)) => {
                self.http.post(format!("{}/v1/{}:{}", self.endpoint, self.key, op.as_str())).json(body)
            }
        };
        let response = request
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| VaultError::Unavailable(format!("Cloud KMS {}: {e}", op.as_str())))?;
        let status = response.status();
        if !status.is_success() {
            let body: Option<ErrorBody> = response.json().await.ok();
            return Err(self.status_error(op, status, body.map(|b| b.error)));
        }
        response.json().await.map_err(|e| VaultError::Unavailable(format!("Cloud KMS {} response: {e}", op.as_str())))
    }

    fn status_error(&self, op: Op, status: StatusCode, detail: Option<ErrorDetail>) -> VaultError {
        let (code, message) = detail.map(|d| (d.status, d.message)).unwrap_or_default();
        let message: String = message.chars().take(300).collect();
        match status {
            StatusCode::FORBIDDEN => {
                error!(key = %self.key, op = op.as_str(), %message, "Cloud KMS permission denied");
                VaultError::Forbidden
            }
            // A wrong AAD (moved row) or a ciphertext of another key: KMS refuses to decrypt.
            StatusCode::BAD_REQUEST if op == Op::Decrypt && code == "INVALID_ARGUMENT" => VaultError::Integrity,
            StatusCode::NOT_FOUND => VaultError::Invalid(format!("Cloud KMS key {} not found: {message}", self.key)),
            _ => VaultError::Unavailable(format!("Cloud KMS {} answered {status} {code}: {message}", op.as_str())),
        }
    }
}

#[async_trait]
impl Kek for GcpKmsKek {
    async fn wrap(&self, aad: &[u8], dek: &[u8]) -> Result<(Vec<u8>, String), VaultError> {
        let request = EncryptRequest {
            plaintext: STANDARD.encode(dek),
            plaintext_crc32c: crc32c(dek).to_string(),
            additional_authenticated_data: STANDARD.encode(aad),
            additional_authenticated_data_crc32c: crc32c(aad).to_string(),
        };
        let response: EncryptResponse = self.call(Op::Encrypt, Some(&request)).await?;
        // Request corrupted on the way in, or response on the way out: retry later.
        if !response.verified_plaintext_crc32c || !response.verified_additional_authenticated_data_crc32c {
            return Err(VaultError::Unavailable("Cloud KMS did not verify the request checksums".into()));
        }
        let ciphertext = STANDARD
            .decode(&response.ciphertext)
            .map_err(|_| VaultError::Unavailable("Cloud KMS returned a ciphertext that is not base64".into()))?;
        if ciphertext.is_empty() || response.ciphertext_crc32c.map(|c| c.0) != Some(crc32c(&ciphertext)) {
            return Err(VaultError::Unavailable("Cloud KMS ciphertext failed its checksum".into()));
        }
        if !response.name.starts_with(&self.version_prefix()) {
            return Err(VaultError::Unavailable(format!("Cloud KMS encrypted with an unexpected key version {:?}", response.name)));
        }
        Ok((ciphertext, response.name))
    }

    async fn unwrap(&self, aad: &[u8], wrapped: &[u8], kek_version: &str) -> Result<SecretBytes, VaultError> {
        if !kek_version.starts_with(&self.version_prefix()) {
            return Err(VaultError::Invalid(format!("DEK wrapped by {kek_version:?}, not by Cloud KMS key {}", self.key)));
        }
        let request = DecryptRequest {
            ciphertext: STANDARD.encode(wrapped),
            ciphertext_crc32c: crc32c(wrapped).to_string(),
            additional_authenticated_data: STANDARD.encode(aad),
            additional_authenticated_data_crc32c: crc32c(aad).to_string(),
        };
        let response: DecryptResponse = self.call(Op::Decrypt, Some(&request)).await?;
        let plaintext = SecretBytes::new(
            STANDARD.decode(&response.plaintext).map_err(|_| VaultError::Unavailable("Cloud KMS returned a plaintext that is not base64".into()))?,
        );
        if response.plaintext_crc32c.map(|c| c.0) != Some(crc32c(plaintext.expose())) {
            return Err(VaultError::Unavailable("Cloud KMS plaintext failed its checksum".into()));
        }
        Ok(plaintext)
    }

    async fn primary_version(&self) -> Result<String, VaultError> {
        let key: CryptoKey = self.call(Op::GetKey, None::<&()>).await?;
        let primary = key.primary.ok_or_else(|| VaultError::Invalid(format!("Cloud KMS key {} has no primary version", self.key)))?;
        if !primary.name.starts_with(&self.version_prefix()) {
            return Err(VaultError::Unavailable(format!("Cloud KMS primary version {:?} is not a version of {}", primary.name, self.key)));
        }
        Ok(primary.name)
    }
}

fn check_key_name(key: &str) -> Result<(), VaultError> {
    let parts: Vec<&str> = key.split('/').collect();
    let id_ok = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.');
    let shape_ok = parts.len() == 8
        && parts[0] == "projects"
        && parts[2] == "locations"
        && parts[4] == "keyRings"
        && parts[6] == "cryptoKeys"
        && [parts[1], parts[3], parts[5], parts[7]].into_iter().all(id_ok);
    if !shape_ok {
        return Err(VaultError::Invalid(format!(
            "Cloud KMS key must be projects/{{p}}/locations/{{l}}/keyRings/{{r}}/cryptoKeys/{{k}}, got {key:?}"
        )));
    }
    Ok(())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EncryptRequest {
    plaintext: String,
    plaintext_crc32c: String,
    additional_authenticated_data: String,
    additional_authenticated_data_crc32c: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EncryptResponse {
    name: String,
    #[serde(default)]
    ciphertext: String,
    #[serde(default)]
    ciphertext_crc32c: Option<Int64>,
    #[serde(default)]
    verified_plaintext_crc32c: bool,
    #[serde(default)]
    verified_additional_authenticated_data_crc32c: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DecryptRequest {
    ciphertext: String,
    ciphertext_crc32c: String,
    additional_authenticated_data: String,
    additional_authenticated_data_crc32c: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DecryptResponse {
    #[serde(default)]
    plaintext: String,
    #[serde(default)]
    plaintext_crc32c: Option<Int64>,
}

#[derive(Deserialize)]
struct CryptoKey {
    #[serde(default)]
    primary: Option<CryptoKeyVersion>,
}

#[derive(Deserialize)]
struct CryptoKeyVersion {
    name: String,
}

#[derive(Deserialize)]
struct ErrorBody {
    error: ErrorDetail,
}

#[derive(Deserialize)]
struct ErrorDetail {
    #[serde(default)]
    message: String,
    #[serde(default)]
    status: String,
}

/// A proto3 JSON `int64` (`Int64Value`): a decimal string, or a number from lenient encoders.
/// CRC32C values fit in `u32`.
struct Int64(u32);

impl<'de> Deserialize<'de> for Int64 {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Text(String),
            Number(u64),
        }
        let value = match Raw::deserialize(deserializer)? {
            Raw::Text(s) => s.parse::<u64>().map_err(serde::de::Error::custom)?,
            Raw::Number(n) => n,
        };
        u32::try_from(value).map(Int64).map_err(serde::de::Error::custom)
    }
}

const CRC32C_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 == 1 { (crc >> 1) ^ 0x82F6_3B78 } else { crc >> 1 };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};

/// CRC-32C (Castagnoli, reflected polynomial `0x82F63B78`), the checksum Cloud KMS uses.
pub fn crc32c(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc = CRC32C_TABLE[((crc ^ u32::from(byte)) & 0xff) as usize] ^ (crc >> 8);
    }
    !crc
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    //! Wiremock stands in for Cloud KMS and the metadata token server. The real-KMS test is
    //! opt-in: set `MAGE_TEST_KMS_KEY` to a symmetric CryptoKey resource name
    //! (`projects/…/locations/…/keyRings/…/cryptoKeys/…`) the Application Default Credentials may
    //! encrypt/decrypt with and read (`roles/cloudkms.cryptoKeyEncrypterDecrypter` +
    //! `cloudkms.cryptoKeys.get`).

    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use serde_json::{Value, json};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    use super::*;
    use crate::adapters::driven::vault::EnvelopeVault;
    use crate::testing::{MemSecretStore, VaultFixture, contract_secret_vault};

    const KEY: &str = "projects/p/locations/l/keyRings/mage/cryptoKeys/mage-kek-platform";
    const TOKEN_PATH: &str = "/computeMetadata/v1/instance/service-accounts/default/token";

    #[test]
    fn crc32c_matches_the_castagnoli_check_value() {
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c(b""), 0);
        assert_eq!(crc32c(&[0u8; 32]), 0x8A91_36AA);
    }

    #[test]
    fn key_names_are_checked() {
        assert!(check_key_name(KEY).is_ok());
        for bad in ["", "projects/p", "projects/p/locations/l/keyRings/r/cryptoKeys/", "projects/p/locations/l/keyRings/r/cryptoKeys/k/cryptoKeyVersions/1"] {
            assert!(check_key_name(bad).is_err(), "{bad}");
        }
    }

    fn b64(v: &Value) -> Vec<u8> {
        STANDARD.decode(v.as_str().expect("base64 string")).expect("base64")
    }

    fn crc_field(v: &Value) -> u32 {
        v.as_str().expect("int64 as string").parse().expect("crc number")
    }

    /// A Cloud KMS stand-in: the "ciphertext" is `[version] ‖ aad_len u32 ‖ aad ‖ plaintext`, so
    /// decrypt can enforce the AAD like KMS does. Checks the request checksums like KMS does.
    struct FakeKms {
        primary: Arc<AtomicU32>,
    }

    impl Respond for FakeKms {
        fn respond(&self, request: &Request) -> ResponseTemplate {
            let path = request.url.path();
            if request.method.as_str() == "GET" {
                let v = self.primary.load(Ordering::SeqCst);
                return ResponseTemplate::new(200).set_body_json(json!({
                    "name": KEY, "primary": { "name": format!("{KEY}/cryptoKeyVersions/{v}"), "state": "ENABLED" }
                }));
            }
            let body: Value = request.body_json().expect("json body");
            let aad = b64(&body["additionalAuthenticatedData"]);
            assert_eq!(crc_field(&body["additionalAuthenticatedDataCrc32c"]), crc32c(&aad), "aad checksum");
            if path.ends_with(":encrypt") {
                let plaintext = b64(&body["plaintext"]);
                assert_eq!(crc_field(&body["plaintextCrc32c"]), crc32c(&plaintext), "plaintext checksum");
                let v = self.primary.load(Ordering::SeqCst);
                let mut ct = vec![u8::try_from(v).expect("small version")];
                ct.extend_from_slice(&u32::try_from(aad.len()).expect("aad len").to_be_bytes());
                ct.extend_from_slice(&aad);
                ct.extend_from_slice(&plaintext);
                return ResponseTemplate::new(200).set_body_json(json!({
                    "name": format!("{KEY}/cryptoKeyVersions/{v}"),
                    "ciphertext": STANDARD.encode(&ct),
                    "ciphertextCrc32c": crc32c(&ct).to_string(),
                    "verifiedPlaintextCrc32c": true,
                    "verifiedAdditionalAuthenticatedDataCrc32c": true,
                    "protectionLevel": "SOFTWARE"
                }));
            }
            let ct = b64(&body["ciphertext"]);
            assert_eq!(crc_field(&body["ciphertextCrc32c"]), crc32c(&ct), "ciphertext checksum");
            let invalid = || {
                ResponseTemplate::new(400).set_body_json(json!({
                    "error": { "code": 400, "message": "Decryption failed: the ciphertext is invalid.", "status": "INVALID_ARGUMENT" }
                }))
            };
            if ct.len() < 5 {
                return invalid();
            }
            let aad_len = u32::from_be_bytes([ct[1], ct[2], ct[3], ct[4]]) as usize;
            if ct.len() < 5 + aad_len || ct[5..5 + aad_len] != aad[..] {
                return invalid();
            }
            let plaintext = &ct[5 + aad_len..];
            ResponseTemplate::new(200).set_body_json(json!({
                "plaintext": STANDARD.encode(plaintext),
                "plaintextCrc32c": crc32c(plaintext).to_string(),
                "usedPrimary": ct[0] as u32 == self.primary.load(Ordering::SeqCst),
                "protectionLevel": "SOFTWARE"
            }))
        }
    }

    async fn kms_server() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(TOKEN_PATH))
            .and(header("Metadata-Flavor", "Google"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "access_token": "kms-test-token", "expires_in": 3600, "token_type": "Bearer" })))
            .mount(&server)
            .await;
        server
    }

    async fn mount_fake_kms(server: &MockServer) -> Arc<AtomicU32> {
        let primary = Arc::new(AtomicU32::new(1));
        Mock::given(header("authorization", "Bearer kms-test-token"))
            .and(path(format!("/v1/{KEY}:encrypt")))
            .respond_with(FakeKms { primary: primary.clone() })
            .mount(server)
            .await;
        Mock::given(header("authorization", "Bearer kms-test-token"))
            .and(path(format!("/v1/{KEY}:decrypt")))
            .respond_with(FakeKms { primary: primary.clone() })
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(header("authorization", "Bearer kms-test-token"))
            .and(path(format!("/v1/{KEY}")))
            .respond_with(FakeKms { primary: primary.clone() })
            .mount(server)
            .await;
        primary
    }

    fn kek(server: &MockServer) -> GcpKmsKek {
        let base = Url::parse(&server.uri()).expect("server url");
        let tokens = AdcTokenSource::metadata_server(base.clone()).expect("metadata token source");
        GcpKmsKek::new(KEY, tokens).expect("kek").with_endpoint(&base)
    }

    #[tokio::test]
    async fn wrap_sends_aad_and_checksums_and_unwrap_round_trips() {
        let server = kms_server().await;
        let primary = mount_fake_kms(&server).await;
        let kek = kek(&server);
        let (wrapped, version) = kek.wrap(b"mage-secret/v1|platform|a/b|1", &[3u8; 32]).await.expect("wrap");
        assert_eq!(version, format!("{KEY}/cryptoKeyVersions/1"));
        let dek = kek.unwrap(b"mage-secret/v1|platform|a/b|1", &wrapped, &version).await.expect("unwrap");
        assert_eq!(dek.expose(), &[3u8; 32]);
        assert_eq!(kek.unwrap(b"mage-secret/v1|platform|a/b|2", &wrapped, &version).await.err(), Some(VaultError::Integrity));
        assert!(matches!(kek.unwrap(b"x", &wrapped, "local-v1").await, Err(VaultError::Invalid(_))));

        primary.store(4, Ordering::SeqCst);
        assert_eq!(kek.primary_version().await.expect("primary"), format!("{KEY}/cryptoKeyVersions/4"));
        let dek = kek.unwrap(b"mage-secret/v1|platform|a/b|1", &wrapped, &version).await.expect("old version still decrypts");
        assert_eq!(dek.expose(), &[3u8; 32]);

        let encrypt = server.received_requests().await.expect("recorded").into_iter().find(|r| r.url.path().ends_with(":encrypt")).expect("an encrypt call");
        let body: Value = encrypt.body_json().expect("json");
        assert_eq!(b64(&body["additionalAuthenticatedData"]), b"mage-secret/v1|platform|a/b|1");
        assert_eq!(crc_field(&body["plaintextCrc32c"]), crc32c(&[3u8; 32]));
    }

    #[tokio::test]
    async fn kms_errors_and_bad_checksums_map_to_vault_errors() {
        let cases: Vec<(&str, ResponseTemplate, VaultError)> = vec![
            (
                "403",
                ResponseTemplate::new(403).set_body_json(json!({"error": {"code": 403, "message": "Permission 'cloudkms.cryptoKeyVersions.useToEncrypt' denied", "status": "PERMISSION_DENIED"}})),
                VaultError::Forbidden,
            ),
            ("503", ResponseTemplate::new(503), VaultError::Unavailable(String::new())),
            (
                "bad ciphertext checksum",
                ResponseTemplate::new(200).set_body_json(json!({
                    "name": format!("{KEY}/cryptoKeyVersions/1"), "ciphertext": STANDARD.encode(b"ct"),
                    "ciphertextCrc32c": (crc32c(b"ct") ^ 1).to_string(),
                    "verifiedPlaintextCrc32c": true, "verifiedAdditionalAuthenticatedDataCrc32c": true
                })),
                VaultError::Unavailable(String::new()),
            ),
            (
                "request checksum not verified",
                ResponseTemplate::new(200).set_body_json(json!({
                    "name": format!("{KEY}/cryptoKeyVersions/1"), "ciphertext": STANDARD.encode(b"ct"),
                    "ciphertextCrc32c": crc32c(b"ct").to_string(),
                    "verifiedPlaintextCrc32c": false, "verifiedAdditionalAuthenticatedDataCrc32c": true
                })),
                VaultError::Unavailable(String::new()),
            ),
        ];
        for (what, response, expected) in cases {
            let server = kms_server().await;
            Mock::given(path(format!("/v1/{KEY}:encrypt"))).respond_with(response).mount(&server).await;
            let got = kek(&server).wrap(b"aad", &[1u8; 32]).await.err();
            match (&got, &expected) {
                (Some(VaultError::Unavailable(_)), VaultError::Unavailable(_)) => {}
                _ => assert_eq!(got.as_ref(), Some(&expected), "{what}"),
            }
        }

        let server = kms_server().await;
        Mock::given(path(format!("/v1/{KEY}:decrypt")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "plaintext": STANDARD.encode([1u8; 32]), "plaintextCrc32c": (crc32c(&[1u8; 32]) ^ 1).to_string()
            })))
            .mount(&server)
            .await;
        let version = format!("{KEY}/cryptoKeyVersions/1");
        assert!(matches!(kek(&server).unwrap(b"aad", b"ct", &version).await, Err(VaultError::Unavailable(_))), "bad plaintext checksum");

        let server = kms_server().await;
        Mock::given(path(format!("/v1/{KEY}:decrypt")))
            .respond_with(ResponseTemplate::new(403).set_body_json(json!({"error": {"code": 403, "message": "denied", "status": "PERMISSION_DENIED"}})))
            .mount(&server)
            .await;
        assert_eq!(kek(&server).unwrap(b"aad", b"ct", &version).await.err(), Some(VaultError::Forbidden));
    }

    /// The whole envelope (secrets, data keys, KEK rotation + rewrap) over the KMS adapter.
    #[tokio::test]
    async fn envelope_vault_over_the_kms_adapter_passes_the_vault_contract() {
        let server = Arc::new(kms_server().await);
        let primary = mount_fake_kms(&server).await;
        contract_secret_vault(|clock| {
            let server = server.clone();
            let primary = primary.clone();
            async move {
                primary.store(1, Ordering::SeqCst);
                let store = Arc::new(MemSecretStore::new(clock.clone()));
                let vault = Arc::new(EnvelopeVault::new(store.clone(), kek(&server), clock));
                let rotate = move || {
                    primary.fetch_add(1, Ordering::SeqCst);
                };
                VaultFixture { vault, store, rotate_kek: Some(Box::new(rotate)) }
            }
        })
        .await;
    }

    /// Opt-in: a real Cloud KMS key (`MAGE_TEST_KMS_KEY`) with ADC.
    #[tokio::test]
    async fn real_cloud_kms_round_trip_and_aad_binding() {
        let Some(key) = std::env::var("MAGE_TEST_KMS_KEY").ok().filter(|k| !k.is_empty()) else {
            eprintln!("skipping real_cloud_kms_round_trip_and_aad_binding: MAGE_TEST_KMS_KEY is not set");
            return;
        };
        let kek = GcpKmsKek::new(key.clone(), AdcTokenSource::from_environment().expect("ADC")).expect("kek");
        let (wrapped, version) = kek.wrap(b"mage-test/v1|aad", &[5u8; 32]).await.expect("wrap");
        assert!(version.starts_with(&format!("{key}/cryptoKeyVersions/")), "{version}");
        assert_eq!(kek.unwrap(b"mage-test/v1|aad", &wrapped, &version).await.expect("unwrap").expose(), &[5u8; 32]);
        assert_eq!(kek.unwrap(b"mage-test/v1|other", &wrapped, &version).await.err(), Some(VaultError::Integrity));
        assert!(kek.primary_version().await.expect("primary").starts_with(&format!("{key}/cryptoKeyVersions/")));

        let kek = Arc::new(kek);
        contract_secret_vault(|clock| {
            let kek = kek.clone();
            async move {
                let store = Arc::new(MemSecretStore::new(clock.clone()));
                let vault = Arc::new(EnvelopeVault::new(store.clone(), kek, clock));
                VaultFixture { vault, store, rotate_kek: None }
            }
        })
        .await;
    }
}
