//! Blob store: `ArtifactStore` over `object_store` (storage.md "Blob layout and signed URLs",
//! "Adapter internals" → `mage_infra::blob`).
//!
//! | Backend | Store | Object metadata | Signed URLs |
//! |---|---|---|---|
//! | `gcs` (prod) | `GoogleCloudStorage` | object attributes | GCS V4 with signed `content-type` and `x-goog-content-length-range` ([`gcs`]) |
//! | `s3` | `AmazonS3` | object attributes | `object_store` SigV4 presign (host only; size and type re-checked at completion) |
//! | `local` (dev) | `LocalFileSystem` at `{root}/objects` | JSON sidecars at `{root}/meta` | dev HMAC URLs served by [`local::dev_routes`] |
//! | memory (tests) | `InMemory` | object attributes | dev HMAC URLs |
//!
//! Object metadata keeps the content type of the put and its time from the `Clock`
//! (`head().updated_at`), so fakes and real adapters agree under a fake clock. Listing returns the
//! backend's size and modification time without per-object metadata reads (`content_type: None`).

pub mod gcs;
pub mod local;

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::{StreamExt, TryStreamExt};
use mage_core::domain::ArtifactKind;
use mage_core::ports::blob::{
    ArtifactStore, BlobError, BlobKey, DownloadPolicy, HttpMethod, ObjectMeta, PutOptions, PutReceipt, SignedUrl, UploadPolicy, check_signed_ttl,
    clamp_range,
};
use mage_core::ports::clock::Clock;
use mage_core::ports::common::{Cursor, Page, PageRequest};
use object_store::path::Path as ObjectPath;
use object_store::{Attribute, AttributeValue, Attributes, GetOptions, ObjectStore, PutMode, PutMultipartOptions, PutPayload, WriteMultipart};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use url::Url;

use crate::config::{SecretSource, expand_home};

/// `put_file` multipart part size (storage.md: 16 MiB parts).
pub const PART_BYTES: usize = 16 << 20;

/// Concurrent part uploads of one `put_file`.
const PART_CONCURRENCY: usize = 4;

/// Object metadata key holding the put time from the `Clock`.
const UPDATED_AT_METADATA: &str = "mage-updated-at";

/// Per-kind upload limits (`blob.limits.<kind>`, storage.md "Artifact kinds and upload limits").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BlobLimits {
    pub video: u64,
    pub gif: u64,
    pub trace: u64,
    pub screenshot: u64,
    pub log: u64,
    pub script: u64,
    pub patch: u64,
    pub transcript: u64,
    pub scip_index: u64,
    pub fix_brief: u64,
    pub source_bundle: u64,
}

const MIB: u64 = 1 << 20;

impl Default for BlobLimits {
    fn default() -> Self {
        Self {
            video: 512 * MIB,
            gif: 64 * MIB,
            trace: 256 * MIB,
            screenshot: 16 * MIB,
            log: 64 * MIB,
            script: MIB,
            patch: 8 * MIB,
            transcript: 64 * MIB,
            scip_index: 1024 * MIB,
            fix_brief: MIB,
            source_bundle: 2048 * MIB,
        }
    }
}

