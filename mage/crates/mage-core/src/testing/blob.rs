//! `MemArtifactStore`: in-memory `ArtifactStore` (storage.md "`ArtifactStore`"). Signed URLs are
//! `mem://blob/{key}?sig={n}` grants that tests redeem with [`MemArtifactStore::upload_signed`] /
//! [`MemArtifactStore::download_signed`], which enforce what GCS enforces (signature, expiry,
//! method, signed content type, size range).

use std::collections::BTreeMap;
use std::fs;
use std::io::{ErrorKind, Write};
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

use crate::ports::blob::{
    ArtifactStore, BlobError, BlobKey, DownloadPolicy, HttpMethod, ObjectMeta, PutOptions, PutReceipt, SignedUrl, UploadPolicy,
    check_signed_ttl, clamp_range,
};
use crate::ports::clock::Clock;
use crate::ports::common::{Cursor, Page, PageRequest};
use crate::testing::page::keyset_page;

const URL_PREFIX: &str = "mem://blob/";

pub struct MemArtifactStore {
    clock: Arc<dyn Clock>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    objects: BTreeMap<BlobKey, Object>,
    grants: BTreeMap<u64, Grant>,
    next_grant: u64,
}

struct Object {
    bytes: Vec<u8>,
    content_type: String,
    updated_at: OffsetDateTime,
}

struct Grant {
    key: BlobKey,
    method: HttpMethod,
    /// Upload only: the signed content type and size ceiling.
    upload: Option<(String, u64)>,
    expires_at: OffsetDateTime,
}

/// Why a signed URL was refused (what the object store's HTTP endpoint would answer).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SignedUseError {
    #[error("signature does not match")]
    BadSignature,
    #[error("signed URL expired")]
    Expired,
    #[error("signed for another method")]
    WrongMethod,
    #[error("content type differs from the signed one")]
    ContentTypeMismatch,
    #[error("body larger than the signed limit {limit}")]
    TooLarge { limit: u64 },
    #[error("object not found")]
    NotFound,
}

fn io_error(path: &Path, err: &std::io::Error) -> BlobError {
    if err.kind() == ErrorKind::NotFound {
        BlobError::Invalid(format!("{}: {err}", path.display()))
    } else {
        BlobError::Unavailable(format!("{}: {err}", path.display()))
    }
}

impl MemArtifactStore {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self { clock, state: Mutex::new(State::default()) }
    }

    /// Every stored key, in order (test inspection).
    pub fn keys(&self) -> Vec<BlobKey> {
        self.state.lock().objects.keys().cloned().collect()
    }

    /// A client `PUT` through a signed upload URL with the given headers and body.
    pub fn upload_signed(&self, url: &str, headers: &[(String, String)], body: Vec<u8>) -> Result<(), SignedUseError> {
        let now = self.clock.now();
        let mut state = self.state.lock();
        let grant = Self::grant(&state, url, now, HttpMethod::Put)?;
        let (content_type, max_bytes) = grant.upload.clone().ok_or(SignedUseError::WrongMethod)?;
        let sent = headers.iter().find(|(name, _)| name.eq_ignore_ascii_case("content-type")).map(|(_, value)| value.as_str());
        if sent != Some(content_type.as_str()) {
            return Err(SignedUseError::ContentTypeMismatch);
        }
        if body.len() as u64 > max_bytes {
            return Err(SignedUseError::TooLarge { limit: max_bytes });
        }
        let key = grant.key.clone();
        state.objects.insert(key, Object { bytes: body, content_type, updated_at: now });
        Ok(())
    }

    /// A client `GET` through a signed download URL.
    pub fn download_signed(&self, url: &str) -> Result<Vec<u8>, SignedUseError> {
        let now = self.clock.now();
        let state = self.state.lock();
        let grant = Self::grant(&state, url, now, HttpMethod::Get)?;
        state.objects.get(&grant.key).map(|object| object.bytes.clone()).ok_or(SignedUseError::NotFound)
    }

    fn grant<'a>(state: &'a State, url: &str, now: OffsetDateTime, method: HttpMethod) -> Result<&'a Grant, SignedUseError> {
        let rest = url.strip_prefix(URL_PREFIX).ok_or(SignedUseError::BadSignature)?;
        let (key, sig) = rest.split_once("?sig=").ok_or(SignedUseError::BadSignature)?;
        let grant = sig.parse::<u64>().ok().and_then(|n| state.grants.get(&n)).ok_or(SignedUseError::BadSignature)?;
        if grant.key.as_str() != key {
            return Err(SignedUseError::BadSignature);
        }
        if now >= grant.expires_at {
            return Err(SignedUseError::Expired);
        }
        if grant.method != method {
            return Err(SignedUseError::WrongMethod);
        }
        Ok(grant)
    }

    fn sign(&self, key: &BlobKey, method: HttpMethod, upload: Option<(String, u64)>, ttl: std::time::Duration) -> SignedUrl {
        let expires_at = self.clock.now() + ttl;
        let mut state = self.state.lock();
        state.next_grant += 1;
        let n = state.next_grant;
        let headers = upload.as_ref().map(|(content_type, _)| vec![("content-type".to_owned(), content_type.clone())]).unwrap_or_default();
        state.grants.insert(n, Grant { key: key.clone(), method, upload, expires_at });
        SignedUrl { method, url: format!("{URL_PREFIX}{key}?sig={n}"), headers, expires_at }
    }

    fn store(&self, key: &BlobKey, body: Vec<u8>, opts: &PutOptions) -> PutReceipt {
        let receipt = PutReceipt { bytes: body.len() as u64, sha256: Sha256::digest(&body).into(), created: true };
        let now = self.clock.now();
        let mut state = self.state.lock();
        if opts.if_absent && state.objects.contains_key(key) {
            return PutReceipt { created: false, ..receipt };
        }
        state.objects.insert(key.clone(), Object { bytes: body, content_type: opts.content_type.clone(), updated_at: now });
        receipt
    }
}

