//! Recordings for the PR body and Linear (fix.md "Media in the PR and in Linear").
//!
//! [`MediaCache`] remembers upload results per `(artifact, destination)` so retries of
//! `fix.open_pr` and `notify.ticket` never upload twice. It is a small JSON object per entry in
//! the artifact store at `org/{org}/media_cache/{artifact}/{destination}.json` (under the org
//! prefix, so org deletion removes it; not an `artifacts` row, so retention never reaps it).
//!
//! [`PrMedia::links`] uploads the before/after GIFs and the after mp4 through
//! `PullRequests::upload_media` and falls back to Mage artifact links on **any** failure: media
//! never block a PR.

use std::sync::Arc;

use mage_core::domain::{ArtifactId, ArtifactKind, OrgId, Repo, VerifyResult};
use mage_core::ports::blob::{ArtifactStore, BlobError, BlobKey, PutOptions};
use url::Url;

use crate::app::fix::pr_body::{MediaLink, PrMediaLinks};
use crate::error::AppError;
use crate::ports::pull_requests::{MediaKind, MediaUpload, PullRequests};
use crate::ports::runs::{ArtifactRecord, RunStore};

/// Destination name of GitHub PR uploads in the cache.
pub const GITHUB: &str = "github";
const ENTRY_MAX_BYTES: u64 = 8 * 1024;

#[derive(serde::Serialize, serde::Deserialize)]
struct Entry {
    url: Url,
}

fn blob_err(err: BlobError) -> AppError {
    match err {
        BlobError::NotFound => AppError::NotFound("media cache entry".into()),
        BlobError::Conflict => AppError::Conflict("media cache entry".into()),
        BlobError::Invalid(m) => AppError::Invalid(m),
        BlobError::Unavailable(m) => AppError::Unavailable(m),
        BlobError::TooLarge { limit } => AppError::Invalid(format!("media cache entry larger than {limit} bytes")),
    }
}

/// Upload results per `(artifact, destination)`.
#[derive(Clone)]
pub struct MediaCache {
    artifacts: Arc<dyn ArtifactStore>,
}

impl MediaCache {
    pub fn new(artifacts: Arc<dyn ArtifactStore>) -> Self {
        Self { artifacts }
    }

    fn key(org: OrgId, artifact: ArtifactId, destination: &str) -> Result<BlobKey, AppError> {
        if destination.is_empty() || !destination.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-') {
            return Err(AppError::Invalid(format!("media destination {destination:?} must be [a-z0-9_-]+")));
        }
        BlobKey::parse(&format!(
            "org/{}/media_cache/{}/{destination}.json",
            org.as_uuid().hyphenated(),
            artifact.as_uuid().hyphenated()
        ))
        .map_err(blob_err)
    }

    pub async fn get(&self, org: OrgId, artifact: ArtifactId, destination: &str) -> Result<Option<Url>, AppError> {
        let key = Self::key(org, artifact, destination)?;
        match self.artifacts.get(&key, ENTRY_MAX_BYTES).await {
            Ok(bytes) => {
                let entry: Entry = serde_json::from_slice(&bytes).map_err(|e| AppError::Internal(format!("media cache entry {key:?}: {e}")))?;
                Ok(Some(entry.url))
            }
            Err(BlobError::NotFound) => Ok(None),
            Err(e) => Err(blob_err(e)),
        }
    }

    pub async fn put(&self, org: OrgId, artifact: ArtifactId, destination: &str, url: &Url) -> Result<(), AppError> {
        let key = Self::key(org, artifact, destination)?;
        let body = serde_json::to_vec(&Entry { url: url.clone() }).map_err(|e| AppError::Internal(e.to_string()))?;
        self.artifacts.put(&key, body, &PutOptions { content_type: "application/json".into(), if_absent: false }).await.map_err(blob_err)?;
        Ok(())
    }
}

/// The artifacts a PR body shows, picked from the repro gate (before) and the fix gate (after).
#[derive(Debug, Clone, Default)]
pub struct MediaPick {
    pub before_gif: Option<ArtifactRecord>,
    pub after_gif: Option<ArtifactRecord>,
    /// The after recording; uploadable only when it is an mp4.
    pub after_video: Option<ArtifactRecord>,
    pub trace: Option<ArtifactId>,
    pub test_log: Option<ArtifactId>,
}