impl BlobLimits {
    pub fn max_bytes(&self, kind: ArtifactKind) -> u64 {
        match kind {
            ArtifactKind::Video => self.video,
            ArtifactKind::Gif => self.gif,
            ArtifactKind::Trace => self.trace,
            ArtifactKind::Screenshot => self.screenshot,
            ArtifactKind::Log => self.log,
            ArtifactKind::Script => self.script,
            ArtifactKind::Patch => self.patch,
            ArtifactKind::Transcript => self.transcript,
            ArtifactKind::ScipIndex => self.scip_index,
            ArtifactKind::FixBrief => self.fix_brief,
            ArtifactKind::SourceBundle => self.source_bundle,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlobBackend {
    Gcs,
    S3,
    Local,
}

/// `[<role>.blob]` section (server.md "Config": `backend = "gcs" | "local"`, `bucket`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlobConfig {
    pub backend: BlobBackend,
    /// Bucket (`gcs`, `s3`).
    #[serde(default)]
    pub bucket: Option<String>,
    /// Local backend directory (`~/` expands).
    #[serde(default = "default_root")]
    pub root: String,
    /// Local backend HMAC key for dev URLs; `None` = a random key per process (URLs die with it).
    #[serde(default)]
    pub dev_signing_key: Option<SecretSource>,
    #[serde(default)]
    pub limits: BlobLimits,
}

fn default_root() -> String {
    "~/.mage/blob".to_owned()
}

/// How a backend keeps the content type and clock time of an object.
enum Meta {
    /// Object attributes (`Content-Type`, metadata `mage-updated-at`).
    Attributes,
    /// JSON sidecar objects at the same key in a separate store (`LocalFileSystem` has no
    /// attributes).
    Sidecar(Arc<dyn ObjectStore>),
}

#[derive(Debug, Serialize, Deserialize)]
struct SidecarMeta {
    content_type: String,
    #[serde(with = "time::serde::rfc3339")]
    updated_at: OffsetDateTime,
}

/// Who signs URLs.
pub enum Signer {
    Gcs(gcs::GcsV4Signer),
    S3(Arc<dyn object_store::signer::Signer>),
    DevHmac(local::DevSigner),
}

/// `ArtifactStore` over `object_store`.
pub struct ObjectBlobs {
    store: Arc<dyn ObjectStore>,
    meta: Meta,
    signer: Signer,
    limits: BlobLimits,
    clock: Arc<dyn Clock>,
    /// The backend lists in key order (GCS, S3, memory); local listing is sorted here.
    sorted_listing: bool,
}

fn retry() -> object_store::RetryConfig {
    object_store::RetryConfig { max_retries: 3, ..Default::default() }
}

impl ObjectBlobs {
    /// In-memory backend with dev HMAC URLs (tests).
    pub fn in_memory(signer: local::DevSigner, clock: Arc<dyn Clock>) -> Self {
        Self {
            store: Arc::new(object_store::memory::InMemory::new()),
            meta: Meta::Attributes,
            signer: Signer::DevHmac(signer),
            limits: BlobLimits::default(),
            clock,
            sorted_listing: true,
        }
    }

    /// Local filesystem backend under `root` (`objects/` + `meta/`) with dev HMAC URLs.
    pub fn local(root: &Path, signer: local::DevSigner, clock: Arc<dyn Clock>) -> Result<Self, BlobError> {
        let dir = |name: &str| -> Result<PathBuf, BlobError> {
            let path = root.join(name);
            std::fs::create_dir_all(&path).map_err(|e| BlobError::Unavailable(format!("create {}: {e}", path.display())))?;
            Ok(path)
        };
        let objects = object_store::local::LocalFileSystem::new_with_prefix(dir("objects")?).map_err(store_error)?;
        let meta = object_store::local::LocalFileSystem::new_with_prefix(dir("meta")?).map_err(store_error)?;
        Ok(Self {
            store: Arc::new(objects),
            meta: Meta::Sidecar(Arc::new(meta)),
            signer: Signer::DevHmac(signer),
            limits: BlobLimits::default(),
            clock,
            sorted_listing: false,
        })
    }

    /// GCS bucket with credentials from the environment (ADC / metadata server) and V4 URLs signed
    /// through IAM `signBlob`.
    pub fn gcs(bucket: &str, clock: Arc<dyn Clock>) -> Result<Self, BlobError> {
        let store = Arc::new(
            object_store::gcp::GoogleCloudStorageBuilder::from_env().with_bucket_name(bucket).with_retry(retry()).build().map_err(store_error)?,
        );
        Ok(Self {
            store: store.clone(),
            meta: Meta::Attributes,
            signer: Signer::Gcs(gcs::GcsV4Signer::new(store, bucket)),
            limits: BlobLimits::default(),
            clock,
            sorted_listing: true,
        })
    }

    /// S3 bucket with credentials and region from the environment (`AWS_*`).
    pub fn s3(bucket: &str, clock: Arc<dyn Clock>) -> Result<Self, BlobError> {
        let store = Arc::new(object_store::aws::AmazonS3Builder::from_env().with_bucket_name(bucket).with_retry(retry()).build().map_err(store_error)?);
        Ok(Self { store: store.clone(), meta: Meta::Attributes, signer: Signer::S3(store), limits: BlobLimits::default(), clock, sorted_listing: true })
    }

    /// Build from config. `public_url` is the server's base URL (dev URLs point at it).
    pub async fn from_config(config: &BlobConfig, public_url: &Url, clock: Arc<dyn Clock>) -> Result<Self, BlobError> {
        let bucket = || config.bucket.as_deref().filter(|b| !b.is_empty()).ok_or_else(|| BlobError::Invalid("blob.bucket is required".into()));
        let blobs = match config.backend {
            BlobBackend::Gcs => Self::gcs(bucket()?, clock)?,
            BlobBackend::S3 => Self::s3(bucket()?, clock)?,
            BlobBackend::Local => {
                let signer = match &config.dev_signing_key {
                    Some(source) => {
                        let key = source.resolve().await.map_err(|e| BlobError::Invalid(format!("blob.dev_signing_key: {e}")))?;
                        local::DevSigner::new(public_url, key.expose().as_bytes())?
                    }
                    None => local::DevSigner::random(public_url)?,
                };
                Self::local(&expand_home(&config.root), signer, clock)?
            }
        };
        Ok(blobs.with_limits(config.limits))
    }

    pub fn with_limits(mut self, limits: BlobLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn limits(&self) -> &BlobLimits {
        &self.limits
    }

    pub fn signer(&self) -> &Signer {
        &self.signer
    }

    pub(crate) fn clock(&self) -> &dyn Clock {
        &*self.clock
    }

    fn put_attributes(&self, content_type: &str, now: OffsetDateTime) -> Attributes {
        match self.meta {
            Meta::Sidecar(_) => Attributes::new(),
            Meta::Attributes => {
                let mut attributes = Attributes::new();
                attributes.insert(Attribute::ContentType, AttributeValue::from(content_type.to_owned()));
                attributes.insert(Attribute::Metadata(UPDATED_AT_METADATA.into()), AttributeValue::from(now.format(&Rfc3339).unwrap_or_default()));
                attributes
            }
        }
    }

    async fn write_sidecar(&self, path: &ObjectPath, content_type: &str, now: OffsetDateTime) -> Result<(), BlobError> {
        if let Meta::Sidecar(meta) = &self.meta {
            let body = serde_json::to_vec(&SidecarMeta { content_type: content_type.to_owned(), updated_at: now })
                .map_err(|e| BlobError::Unavailable(format!("sidecar: {e}")))?;
            meta.put(path, PutPayload::from(body)).await.map_err(store_error)?;
        }
        Ok(())
    }

    async fn delete_sidecar(&self, path: &ObjectPath) -> Result<(), BlobError> {
        if let Meta::Sidecar(meta) = &self.meta {
            match meta.delete(path).await {
                Ok(()) | Err(object_store::Error::NotFound { .. }) => {}
                Err(e) => return Err(store_error(e)),
            }
        }
        Ok(())
    }

    /// Keys under `prefix` (and `prefix` itself), in key order, strictly after `after`, at most
    /// `limit` when given.
    async fn keys_under(&self, prefix: &BlobKey, after: Option<&BlobKey>, limit: Option<usize>) -> Result<Vec<(BlobKey, object_store::ObjectMeta)>, BlobError> {
        let prefix_path = path_of(prefix)?;
        let mut out: Vec<(BlobKey, object_store::ObjectMeta)> = Vec::new();
        if after.is_none() {
            match self.store.head(&prefix_path).await {
                Ok(meta) => out.push((prefix.clone(), meta)),
                Err(object_store::Error::NotFound { .. }) => {}
                Err(e) => return Err(store_error(e)),
            }
        }
        let mut stream = match (after, self.sorted_listing) {
            (Some(after), true) => self.store.list_with_offset(Some(&prefix_path), &path_of(after)?),
            _ => self.store.list(Some(&prefix_path)),
        };
        while let Some(meta) = stream.next().await {
            let meta = meta.map_err(store_error)?;
            let key = BlobKey::parse(meta.location.as_ref())?;
            if after.is_some_and(|a| key <= *a) || !key.is_within(prefix) || key == *prefix {
                continue;
            }
            out.push((key, meta));
            if self.sorted_listing && limit.is_some_and(|l| out.len() >= l) {
                break;
            }
        }
        if !self.sorted_listing {
            out.sort_by(|a, b| a.0.cmp(&b.0));
            if let Some(limit) = limit {
                out.truncate(limit);
            }
        }
        Ok(out)
    }

    async fn sign_dev_or_backend(&self, key: &BlobKey, method: HttpMethod, upload: Option<&UploadPolicy>, ttl: Duration) -> Result<SignedUrl, BlobError> {
        check_signed_ttl(ttl)?;
        let now = self.clock.now();
        let expires_at = now + ttl;
        let content_type_header = upload.map(|p| vec![("content-type".to_owned(), p.content_type.clone())]).unwrap_or_default();
        match &self.signer {
            Signer::DevHmac(signer) => {
                let url = signer.sign(key, method, upload.map(|p| (p.content_type.as_str(), p.max_bytes)), expires_at)?;
                Ok(SignedUrl { method, url, headers: content_type_header, expires_at })
            }
            Signer::Gcs(signer) => {
                let mut headers = content_type_header;
                if let Some(policy) = upload {
                    headers.push((gcs::CONTENT_LENGTH_RANGE.to_owned(), format!("0,{}", policy.max_bytes)));
                }
                let url = signer.sign(method, key, &headers, now, ttl).await?;
                Ok(SignedUrl { method, url, headers, expires_at })
            }
            Signer::S3(signer) => {
                let verb = match method {
                    HttpMethod::Get => reqwest::Method::GET,
                    HttpMethod::Put => reqwest::Method::PUT,
                };
                let url = signer.signed_url(verb, &path_of(key)?, ttl).await.map_err(store_error)?;
                Ok(SignedUrl { method, url: url.to_string(), headers: content_type_header, expires_at })
            }
        }
    }
}

fn path_of(key: &BlobKey) -> Result<ObjectPath, BlobError> {
    ObjectPath::parse(key.as_str()).map_err(|e| BlobError::Invalid(format!("blob key {key}: {e}")))
}

/// `object_store` errors → `BlobError` (storage.md: `NotFound`, `Unavailable`, `Invalid`).
pub(crate) fn store_error(err: object_store::Error) -> BlobError {
    match err {
        object_store::Error::NotFound { .. } => BlobError::NotFound,
        object_store::Error::AlreadyExists { .. } | object_store::Error::Precondition { .. } => BlobError::Conflict,
        object_store::Error::InvalidPath { source } => BlobError::Invalid(source.to_string()),
        object_store::Error::NotSupported { source } => BlobError::Invalid(source.to_string()),
        object_store::Error::NotImplemented => BlobError::Invalid("operation not supported by this backend".into()),
        other => BlobError::Unavailable(other.to_string()),
    }
}

fn to_offset(meta: &object_store::ObjectMeta) -> OffsetDateTime {
    let nanos = i128::from(meta.last_modified.timestamp()) * 1_000_000_000 + i128::from(meta.last_modified.timestamp_subsec_nanos());
    OffsetDateTime::from_unix_timestamp_nanos(nanos).unwrap_or(OffsetDateTime::UNIX_EPOCH)
}

fn receipt(bytes: u64, hasher: Sha256, created: bool) -> PutReceipt {
    PutReceipt { bytes, sha256: hasher.finalize().into(), created }
}

#[async_trait]
impl ArtifactStore for ObjectBlobs {
    async fn put(&self, key: &BlobKey, body: Vec<u8>, opts: &PutOptions) -> Result<PutReceipt, BlobError> {
        let path = path_of(key)?;
        let now = self.clock.now();
        let len = body.len() as u64;
        let mut hasher = Sha256::new();
        hasher.update(&body);
        let put = object_store::PutOptions {
            mode: if opts.if_absent { PutMode::Create } else { PutMode::Overwrite },
            attributes: self.put_attributes(&opts.content_type, now),
            ..Default::default()
        };
        match self.store.put_opts(&path, PutPayload::from(body), put).await {
            Ok(_) => {
                self.write_sidecar(&path, &opts.content_type, now).await?;
                Ok(receipt(len, hasher, true))
            }
            Err(object_store::Error::AlreadyExists { .. } | object_store::Error::Precondition { .. }) if opts.if_absent => Ok(receipt(len, hasher, false)),
            Err(e) => Err(store_error(e)),
        }
    }

    async fn put_file(&self, key: &BlobKey, src: &Path, opts: &PutOptions) -> Result<PutReceipt, BlobError> {
        let path = path_of(key)?;
        let mut file = tokio::fs::File::open(src).await.map_err(|e| BlobError::Invalid(format!("source file {}: {e}", src.display())))?;
        let size = file.metadata().await.map_err(|e| BlobError::Invalid(format!("source file {}: {e}", src.display())))?.len();
        if size <= PART_BYTES as u64 {
            let mut body = Vec::with_capacity(usize::try_from(size).unwrap_or(0));
            file.read_to_end(&mut body).await.map_err(|e| BlobError::Unavailable(format!("read {}: {e}", src.display())))?;
            return self.put(key, body, opts).await;
        }
        // Large file: conditional create checked up front (multipart has no create mode; the keys
        // that use `if_absent` are content-addressed, so a racing writer stores the same bytes).
        let exists = match self.store.head(&path).await {
            Ok(_) => true,
            Err(object_store::Error::NotFound { .. }) => false,
            Err(e) => return Err(store_error(e)),
        };
        let now = self.clock.now();
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 1 << 20];
        let mut total = 0u64;
        if opts.if_absent && exists {
            loop {
                let n = file.read(&mut buf).await.map_err(|e| BlobError::Unavailable(format!("read {}: {e}", src.display())))?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                total += n as u64;
            }
            return Ok(receipt(total, hasher, false));
        }
        let upload = self
            .store
            .put_multipart_opts(&path, PutMultipartOptions { attributes: self.put_attributes(&opts.content_type, now), ..Default::default() })
            .await
            .map_err(store_error)?;
        let mut writer = WriteMultipart::new_with_chunk_size(upload, PART_BYTES);
        let streamed: Result<(), BlobError> = async {
            loop {
                let n = file.read(&mut buf).await.map_err(|e| BlobError::Unavailable(format!("read {}: {e}", src.display())))?;
                if n == 0 {
                    return Ok(());
                }
                hasher.update(&buf[..n]);
                total += n as u64;
                writer.wait_for_capacity(PART_CONCURRENCY).await.map_err(store_error)?;
                writer.write(&buf[..n]);
            }
        }
        .await;
        if let Err(e) = streamed {
            let _ = writer.abort().await;
            return Err(e);
        }
        writer.finish().await.map_err(store_error)?;
        self.write_sidecar(&path, &opts.content_type, now).await?;
        Ok(receipt(total, hasher, true))
    }

    async fn get(&self, key: &BlobKey, max_bytes: u64) -> Result<Vec<u8>, BlobError> {
        let result = self.store.get(&path_of(key)?).await.map_err(store_error)?;
        if result.meta.size > max_bytes {
            return Err(BlobError::TooLarge { limit: max_bytes });
        }
        Ok(result.bytes().await.map_err(store_error)?.to_vec())
    }

    async fn get_range(&self, key: &BlobKey, range: Range<u64>) -> Result<Vec<u8>, BlobError> {
        let path = path_of(key)?;
        let size = self.store.head(&path).await.map_err(store_error)?.size;
        let range = clamp_range(&range, size)?;
        Ok(self.store.get_range(&path, range).await.map_err(store_error)?.to_vec())
    }

    async fn get_to_file(&self, key: &BlobKey, dst: &Path) -> Result<u64, BlobError> {
        let result = self.store.get(&path_of(key)?).await.map_err(store_error)?;
        let mut tmp_name = dst.as_os_str().to_owned();
        tmp_name.push(".tmp");
        let tmp = PathBuf::from(tmp_name);
        let written: Result<u64, BlobError> = async {
            let mut out = tokio::fs::File::create(&tmp).await.map_err(|e| BlobError::Unavailable(format!("create {}: {e}", tmp.display())))?;
            let mut stream = result.into_stream();
            let mut total = 0u64;
            while let Some(chunk) = stream.try_next().await.map_err(store_error)? {
                out.write_all(&chunk).await.map_err(|e| BlobError::Unavailable(format!("write {}: {e}", tmp.display())))?;
                total += chunk.len() as u64;
            }
            out.sync_all().await.map_err(|e| BlobError::Unavailable(format!("fsync {}: {e}", tmp.display())))?;
            Ok(total)
        }
        .await;
        match written {
            Ok(total) => {
                tokio::fs::rename(&tmp, dst).await.map_err(|e| BlobError::Unavailable(format!("rename to {}: {e}", dst.display())))?;
                Ok(total)
            }
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp).await;
                Err(e)
            }
        }
    }

