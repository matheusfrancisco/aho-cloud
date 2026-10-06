//! `ArtifactStore` port (storage.md "`ArtifactStore`", "Blob layout and signed URLs"): bytes by
//! key plus signed URLs. One trait, because `mage_infra` cannot implement a server port and signed
//! URLs are an object-store capability.
//!
//! Semantics every implementation follows (contract suite in `mage_core::testing`):
//!
//! - `put` with `if_absent` is a conditional create: an existing object is not rewritten and the
//!   receipt says `created: false`. The receipt's `bytes`/`sha256` describe the given body.
//! - `get(max_bytes)` of a bigger object is `TooLarge { limit: max_bytes }`.
//! - `get_range(start..end)`: `start >= end` or `start >= size` is `Invalid`; `end > size` is cut
//!   to the object size (object_store `GetRange::Bounded`).
//! - Prefix operations are segment-wise: prefix `org/a` covers `org/a` and `org/a/…`, never
//!   `org/ab`. Listing is ordered by key.
//! - `delete` of a missing key is `Ok`.
//! - Signed URLs: TTL in `(0, 1 h]` (else `Invalid`), `expires_at = now + ttl`. Uploads are `PUT`
//!   and carry the signed `content-type` header the client must send; downloads are `GET`.

use std::fmt;
use std::ops::Range;
use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::domain::artifact::ArtifactKind;
use crate::domain::ids::{BlobOid, OrgId, RepoId, RunId, TreeOid};
use crate::domain::knowledge::blob_content_key;
use crate::ports::common::{Page, PageRequest};

/// Signed URL lifetime ceiling (storage.md "Signed URL policy").
pub const MAX_SIGNED_URL_TTL: Duration = Duration::from_secs(60 * 60);

/// Object names are capped at GCS's 1024 bytes.
pub const BLOB_KEY_MAX_BYTES: usize = 1024;

/// `{name}` of a run artifact key: `[a-z0-9][a-z0-9._-]{0,127}`.
const ARTIFACT_NAME_MAX_BYTES: usize = 128;

/// A validated object key (storage.md "Keys"): `/`-separated segments of `[a-z0-9._-]`, no empty,
/// `.` or `..` segments, no leading or trailing `/`. Every tenant key starts with `org/{org}/`.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlobKey(String);

impl BlobKey {
    /// `org/{org}/runs/{run}/{kind}/{name}`; `name` must match `[a-z0-9][a-z0-9._-]{0,127}`.
    pub fn run_artifact(org: OrgId, run: RunId, kind: ArtifactKind, name: &str) -> Result<Self, BlobError> {
        validate_artifact_name(name)?;
        Ok(Self(format!("org/{}/runs/{}/{}/{name}", org.as_uuid().hyphenated(), run.as_uuid().hyphenated(), kind.as_str())))
    }

    /// `org/{org}/blobs/{oid_hex[0..2]}/{oid_hex}` (`mage_core::domain::knowledge::blob_content_key`).
    pub fn blob_content(org: OrgId, oid: BlobOid) -> Self {
        Self(blob_content_key(org, oid))
    }

    /// `org/{org}/repos/{repo}/bundles/{tree_oid}.tar.zst`.
    pub fn source_bundle(org: OrgId, repo: RepoId, tree: TreeOid) -> Self {
        Self(format!("org/{}/repos/{}/bundles/{tree}.tar.zst", org.as_uuid().hyphenated(), repo.as_uuid().hyphenated()))
    }

    /// `org/{org}`: everything the tenant owns (org deletion is one prefix delete).
    pub fn org_prefix(org: OrgId) -> Self {
        Self(format!("org/{}", org.as_uuid().hyphenated()))
    }

    pub fn parse(s: &str) -> Result<Self, BlobError> {
        if s.is_empty() {
            return Err(BlobError::Invalid("blob key is empty".into()));
        }
        if s.len() > BLOB_KEY_MAX_BYTES {
            return Err(BlobError::Invalid(format!("blob key longer than {BLOB_KEY_MAX_BYTES} bytes")));
        }
        if let Some(c) = s.chars().find(|c| !matches!(c, 'a'..='z' | '0'..='9' | '.' | '_' | '/' | '-')) {
            return Err(BlobError::Invalid(format!("blob key contains {c:?}")));
        }
        for segment in s.split('/') {
            match segment {
                "" => return Err(BlobError::Invalid("blob key has an empty segment or a leading/trailing `/`".into())),
                "." | ".." => return Err(BlobError::Invalid("blob key has a `.` or `..` segment".into())),
                _ => {}
            }
        }
        Ok(Self(s.to_owned()))
    }