fn is_mp4(rec: &ArtifactRecord) -> bool {
    rec.content_type.eq_ignore_ascii_case("video/mp4") || rec.name.ends_with(".mp4")
}

/// Upload step of `OpenPullRequest`.
pub struct PrMedia {
    pub runs: Arc<dyn RunStore>,
    pub artifacts: Arc<dyn ArtifactStore>,
    pub pull_requests: Arc<dyn PullRequests>,
    pub cache: MediaCache,
}

impl PrMedia {
    async fn records(&self, org: OrgId, verify: &VerifyResult) -> Vec<ArtifactRecord> {
        let mut out = Vec::with_capacity(verify.artifacts.len());
        for id in &verify.artifacts {
            match self.runs.artifact(org, *id).await {
                Ok(rec) => out.push(rec),
                Err(e) => tracing::warn!(artifact = %id, error = %e, "verify artifact missing; skipped in the PR body"),
            }
        }
        out
    }

    /// GIF of the repro gate; GIF, video (mp4 preferred), trace and test log of the fix gate.
    pub async fn pick(&self, org: OrgId, before: Option<&VerifyResult>, after: &VerifyResult) -> MediaPick {
        let before = match before {
            Some(v) => self.records(org, v).await,
            None => Vec::new(),
        };
        let after = self.records(org, after).await;
        let first = |recs: &[ArtifactRecord], kind: ArtifactKind| recs.iter().find(|r| r.artifact.kind == kind).cloned();
        let videos: Vec<&ArtifactRecord> = after.iter().filter(|r| r.artifact.kind == ArtifactKind::Video).collect();
        MediaPick {
            before_gif: first(&before, ArtifactKind::Gif),
            after_gif: first(&after, ArtifactKind::Gif),
            after_video: videos.iter().find(|r| is_mp4(r)).or(videos.first()).map(|r| (*r).clone()),
            trace: first(&after, ArtifactKind::Trace).map(|r| r.artifact.id),
            test_log: first(&after, ArtifactKind::Log).map(|r| r.artifact.id),
        }
    }

    /// One slot: cached URL, else upload; any failure → Mage link.
    async fn link(&self, org: OrgId, repo: &Repo, rec: Option<&ArtifactRecord>, label: &str) -> MediaLink {
        let Some(rec) = rec else { return MediaLink::Missing };
        let id = rec.artifact.id;
        let kind = match rec.artifact.kind {
            ArtifactKind::Gif => MediaKind::Gif,
            ArtifactKind::Video if is_mp4(rec) => MediaKind::Mp4,
            ArtifactKind::Screenshot => MediaKind::Png,
            _ => return MediaLink::Fallback(id),
        };
        match self.cache.get(org, id, GITHUB).await {
            Ok(Some(url)) => return MediaLink::Uploaded(url),
            Ok(None) => {}
            Err(e) => tracing::warn!(artifact = %id, error = %e, "media cache unreadable; uploading"),
        }
        if rec.artifact.bytes > kind.max_bytes() {
            return MediaLink::Fallback(id);
        }
        let bytes = match BlobKey::parse(&rec.artifact.key) {
            Ok(key) => self.artifacts.get(&key, kind.max_bytes()).await,
            Err(e) => Err(e),
        };
        let bytes = match bytes {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::warn!(artifact = %id, error = %e, "recording unreadable; linking the Mage artifact");
                return MediaLink::Fallback(id);
            }
        };
        let upload = MediaUpload { name: format!("{label}.{}", kind.extension()), kind, bytes };
        match self.pull_requests.upload_media(org, repo, &upload).await {
            Ok(url) => {
                if let Err(e) = self.cache.put(org, id, GITHUB, &url.0).await {
                    tracing::warn!(artifact = %id, error = %e, "media cache write failed");
                }
                MediaLink::Uploaded(url.0)
            }
            Err(e) => {
                tracing::info!(artifact = %id, error = %e, "GitHub refused the media upload; linking the Mage artifact");
                MediaLink::Fallback(id)
            }
        }
    }

    pub async fn links(&self, org: OrgId, repo: &Repo, pick: &MediaPick) -> PrMediaLinks {
        PrMediaLinks {
            before_gif: self.link(org, repo, pick.before_gif.as_ref(), "before").await,
            after_gif: self.link(org, repo, pick.after_gif.as_ref(), "after").await,
            after_video: self.link(org, repo, pick.after_video.as_ref(), "after").await,
        }
    }
}