    async fn head(&self, key: &BlobKey) -> Result<ObjectMeta, BlobError> {
        let path = path_of(key)?;
        match &self.meta {
            Meta::Attributes => {
                let result = self.store.get_opts(&path, GetOptions { head: true, ..Default::default() }).await.map_err(store_error)?;
                let content_type = result.attributes.get(&Attribute::ContentType).map(|v| v.as_ref().to_owned());
                let updated_at = result
                    .attributes
                    .get(&Attribute::Metadata(UPDATED_AT_METADATA.into()))
                    .and_then(|v| OffsetDateTime::parse(v.as_ref(), &Rfc3339).ok())
                    .unwrap_or_else(|| to_offset(&result.meta));
                Ok(ObjectMeta { bytes: result.meta.size, content_type, updated_at })
            }
            Meta::Sidecar(meta_store) => {
                let meta = self.store.head(&path).await.map_err(store_error)?;
                let sidecar = match meta_store.get(&path).await {
                    Ok(result) => serde_json::from_slice::<SidecarMeta>(&result.bytes().await.map_err(store_error)?).ok(),
                    Err(object_store::Error::NotFound { .. }) => None,
                    Err(e) => return Err(store_error(e)),
                };
                Ok(match sidecar {
                    Some(s) => ObjectMeta { bytes: meta.size, content_type: Some(s.content_type), updated_at: s.updated_at },
                    None => ObjectMeta { bytes: meta.size, content_type: None, updated_at: to_offset(&meta) },
                })
            }
        }
    }

