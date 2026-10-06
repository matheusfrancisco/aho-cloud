//! Local (dev) backend: HMAC-signed URLs and the axum routes that redeem them (storage.md
//! "Backends": `{server}/_dev/blob/{key}?m=&exp=&ct=&max=&sig=`, HMAC-SHA256 over method, key,
//! exp, content type, max). Local filesystems cannot sign URLs, so dev runners upload and download
//! through the server; `mage_server::compose` mounts [`dev_routes`] only when the blob backend is
//! `local`.
//!
//! Redemption mirrors GCS: the signature, method and expiry must hold (403), an upload must carry
//! exactly the signed `content-type` (403) and at most `max` bytes (413).

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path as AxPath, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use hmac::{Hmac, Mac};
use mage_core::ports::blob::{ArtifactStore, BlobError, BlobKey, HttpMethod, PutOptions};
use serde::Deserialize;
use sha2::Sha256;
use time::OffsetDateTime;
use url::Url;

use super::{ObjectBlobs, Signer};

/// Path prefix of the dev blob routes.
pub const DEV_BLOB_PREFIX: &str = "/_dev/blob";

type HmacSha256 = Hmac<Sha256>;

/// Signs and verifies dev blob URLs.
#[derive(Clone)]
pub struct DevSigner {
    /// HMAC-SHA256 keyed once; cloned per signature.
    mac: HmacSha256,
    /// `{public_url}/_dev/blob/`
    base: String,
}

impl std::fmt::Debug for DevSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DevSigner").field("base", &self.base).finish_non_exhaustive()
    }
}

/// Why a dev URL was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DevUrlError {
    #[error("signature does not match")]
    BadSignature,
    #[error("signed URL expired")]
    Expired,
    #[error("method does not match the signed method")]
    WrongMethod,
}

/// What a verified URL allows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevGrant {
    pub method: HttpMethod,
    /// Uploads: the signed content type and size cap.
    pub upload: Option<(String, u64)>,
}

/// Query of a dev URL.
#[derive(Debug, Clone, Deserialize)]
pub struct DevQuery {
    pub m: String,
    pub exp: i64,
    #[serde(default)]
    pub ct: Option<String>,
    #[serde(default)]
    pub max: Option<u64>,
    pub sig: String,
}

fn method_str(method: HttpMethod) -> &'static str {
    match method {
        HttpMethod::Get => "GET",
        HttpMethod::Put => "PUT",
    }
}

impl DevSigner {
    /// `key` must be at least 16 bytes.
    pub fn new(public_url: &Url, key: &[u8]) -> Result<Self, BlobError> {
        if key.len() < 16 {
            return Err(BlobError::Invalid("dev blob signing key must be at least 16 bytes".into()));
        }
        let mac = <HmacSha256 as Mac>::new_from_slice(key).map_err(|e| BlobError::Invalid(format!("dev blob signing key: {e}")))?;
        Ok(Self { mac, base: format!("{}{DEV_BLOB_PREFIX}/", public_url.as_str().trim_end_matches('/')) })
    }

    /// A random 256-bit key: URLs stay valid for the life of this process.
    pub fn random(public_url: &Url) -> Result<Self, BlobError> {
        let mut key = uuid::Uuid::new_v4().as_bytes().to_vec();
        key.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
        Self::new(public_url, &key)
    }

    fn mac(&self, method: &str, key: &str, exp: i64, upload: Option<(&str, u64)>) -> HmacSha256 {
        let (ct, max) = upload.map(|(ct, max)| (ct.to_owned(), max.to_string())).unwrap_or_default();
        let mut mac = self.mac.clone();
        mac.update(format!("{method}\n{key}\n{exp}\n{ct}\n{max}").as_bytes());
        mac
    }

    /// The URL for `method` on `key`, valid until `expires_at`.
    pub fn sign(&self, key: &BlobKey, method: HttpMethod, upload: Option<(&str, u64)>, expires_at: OffsetDateTime) -> Result<String, BlobError> {
        let exp = expires_at.unix_timestamp();
        let m = method_str(method);
        let sig = hex::encode(self.mac(m, key.as_str(), exp, upload).finalize().into_bytes());
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        query.append_pair("m", m).append_pair("exp", &exp.to_string());
        if let Some((ct, max)) = upload {
            query.append_pair("ct", ct).append_pair("max", &max.to_string());
        }
        query.append_pair("sig", &sig);
        Ok(format!("{}{key}?{}", self.base, query.finish()))
    }

