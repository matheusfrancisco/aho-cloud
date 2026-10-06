//! RequestUpload, CompleteUpload, RefreshDownloads (runs.md; storage.md "Signed URL policy").

use std::time::Duration;

use mage_core::domain::{ArtifactId, ArtifactKind, JobPayload, RunEventKind, RunId};
use mage_core::ports::blob::{BlobError, BlobKey, HttpMethod, UploadPolicy};
use mage_core::ports::jobs::NewJob;
use mage_core::wire::runner::{
    ArtifactSpec, CompleteUploadAck, Download, RefreshDownloadsRequest, Rejection, RejectionItem, UploadGrant, UploadRequest,
};

use crate::app::runs::{ProtocolError, RunManager, RunnerCaller, blob_err};
use crate::error::AppError;
use crate::ports::runs::{ArtifactState, NewArtifact, RunEventDraft, RunWrite};

fn rejected(message: impl Into<String>, path: &str, reason: impl Into<String>) -> ProtocolError {
    ProtocolError::Rejected(Rejection { gate: None, message: message.into(), items: vec![RejectionItem { path: path.into(), reason: reason.into() }] })
}

impl RunManager {
    /// `POST /runs/{id}/artifacts`: reserve the row (idempotent on `(run, attempt, kind, name)`)
    /// and sign a PUT URL that requires the declared `Content-Type` and size.
    pub async fn request_upload(&self, caller: &RunnerCaller, run: RunId, req: UploadRequest) -> Result<UploadGrant, ProtocolError> {
        let rec = self.fenced(caller, run, req.attempt).await?;
        let spec: ArtifactSpec = req.spec;
        if !ArtifactSpec::kind_allowed(spec.kind) {
            return Err(AppError::Invalid(format!("runners may not upload {} artifacts", spec.kind.as_str())).into());
        }
        if !ArtifactSpec::name_valid(&spec.name) {
            return Err(AppError::Invalid(format!("artifact name {:?} must match [a-z0-9][a-z0-9._-]{{0,127}}", spec.name)).into());
        }
        let max = ArtifactSpec::max_bytes(spec.kind);
        if spec.bytes > max {
            return Err(ProtocolError::TooLarge(format!("{} artifacts are limited to {max} bytes", spec.kind.as_str())));
        }
        if spec.content_type.trim().is_empty() || spec.content_type.len() > 256 {
            return Err(AppError::Invalid("content_type is required".into()).into());
        }
        let org = rec.org();
        let a = self
            .d
            .runs
            .reserve_artifact(
                org,
                NewArtifact {
                    run_id: run,
                    attempt: req.attempt,
                    kind: spec.kind,
                    name: spec.name.clone(),
                    bytes: spec.bytes,
                    sha256: spec.sha256,
                    content_type: spec.content_type.clone(),
                },
            )
            .await?;
        if a.artifact.bytes != spec.bytes || a.artifact.sha256 != spec.sha256 || a.content_type != spec.content_type {
            return Err(ProtocolError::IdempotencyMismatch);
        }
        let key = BlobKey::parse(&a.artifact.key).map_err(blob_err)?;
        let policy = UploadPolicy { content_type: a.content_type.clone(), max_bytes: spec.bytes, ttl: Duration::from_secs(self.d.policy.upload_url_ttl_secs) };
        let url = self.d.artifacts.signed_upload(&key, &policy).await.map_err(blob_err)?;
        Ok(UploadGrant {
            artifact_id: a.artifact.id,
            url: url.url,
            method: HttpMethod::Put,
            headers: url.headers,
            expires_at: url.expires_at,
            already_complete: a.state == ArtifactState::Complete,
        })
    }

    /// `POST /runs/{id}/artifacts/{artifact_id}/complete`: the stored size must equal the
    /// declaration (else the object is deleted and 422); commits `complete` + an `Artifact` event,
    /// and `index.ingest_scip` for a `ScipIndex`. Completing a complete artifact is a no-op.
    pub async fn complete_upload(&self, caller: &RunnerCaller, run: RunId, artifact: ArtifactId, attempt: u16) -> Result<CompleteUploadAck, ProtocolError> {
        let rec = self.fenced(caller, run, attempt).await?;
        let org = rec.org();
        let a = self.d.runs.artifact(org, artifact).await?;
        if a.artifact.run_id != run || a.attempt != attempt {
            return Err(AppError::NotFound("artifact".into()).into());
        }
        if a.state == ArtifactState::Complete {
            return Ok(CompleteUploadAck { artifact_id: artifact, state: ArtifactState::Complete });
        }
        let key = BlobKey::parse(&a.artifact.key).map_err(blob_err)?;
        let meta = match self.d.artifacts.head(&key).await {
            Ok(m) => m,
            Err(BlobError::NotFound) => return Err(rejected("artifact not uploaded", "artifact", "no object at the signed URL")),
            Err(e) => return Err(blob_err(e).into()),
        };
        if meta.bytes != a.artifact.bytes {
            self.d.artifacts.delete(&key).await.map_err(blob_err)?;
            return Err(rejected(
                "uploaded size differs from the declaration",
                "bytes",
                format!("declared {}, stored {}", a.artifact.bytes, meta.bytes),
            ));
        }
        let now = self.now();
        let mut w = RunWrite {
            complete_artifacts: vec![(artifact, meta.bytes)],
            events: vec![RunEventDraft { run_id: run, attempt, at: now, kind: RunEventKind::Artifact(artifact) }],
            ..RunWrite::default()
        };
        if a.artifact.kind == ArtifactKind::ScipIndex
            && let (Some(repo_id), Some(sha)) = (rec.spec.repo_id, rec.spec.sha)
        {
            w.jobs.push(NewJob::new(JobPayload::IndexIngestScip { org_id: org, repo_id, sha, run_id: run, artifact_id: artifact, key: a.artifact.key.clone() }));
        }
        self.d.runs.commit(org, w).await?;
        Ok(CompleteUploadAck { artifact_id: artifact, state: ArtifactState::Complete })
    }

    /// `POST /runs/{id}/downloads`: re-signs the run's own source bundle and the artifacts its
    /// specs reference; anything else is 403.
    pub async fn refresh_downloads(&self, caller: &RunnerCaller, run: RunId, req: RefreshDownloadsRequest) -> Result<Vec<Download>, ProtocolError> {
        let rec = self.fenced(caller, run, req.attempt).await?;
        let mut out = Vec::with_capacity(req.refs.len());
        for what in req.refs {
            out.push(self.download(rec.org(), &rec, what).await?);
        }
        Ok(out)
    }
}