    /// The tenant of an `org/{uuid}/…` key.
    pub fn org(&self) -> Option<OrgId> {
        let mut segments = self.0.split('/');
        if segments.next() != Some("org") {
            return None;
        }
        let segment = segments.next()?;
        let uuid = Uuid::try_parse(segment).ok()?;
        // Only the canonical hyphenated lowercase form is a tenant segment.
        (uuid.hyphenated().to_string() == segment).then(|| OrgId::from_uuid(uuid))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `self` equals `prefix` or lies below it, segment-wise.
    pub fn is_within(&self, prefix: &BlobKey) -> bool {
        match self.0.strip_prefix(prefix.as_str()) {
            Some("") => true,
            Some(rest) => rest.starts_with('/'),
            None => false,
        }
    }
}

fn validate_artifact_name(name: &str) -> Result<(), BlobError> {
    let mut chars = name.chars();
    let first_ok = chars.next().is_some_and(|c| matches!(c, 'a'..='z' | '0'..='9'));
    let rest_ok = chars.all(|c| matches!(c, 'a'..='z' | '0'..='9' | '.' | '_' | '-'));
    if first_ok && rest_ok && name.len() <= ARTIFACT_NAME_MAX_BYTES && name != "." && name != ".." {
        Ok(())
    } else {
        Err(BlobError::Invalid(format!("artifact name {name:?} must match [a-z0-9][a-z0-9._-]{{0,127}}")))
    }
}

impl fmt::Display for BlobKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for BlobKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BlobKey({:?})", self.0)
    }
}