    /// Check a redeemed URL: signature (constant time), method, expiry.
    pub fn verify(&self, method: HttpMethod, key: &BlobKey, query: &DevQuery, now: OffsetDateTime) -> Result<DevGrant, DevUrlError> {
        let upload = match (&query.ct, query.max) {
            (Some(ct), Some(max)) => Some((ct.as_str(), max)),
            (None, None) => None,
            _ => return Err(DevUrlError::BadSignature),
        };
        let sig = hex::decode(&query.sig).map_err(|_| DevUrlError::BadSignature)?;
        self.mac(&query.m, key.as_str(), query.exp, upload).verify_slice(&sig).map_err(|_| DevUrlError::BadSignature)?;
        if query.m != method_str(method) {
            return Err(DevUrlError::WrongMethod);
        }
        if now.unix_timestamp() >= query.exp {
            return Err(DevUrlError::Expired);
        }
        Ok(DevGrant { method, upload: upload.map(|(ct, max)| (ct.to_owned(), max)) })
    }
}

/// `GET|PUT /_dev/blob/{*key}` with the state applied. Errors unless `blobs` signs with the dev
/// HMAC signer (local / in-memory backends).
pub fn dev_routes(blobs: Arc<ObjectBlobs>) -> Result<Router, BlobError> {
    if !matches!(blobs.signer(), Signer::DevHmac(_)) {
        return Err(BlobError::Invalid("dev blob routes need the local backend's HMAC signer".into()));
    }
    Ok(Router::new()
        .route(&format!("{DEV_BLOB_PREFIX}/{{*key}}"), get(download).put(upload))
        .layer(DefaultBodyLimit::disable())
        .with_state(blobs))
}

fn refuse(status: StatusCode, message: &str) -> Response {
    (status, message.to_owned()).into_response()
}

fn blob_error(err: BlobError) -> Response {
    match err {
        BlobError::NotFound => refuse(StatusCode::NOT_FOUND, "object not found"),
        BlobError::Invalid(msg) => refuse(StatusCode::BAD_REQUEST, &msg),
        BlobError::TooLarge { .. } => refuse(StatusCode::PAYLOAD_TOO_LARGE, "body larger than the signed maximum"),
        BlobError::Conflict => refuse(StatusCode::CONFLICT, "object conflict"),
        BlobError::Unavailable(msg) => refuse(StatusCode::SERVICE_UNAVAILABLE, &msg),
    }
}

fn verified(blobs: &ObjectBlobs, method: HttpMethod, key: &str, query: &DevQuery) -> Result<(BlobKey, DevGrant), Response> {
    let Signer::DevHmac(signer) = blobs.signer() else {
        return Err(refuse(StatusCode::NOT_FOUND, "dev blob routes are disabled"));
    };
    let key = BlobKey::parse(key).map_err(blob_error)?;
    let grant = signer.verify(method, &key, query, blobs.clock().now()).map_err(|e| refuse(StatusCode::FORBIDDEN, &e.to_string()))?;
    Ok((key, grant))
}

async fn download(State(blobs): State<Arc<ObjectBlobs>>, AxPath(key): AxPath<String>, Query(query): Query<DevQuery>) -> Response {
    let (key, _) = match verified(&blobs, HttpMethod::Get, &key, &query) {
        Ok(ok) => ok,
        Err(response) => return response,
    };
    let meta = match blobs.head(&key).await {
        Ok(meta) => meta,
        Err(e) => return blob_error(e),
    };
    match blobs.get(&key, meta.bytes).await {
        Ok(body) => {
            let mut response = Body::from(body).into_response();
            let content_type = meta.content_type.as_deref().unwrap_or("application/octet-stream");
            if let Ok(value) = HeaderValue::from_str(content_type) {
                response.headers_mut().insert(header::CONTENT_TYPE, value);
            }
            response
        }
        Err(e) => blob_error(e),
    }
}