    async fn delete(&self, key: &BlobKey) -> Result<(), BlobError> {
        let path = path_of(key)?;
        match self.store.delete(&path).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => {}
            Err(e) => return Err(store_error(e)),
        }
        self.delete_sidecar(&path).await
    }

    async fn delete_prefix(&self, prefix: &BlobKey) -> Result<u64, BlobError> {
        let keys = self.keys_under(prefix, None, None).await?;
        let mut deleted = 0u64;
        for (key, _) in keys {
            let path = path_of(&key)?;
            match self.store.delete(&path).await {
                Ok(()) => deleted += 1,
                Err(object_store::Error::NotFound { .. }) => {}
                Err(e) => return Err(store_error(e)),
            }
            self.delete_sidecar(&path).await?;
        }
        Ok(deleted)
    }

    async fn list_prefix(&self, prefix: &BlobKey, page: PageRequest) -> Result<Page<(BlobKey, ObjectMeta)>, BlobError> {
        let after = page.after.as_ref().map(|c| BlobKey::parse(&c.0)).transpose().map_err(|_| BlobError::Invalid("bad list cursor".into()))?;
        let limit = page.effective_limit();
        let mut keys = self.keys_under(prefix, after.as_ref(), Some(limit + 1)).await?;
        let more = keys.len() > limit;
        keys.truncate(limit);
        let next = if more { keys.last().map(|(k, _)| Cursor(k.as_str().to_owned())) } else { None };
        let items = keys
            .into_iter()
            .map(|(key, meta)| {
                let updated_at = to_offset(&meta);
                (key, ObjectMeta { bytes: meta.size, content_type: None, updated_at })
            })
            .collect();
        Ok(Page { items, next })
    }

    async fn signed_upload(&self, key: &BlobKey, policy: &UploadPolicy) -> Result<SignedUrl, BlobError> {
        if policy.content_type.is_empty() {
            return Err(BlobError::Invalid("signed upload needs a content type".into()));
        }
        self.sign_dev_or_backend(key, HttpMethod::Put, Some(policy), policy.ttl).await
    }

    async fn signed_download(&self, key: &BlobKey, policy: &DownloadPolicy) -> Result<SignedUrl, BlobError> {
        self.sign_dev_or_backend(key, HttpMethod::Get, None, policy.ttl).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_default_to_the_storage_table_and_deserialize_partially() {
        let limits: BlobLimits = toml::from_str("video = 1024\n").unwrap_or_default();
        assert_eq!(limits.max_bytes(ArtifactKind::Video), 1024);
        assert_eq!(limits.max_bytes(ArtifactKind::Script), MIB, "unset kinds keep their default");
        assert_eq!(BlobLimits::default().max_bytes(ArtifactKind::SourceBundle), 2048 * MIB);
        assert!(toml::from_str::<BlobLimits>("vidoe = 1\n").is_err(), "unknown kinds are refused");
    }
}