impl std::str::FromStr for BlobKey {
    type Err = BlobError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl Serialize for BlobKey {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for BlobKey {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = <std::borrow::Cow<'de, str>>::deserialize(d)?;
        Self::parse(&s).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PutOptions {
    pub content_type: String,
    /// Conditional create (content-addressed keys): keep an existing object.
    pub if_absent: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PutReceipt {
    pub bytes: u64,
    pub sha256: [u8; 32],
    /// `false` when `if_absent` found an existing object (not rewritten).
    pub created: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectMeta {
    pub bytes: u64,
    pub content_type: Option<String>,
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadPolicy {
    /// Signed: the client must send exactly this `content-type`.
    pub content_type: String,
    pub max_bytes: u64,
    pub ttl: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadPolicy {
    pub ttl: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    Get,
    Put,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedUrl {
    pub method: HttpMethod,
    pub url: String,
    /// The client MUST send exactly these headers (they are signed).
    pub headers: Vec<(String, String)>,
    pub expires_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BlobError {
    #[error("object not found")]
    NotFound,
    #[error("object conflict")]
    Conflict,
    #[error("invalid blob request: {0}")]
    Invalid(String),
    #[error("object store unavailable: {0}")]
    Unavailable(String),
    #[error("object larger than {limit} bytes")]
    TooLarge { limit: u64 },
}

/// TTL rule shared by the signing methods: `(0, MAX_SIGNED_URL_TTL]`.
pub fn check_signed_ttl(ttl: Duration) -> Result<(), BlobError> {
    if ttl.is_zero() || ttl > MAX_SIGNED_URL_TTL {
        Err(BlobError::Invalid(format!("signed URL ttl {}s outside (0, 3600]", ttl.as_secs_f64())))
    } else {
        Ok(())
    }
}

/// Range rule of `get_range` for an object of `size` bytes (see the module docs).
pub fn clamp_range(range: &Range<u64>, size: u64) -> Result<Range<u64>, BlobError> {
    if range.start >= range.end {
        return Err(BlobError::Invalid(format!("empty or inverted range {}..{}", range.start, range.end)));
    }
    if range.start >= size {
        return Err(BlobError::Invalid(format!("range start {} at or beyond object size {size}", range.start)));
    }
    Ok(range.start..range.end.min(size))
}

#[async_trait]
pub trait ArtifactStore: Send + Sync {
    async fn put(&self, key: &BlobKey, body: Vec<u8>, opts: &PutOptions) -> Result<PutReceipt, BlobError>;
    /// Upload a local file (multipart, streamed).
    async fn put_file(&self, key: &BlobKey, src: &Path, opts: &PutOptions) -> Result<PutReceipt, BlobError>;
    /// Whole object; `TooLarge` if bigger than `max_bytes`.
    async fn get(&self, key: &BlobKey, max_bytes: u64) -> Result<Vec<u8>, BlobError>;
    async fn get_range(&self, key: &BlobKey, range: Range<u64>) -> Result<Vec<u8>, BlobError>;
    /// Stream to `dst.tmp`, fsync, rename to `dst`; returns the byte count.
    async fn get_to_file(&self, key: &BlobKey, dst: &Path) -> Result<u64, BlobError>;
    async fn head(&self, key: &BlobKey) -> Result<ObjectMeta, BlobError>;
    /// `NotFound` is `Ok`.
    async fn delete(&self, key: &BlobKey) -> Result<(), BlobError>;
    /// Returns how many objects were deleted.
    async fn delete_prefix(&self, prefix: &BlobKey) -> Result<u64, BlobError>;
    async fn list_prefix(&self, prefix: &BlobKey, page: PageRequest) -> Result<Page<(BlobKey, ObjectMeta)>, BlobError>;
    async fn signed_upload(&self, key: &BlobKey, policy: &UploadPolicy) -> Result<SignedUrl, BlobError>;
    async fn signed_download(&self, key: &BlobKey, policy: &DownloadPolicy) -> Result<SignedUrl, BlobError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn org() -> OrgId {
        OrgId::from_uuid(Uuid::from_u128(0x0190_f8a1_2b3c_7d4e_8f00_1122_3344_5566))
    }

    #[test]
    fn constructors_follow_the_documented_layout() {
        let run = RunId::from_uuid(Uuid::from_u128(7));
        let key = BlobKey::run_artifact(org(), run, ArtifactKind::ScipIndex, "scip-go.scip").unwrap();
        assert_eq!(key.as_str(), "org/0190f8a1-2b3c-7d4e-8f00-112233445566/runs/00000000-0000-0000-0000-000000000007/scip_index/scip-go.scip");
        let repo = RepoId::from_uuid(Uuid::from_u128(8));
        let bundle = BlobKey::source_bundle(org(), repo, TreeOid::from_bytes([0xab; 20]));
        assert!(bundle.as_str().ends_with(&format!("/bundles/{}.tar.zst", "ab".repeat(20))));
        for key in [&key, &bundle, &BlobKey::blob_content(org(), BlobOid::from_bytes([1; 20])), &BlobKey::org_prefix(org())] {
            assert_eq!(BlobKey::parse(key.as_str()).as_ref(), Ok(key), "constructors produce parseable keys");
            assert_eq!(key.org(), Some(org()));
            assert!(key.is_within(&BlobKey::org_prefix(org())));
        }
    }

    #[test]
    fn artifact_names_are_restricted() {
        let run = RunId::from_uuid(Uuid::from_u128(7));
        for bad in ["", "Video.webm", "-x", ".hidden", "a/b", "..", &"a".repeat(129)] {
            assert!(BlobKey::run_artifact(org(), run, ArtifactKind::Video, bad).is_err(), "{bad:?}");
        }
        for good in ["attempt-2.webm", "brief-v3.md", "0", &"a".repeat(128)] {
            assert!(BlobKey::run_artifact(org(), run, ArtifactKind::Video, good).is_ok(), "{good:?}");
        }
    }

    #[test]
    fn parse_rejects_traversal_and_foreign_characters() {
        for bad in ["", "/org/x", "org/x/", "org//x", "org/../x", "org/./x", "Org/x", "org/x y", "org/x\0", &"a".repeat(1025)] {
            assert!(BlobKey::parse(bad).is_err(), "{bad:?}");
        }
        assert_eq!(BlobKey::parse("tmp/a.b_c-d").map(|k| k.org()), Ok(None));
        assert_eq!(BlobKey::parse("org/not-a-uuid/x").map(|k| k.org()), Ok(None));
    }

    #[test]
    fn prefixes_are_segment_wise() {
        let prefix = BlobKey::parse("org/a").unwrap();
        assert!(BlobKey::parse("org/a").unwrap().is_within(&prefix));
        assert!(BlobKey::parse("org/a/b").unwrap().is_within(&prefix));
        assert!(!BlobKey::parse("org/ab").unwrap().is_within(&prefix));
    }

    #[test]
    fn ranges_and_ttls_follow_the_object_store_rules() {
        assert_eq!(clamp_range(&(2..5), 10), Ok(2..5));
        assert_eq!(clamp_range(&(8..50), 10), Ok(8..10));
        assert!(clamp_range(&(10..11), 10).is_err());
        assert!(clamp_range(&(3..3), 10).is_err());
        assert!(check_signed_ttl(Duration::ZERO).is_err());
        assert!(check_signed_ttl(MAX_SIGNED_URL_TTL).is_ok());
        assert!(check_signed_ttl(MAX_SIGNED_URL_TTL + Duration::from_secs(1)).is_err());
    }

    #[test]
    fn http_method_serializes_uppercase() {
        assert_eq!(serde_json::to_string(&HttpMethod::Put).unwrap(), "\"PUT\"");
        assert_eq!(serde_json::from_str::<HttpMethod>("\"GET\"").unwrap(), HttpMethod::Get);
    }
}