async fn upload(
    State(blobs): State<Arc<ObjectBlobs>>,
    AxPath(key): AxPath<String>,
    Query(query): Query<DevQuery>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let (key, grant) = match verified(&blobs, HttpMethod::Put, &key, &query) {
        Ok(ok) => ok,
        Err(response) => return response,
    };
    let Some((content_type, max_bytes)) = grant.upload else {
        return refuse(StatusCode::FORBIDDEN, "not an upload URL");
    };
    let sent: Vec<&str> = headers.get_all(header::CONTENT_TYPE).iter().filter_map(|v| v.to_str().ok()).collect();
    if sent != [content_type.as_str()] {
        return refuse(StatusCode::FORBIDDEN, "content-type does not match the signed content type");
    }
    let declared = headers.get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|n| n > max_bytes) {
        return refuse(StatusCode::PAYLOAD_TOO_LARGE, "body larger than the signed maximum");
    }
    let limit = usize::try_from(max_bytes).unwrap_or(usize::MAX);
    let bytes = match axum::body::to_bytes(body, limit).await {
        Ok(bytes) => bytes,
        Err(_) => return refuse(StatusCode::PAYLOAD_TOO_LARGE, "body larger than the signed maximum"),
    };
    match blobs.put(&key, bytes.to_vec(), &PutOptions { content_type, if_absent: false }).await {
        Ok(_) => StatusCode::OK.into_response(),
        Err(e) => blob_error(e),
    }
}

/// The query pairs of a dev URL (tests and tools redeeming URLs without HTTP).
pub fn query_of(url: &str) -> BTreeMap<String, String> {
    Url::parse(url).map(|u| u.query_pairs().into_owned().collect()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn signer() -> DevSigner {
        DevSigner::new(&Url::parse("http://localhost:8080/").unwrap(), &[7u8; 32]).unwrap()
    }

    fn query(url: &str) -> DevQuery {
        let q = query_of(url);
        DevQuery {
            m: q["m"].clone(),
            exp: q["exp"].parse().unwrap(),
            ct: q.get("ct").cloned(),
            max: q.get("max").map(|m| m.parse().unwrap()),
            sig: q["sig"].clone(),
        }
    }

    #[test]
    fn urls_verify_and_every_signed_field_matters() {
        let s = signer();
        let key = BlobKey::parse("org/a/runs/b/video/attempt-1.webm").unwrap();
        let now = OffsetDateTime::from_unix_timestamp(1_767_225_600).unwrap();
        let exp = now + std::time::Duration::from_secs(900);
        let url = s.sign(&key, HttpMethod::Put, Some(("video/webm", 100)), exp).unwrap();
        assert!(url.starts_with("http://localhost:8080/_dev/blob/org/a/runs/b/video/attempt-1.webm?m=PUT&exp="), "{url}");
        let q = query(&url);
        assert_eq!(s.verify(HttpMethod::Put, &key, &q, now), Ok(DevGrant { method: HttpMethod::Put, upload: Some(("video/webm".into(), 100)) }));
        assert_eq!(s.verify(HttpMethod::Get, &key, &q, now), Err(DevUrlError::WrongMethod));
        assert_eq!(s.verify(HttpMethod::Put, &key, &q, exp), Err(DevUrlError::Expired));
        let other = BlobKey::parse("org/a/runs/b/video/attempt-2.webm").unwrap();
        assert_eq!(s.verify(HttpMethod::Put, &other, &q, now), Err(DevUrlError::BadSignature), "per key");
        for tamper in [
            DevQuery { max: Some(101), ..q.clone() },
            DevQuery { ct: Some("text/html".into()), ..q.clone() },
            DevQuery { exp: q.exp + 1, ..q.clone() },
            DevQuery { m: "GET".into(), ..q.clone() },
            DevQuery { sig: "00".repeat(32), ..q.clone() },
            DevQuery { ct: None, ..q.clone() },
        ] {
            assert_eq!(s.verify(HttpMethod::Put, &key, &tamper, now).err().map(|_| ()), Some(()), "{tamper:?}");
        }
        let foreign = DevSigner::new(&Url::parse("http://localhost:8080").unwrap(), &[8u8; 32]).unwrap();
        assert_eq!(foreign.verify(HttpMethod::Put, &key, &q, now), Err(DevUrlError::BadSignature), "another key");
        assert!(DevSigner::new(&Url::parse("http://x/").unwrap(), b"short").is_err());
    }
}