#[async_trait]
impl ArtifactStore for MemArtifactStore {
    async fn put(&self, key: &BlobKey, body: Vec<u8>, opts: &PutOptions) -> Result<PutReceipt, BlobError> {
        Ok(self.store(key, body, opts))
    }

    async fn put_file(&self, key: &BlobKey, src: &Path, opts: &PutOptions) -> Result<PutReceipt, BlobError> {
        let body = fs::read(src).map_err(|e| io_error(src, &e))?;
        Ok(self.store(key, body, opts))
    }

    async fn get(&self, key: &BlobKey, max_bytes: u64) -> Result<Vec<u8>, BlobError> {
        let state = self.state.lock();
        let object = state.objects.get(key).ok_or(BlobError::NotFound)?;
        if object.bytes.len() as u64 > max_bytes {
            return Err(BlobError::TooLarge { limit: max_bytes });
        }
        Ok(object.bytes.clone())
    }

    async fn get_range(&self, key: &BlobKey, range: Range<u64>) -> Result<Vec<u8>, BlobError> {
        let state = self.state.lock();
        let object = state.objects.get(key).ok_or(BlobError::NotFound)?;
        let range = clamp_range(&range, object.bytes.len() as u64)?;
        let (start, end) = (usize::try_from(range.start), usize::try_from(range.end));
        match (start, end) {
            (Ok(start), Ok(end)) => Ok(object.bytes[start..end].to_vec()),
            _ => Err(BlobError::Invalid("range beyond addressable memory".into())),
        }
    }

    async fn get_to_file(&self, key: &BlobKey, dst: &Path) -> Result<u64, BlobError> {
        let bytes = {
            let state = self.state.lock();
            state.objects.get(key).ok_or(BlobError::NotFound)?.bytes.clone()
        };
        let mut tmp = dst.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = Path::new(&tmp);
        let mut file = fs::File::create(tmp).map_err(|e| io_error(tmp, &e))?;
        file.write_all(&bytes).and_then(|()| file.sync_all()).map_err(|e| io_error(tmp, &e))?;
        fs::rename(tmp, dst).map_err(|e| io_error(dst, &e))?;
        Ok(bytes.len() as u64)
    }

    async fn head(&self, key: &BlobKey) -> Result<ObjectMeta, BlobError> {
        let state = self.state.lock();
        let object = state.objects.get(key).ok_or(BlobError::NotFound)?;
        Ok(ObjectMeta { bytes: object.bytes.len() as u64, content_type: Some(object.content_type.clone()), updated_at: object.updated_at })
    }

    async fn delete(&self, key: &BlobKey) -> Result<(), BlobError> {
        self.state.lock().objects.remove(key);
        Ok(())
    }

    async fn delete_prefix(&self, prefix: &BlobKey) -> Result<u64, BlobError> {
        let mut state = self.state.lock();
        let before = state.objects.len();
        state.objects.retain(|key, _| !key.is_within(prefix));
        Ok((before - state.objects.len()) as u64)
    }

