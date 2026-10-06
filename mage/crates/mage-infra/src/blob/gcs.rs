//! GCS V4 signed URLs with extra signed headers (storage.md "Signed URL policy").
//!
//! `object_store`'s GCS signer signs only `host`, so an upload URL could not pin the content type
//! or the size range. This module implements the documented V4 algorithm
//! (<https://cloud.google.com/storage/docs/access-control/signing-urls-manually>): canonical
//! request over `content-type`, `host` and `x-goog-content-length-range`, `GOOG4-RSA-SHA256`
//! string to sign, signature from IAM `signBlob` with the store's own credentials (Cloud Run has
//! no key file; the service account needs `roles/iam.serviceAccountTokenCreator` on itself).
//! GCS rejects a PUT whose body is outside the signed range or whose content type differs.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use mage_core::ports::blob::{BlobError, BlobKey, HttpMethod};
use object_store::gcp::GoogleCloudStorage;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use time::macros::format_description;

use super::store_error;

/// Signed header carrying the allowed body size range (`0,{max}`).
pub const CONTENT_LENGTH_RANGE: &str = "x-goog-content-length-range";

/// Host of path-style signed URLs.
pub const GCS_HOST: &str = "storage.googleapis.com";

const ALGORITHM: &str = "GOOG4-RSA-SHA256";

/// Signs V4 URLs for one bucket through IAM `signBlob`.
pub struct GcsV4Signer {
    store: Arc<GoogleCloudStorage>,
    bucket: String,
    http: reqwest::Client,
    iam_base: String,
}

#[derive(Serialize)]
struct SignBlobRequest {
    payload: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SignBlobResponse {
    signed_blob: String,
}

impl GcsV4Signer {
    pub fn new(store: Arc<GoogleCloudStorage>, bucket: &str) -> Self {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(10)).build().unwrap_or_default();
        Self { store, bucket: bucket.to_owned(), http, iam_base: "https://iamcredentials.googleapis.com".to_owned() }
    }

    /// A V4 URL for `method` on `key`; `headers` (name, value) are signed in addition to `host` and
    /// must be sent exactly by the client.
    pub async fn sign(&self, method: HttpMethod, key: &BlobKey, headers: &[(String, String)], now: OffsetDateTime, ttl: Duration) -> Result<String, BlobError> {
        let signing = self.store.signing_credentials().get_credential().await.map_err(store_error)?;
        let request = V4Request { method, bucket: &self.bucket, key: key.as_str(), email: &signing.email, headers, now, ttl };
        let to_sign = request.string_to_sign();
        let token = self.store.credentials().get_credential().await.map_err(store_error)?;
        let response = self
            .http
            .post(format!("{}/v1/projects/-/serviceAccounts/{}:signBlob", self.iam_base, signing.email))
            .bearer_auth(&token.bearer)
            .json(&SignBlobRequest { payload: base64::engine::general_purpose::STANDARD.encode(to_sign.as_bytes()) })
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| BlobError::Unavailable(format!("signBlob: {e}")))?
            .json::<SignBlobResponse>()
            .await
            .map_err(|e| BlobError::Unavailable(format!("signBlob response: {e}")))?;
        let signature = base64::engine::general_purpose::STANDARD
            .decode(response.signed_blob)
            .map_err(|e| BlobError::Unavailable(format!("signBlob signature: {e}")))?;
        Ok(request.url(&hex::encode(signature)))
    }
}

/// One V4 signing request (pure; the signature comes from outside).
pub struct V4Request<'a> {
    pub method: HttpMethod,
    pub bucket: &'a str,
    pub key: &'a str,
    /// Service account email (the credential's subject).
    pub email: &'a str,
    /// Extra signed headers besides `host`.
    pub headers: &'a [(String, String)],
    pub now: OffsetDateTime,
    pub ttl: Duration,
}

/// RFC 3986 percent-encoding of everything but unreserved characters (and `/` when `keep_slash`).
fn encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') || (keep_slash && b == b'/') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