    async fn list_prefix(&self, prefix: &BlobKey, page: PageRequest) -> Result<Page<(BlobKey, ObjectMeta)>, BlobError> {
        let after = page.after.as_ref().map(|Cursor(key)| BlobKey::parse(key)).transpose()?;
        let state = self.state.lock();
        let items: Vec<(BlobKey, (BlobKey, ObjectMeta))> = state
            .objects
            .iter()
            .filter(|(key, _)| key.is_within(prefix))
            .map(|(key, object)| {
                let meta = ObjectMeta { bytes: object.bytes.len() as u64, content_type: Some(object.content_type.clone()), updated_at: object.updated_at };
                (key.clone(), (key.clone(), meta))
            })
            .collect();
        let (items, next) = keyset_page(items, after.as_ref(), page.effective_limit(), |key| key.as_str().to_owned());
        Ok(Page { items, next })
    }

    async fn signed_upload(&self, key: &BlobKey, policy: &UploadPolicy) -> Result<SignedUrl, BlobError> {
        check_signed_ttl(policy.ttl)?;
        if policy.content_type.is_empty() {
            return Err(BlobError::Invalid("signed upload needs a content type".into()));
        }
        Ok(self.sign(key, HttpMethod::Put, Some((policy.content_type.clone(), policy.max_bytes)), policy.ttl))
    }

    async fn signed_download(&self, key: &BlobKey, policy: &DownloadPolicy) -> Result<SignedUrl, BlobError> {
        check_signed_ttl(policy.ttl)?;
        Ok(self.sign(key, HttpMethod::Get, None, policy.ttl))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::testing::{FakeClock, contract_artifact_store};

    fn key(s: &str) -> BlobKey {
        BlobKey::parse(s).unwrap()
    }

    #[tokio::test]
    async fn mem_artifact_store_passes_the_contract() {
        let scratch = tempfile::tempdir().unwrap();
        contract_artifact_store(|clock| async move { Arc::new(MemArtifactStore::new(clock)) as Arc<dyn ArtifactStore> }, scratch.path()).await;
    }

    #[tokio::test]
    async fn signed_urls_round_trip_and_reject_tampering_expiry_type_and_size() {
        let clock = Arc::new(FakeClock::new());
        let store = MemArtifactStore::new(clock.clone());
        let k = key("org/a/runs/b/video/attempt-1.webm");
        let policy = UploadPolicy { content_type: "video/webm".into(), max_bytes: 4, ttl: Duration::from_secs(900) };
        let up = store.signed_upload(&k, &policy).await.unwrap();
        let ct = vec![("Content-Type".to_owned(), "video/webm".to_owned())];

        let tampered = up.url.replace("attempt-1", "attempt-2");
        assert_eq!(store.upload_signed(&tampered, &ct, b"ok".to_vec()), Err(SignedUseError::BadSignature));
        let forged = format!("{}9", up.url);
        assert_eq!(store.upload_signed(&forged, &ct, b"ok".to_vec()), Err(SignedUseError::BadSignature));
        let wrong_type = vec![("content-type".to_owned(), "text/plain".to_owned())];
        assert_eq!(store.upload_signed(&up.url, &wrong_type, b"ok".to_vec()), Err(SignedUseError::ContentTypeMismatch));
        assert_eq!(store.upload_signed(&up.url, &ct, b"too big".to_vec()), Err(SignedUseError::TooLarge { limit: 4 }));
        assert_eq!(store.download_signed(&up.url), Err(SignedUseError::WrongMethod));
        store.upload_signed(&up.url, &ct, b"webm".to_vec()).unwrap();
        assert_eq!(store.head(&k).await.unwrap().content_type.as_deref(), Some("video/webm"));

        let down = store.signed_download(&k, &DownloadPolicy { ttl: Duration::from_secs(60) }).await.unwrap();
        assert_eq!(store.download_signed(&down.url), Ok(b"webm".to_vec()));
        clock.advance(Duration::from_secs(60));
        assert_eq!(store.download_signed(&down.url), Err(SignedUseError::Expired));
        assert_eq!(store.upload_signed(&up.url, &ct, b"late".to_vec()), Ok(()), "the upload grant lives 15 min");
        clock.advance(Duration::from_secs(900));
        assert_eq!(store.upload_signed(&up.url, &ct, b"late".to_vec()), Err(SignedUseError::Expired));
    }
}