impl V4Request<'_> {
    fn datetime(&self) -> String {
        self.now.format(format_description!("[year][month][day]T[hour][minute][second]Z")).unwrap_or_default()
    }

    fn date(&self) -> String {
        self.now.format(format_description!("[year][month][day]")).unwrap_or_default()
    }

    fn scope(&self) -> String {
        format!("{}/auto/storage/goog4_request", self.date())
    }

    fn signed_headers(&self) -> BTreeMap<String, String> {
        let mut headers: BTreeMap<String, String> = self.headers.iter().map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned())).collect();
        headers.insert("host".to_owned(), GCS_HOST.to_owned());
        headers
    }

    pub fn path(&self) -> String {
        format!("/{}/{}", encode(self.bucket, false), encode(self.key, true))
    }

    /// Canonical query string (without `X-Goog-Signature`), sorted by name.
    pub fn query(&self) -> String {
        let signed: Vec<String> = self.signed_headers().into_keys().collect();
        let mut pairs = [
            ("X-Goog-Algorithm", ALGORITHM.to_owned()),
            ("X-Goog-Credential", format!("{}/{}", self.email, self.scope())),
            ("X-Goog-Date", self.datetime()),
            ("X-Goog-Expires", self.ttl.as_secs().to_string()),
            ("X-Goog-SignedHeaders", signed.join(";")),
        ];
        pairs.sort_by(|a, b| a.0.cmp(b.0));
        pairs.iter().map(|(k, v)| format!("{}={}", encode(k, false), encode(v, false))).collect::<Vec<_>>().join("&")
    }

    pub fn canonical_request(&self) -> String {
        let headers = self.signed_headers();
        let canonical_headers: Vec<String> = headers.iter().map(|(k, v)| format!("{k}:{v}")).collect();
        let signed: Vec<&str> = headers.keys().map(String::as_str).collect();
        let verb = match self.method {
            HttpMethod::Get => "GET",
            HttpMethod::Put => "PUT",
        };
        format!("{verb}\n{}\n{}\n{}\n\n{}\nUNSIGNED-PAYLOAD", self.path(), self.query(), canonical_headers.join("\n"), signed.join(";"))
    }

    pub fn string_to_sign(&self) -> String {
        let digest = hex::encode(Sha256::digest(self.canonical_request().as_bytes()));
        format!("{ALGORITHM}\n{}\n{}\n{digest}", self.datetime(), self.scope())
    }

    pub fn url(&self, signature_hex: &str) -> String {
        format!("https://{GCS_HOST}{}?{}&X-Goog-Signature={signature_hex}", self.path(), self.query())
    }
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;

    #[test]
    fn canonical_request_matches_the_documented_shape() {
        // Google's example (signing-urls-manually): GET of cat-pics/tabby.jpeg, host only.
        let request = V4Request {
            method: HttpMethod::Get,
            bucket: "example-bucket",
            key: "cat-pics/tabby.jpeg",
            email: "example@example-project.iam.gserviceaccount.com",
            headers: &[],
            now: datetime!(2019-03-01 19:08:59 UTC),
            ttl: Duration::from_secs(3600),
        };
        assert_eq!(
            request.canonical_request(),
            "GET\n/example-bucket/cat-pics/tabby.jpeg\n\
             X-Goog-Algorithm=GOOG4-RSA-SHA256&X-Goog-Credential=example%40example-project.iam.gserviceaccount.com%2F20190301%2Fauto%2Fstorage%2Fgoog4_request&X-Goog-Date=20190301T190859Z&X-Goog-Expires=3600&X-Goog-SignedHeaders=host\n\
             host:storage.googleapis.com\n\nhost\nUNSIGNED-PAYLOAD"
        );
        let to_sign = request.string_to_sign();
        assert!(to_sign.starts_with("GOOG4-RSA-SHA256\n20190301T190859Z\n20190301/auto/storage/goog4_request\n"), "{to_sign}");
        assert_eq!(to_sign.lines().last().map(str::len), Some(64), "hex sha256 of the canonical request");
    }

    #[test]
    fn uploads_sign_content_type_and_length_range() {
        let headers = [("Content-Type".to_owned(), "video/webm".to_owned()), (CONTENT_LENGTH_RANGE.to_owned(), "0,536870912".to_owned())];
        let request = V4Request {
            method: HttpMethod::Put,
            bucket: "mage-prod-data",
            key: "org/0190f8a1-2b3c-7d4e-8f00-112233445566/runs/r/video/attempt-1.webm",
            email: "mage-server@p.iam.gserviceaccount.com",
            headers: &headers,
            now: datetime!(2026-10-05 12:00:00 UTC),
            ttl: Duration::from_secs(900),
        };
        let canonical = request.canonical_request();
        assert!(canonical.starts_with("PUT\n/mage-prod-data/org/0190f8a1-2b3c-7d4e-8f00-112233445566/runs/r/video/attempt-1.webm\n"));
        assert!(canonical.contains("X-Goog-SignedHeaders=content-type%3Bhost%3Bx-goog-content-length-range"));
        assert!(canonical.ends_with(
            "content-type:video/webm\nhost:storage.googleapis.com\nx-goog-content-length-range:0,536870912\n\n\
             content-type;host;x-goog-content-length-range\nUNSIGNED-PAYLOAD"
        ));
        let url = request.url("abcd");
        assert!(url.starts_with("https://storage.googleapis.com/mage-prod-data/org/"));
        assert!(url.ends_with("&X-Goog-Signature=abcd") && url.contains("X-Goog-Expires=900"));
    }
}
