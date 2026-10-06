//! UI reads and starts (server.md catalog): run list/detail, event history, artifacts and their
//! bytes, "Run diagnostic" on a cause, Learn on a repo.

use std::ops::Range;

use mage_core::domain::{
    ArtifactId, ArtifactKind, CauseId, CommitSha, HarnessKind, OrgId, RepoId, RepoVersionKey, RunId, RunKind, RunSubject, TicketId, UserId,
};
use mage_core::ports::blob::BlobKey;
use mage_core::wire::api::{ArtifactView, Paged, RunDetail, RunEventPage, RunListQuery, RunSummary};

use crate::app::runs::{CreateRun, RunManager, blob_err, repo_err};
use crate::error::AppError;
use crate::ports::runs::{ArtifactRecord, ArtifactState, RunQuery, RunRecord};

/// Default and maximum page sizes of the UI lists.
pub const LIST_DEFAULT: u32 = 50;
pub const EVENTS_DEFAULT: u32 = 200;
pub const EVENTS_MAX: u32 = 500;

pub fn artifact_view(a: &ArtifactRecord) -> ArtifactView {
    ArtifactView {
        id: a.artifact.id,
        run_id: a.artifact.run_id,
        kind: a.artifact.kind,
        name: a.name.clone(),
        content_type: a.content_type.clone(),
        bytes: a.artifact.bytes,
        sha256: a.artifact.sha256,
        complete: a.state == ArtifactState::Complete,
        created_at: a.artifact.created_at,
    }
}

impl RunManager {
    pub async fn list_runs(&self, org: OrgId, q: &RunListQuery) -> Result<Paged<RunSummary>, AppError> {
        let limit = q.limit.unwrap_or(LIST_DEFAULT).clamp(1, RunQuery::MAX_LIMIT);
        let cursor = match &q.cursor {
            Some(c) => Some(c.parse::<RunId>().map_err(|_| AppError::Invalid("bad cursor".into()))?),
            None => None,
        };
        let query = RunQuery {
            kind: q.kind,
            subject: q.cause.map(RunSubject::Cause),
            ticket: q.ticket,
            repo: q.repo,
            statuses: q.status.into_iter().collect(),
            newest_first: true,
            limit,
            cursor,
            ..RunQuery::default()
        };
        let runs = self.d.runs.list(org, &query).await?;
        let next_cursor = (runs.len() == limit as usize).then(|| runs.last().map(|r| r.id().to_string())).flatten();
        Ok(Paged { items: runs.iter().map(|r| RunSummary::of(&r.run)).collect(), next_cursor })
    }

    pub async fn run_detail(&self, org: OrgId, run: RunId) -> Result<RunDetail, AppError> {
        let rec = self.d.runs.get(org, run).await?;
        let gates = self.d.runs.gates(org, run).await?.into_iter().map(|g| g.gate).collect();
        let verify_runs = self
            .d
            .runs
            .list(org, &RunQuery { parent: Some(run), kind: Some(RunKind::Verify), limit: RunQuery::MAX_LIMIT, ..RunQuery::default() })
            .await?
            .iter()
            .map(|r| RunSummary::of(&r.run))
            .collect();
        let mut detail_run = rec.run.clone();
        let effective = rec.effective_usage(self.now());
        let _ = detail_run.record_usage(effective);
        Ok(RunDetail { run: detail_run, gates, verify_runs })
    }

    pub async fn events_page(&self, org: OrgId, run: RunId, after: Option<u64>, limit: Option<u32>) -> Result<RunEventPage, AppError> {
        let limit = limit.unwrap_or(EVENTS_DEFAULT).clamp(1, EVENTS_MAX);
        let mut events = self.d.runs.events_after(org, run, after.unwrap_or(0), limit + 1).await?;
        let more = events.len() > limit as usize;
        events.truncate(limit as usize);
        Ok(RunEventPage { events, more })
    }

    pub async fn run_artifacts(&self, org: OrgId, run: RunId) -> Result<Vec<ArtifactView>, AppError> {
        Ok(self.d.runs.artifacts(org, run).await?.iter().map(artifact_view).collect())
    }

    pub async fn artifact_meta(&self, org: OrgId, id: ArtifactId) -> Result<ArtifactRecord, AppError> {
        Ok(self.d.runs.artifact(org, id).await?)
    }

    /// The UI may read complete artifacts of its org, except `SourceBundle` and `ScipIndex` (403).
    pub fn viewable(org: OrgId, a: &ArtifactRecord) -> Result<(), AppError> {
        if a.state != ArtifactState::Complete || a.artifact.org_id != org {
            return Err(AppError::NotFound("artifact".into()));
        }
        if matches!(a.artifact.kind, ArtifactKind::SourceBundle | ArtifactKind::ScipIndex) {
            return Err(AppError::Forbidden("artifact kind not viewable".into()));
        }
        Ok(())
    }

    /// Bytes `range` (clamped to the object) of a complete artifact the UI may read
    /// (`SourceBundle` and `ScipIndex` are refused).
    pub async fn artifact_bytes(&self, org: OrgId, a: &ArtifactRecord, range: Range<u64>) -> Result<Vec<u8>, AppError> {
        Self::viewable(org, a)?;
        let key = BlobKey::parse(&a.artifact.key).map_err(blob_err)?;
        self.d.artifacts.get_range(&key, range).await.map_err(blob_err)
    }

    /// "Run diagnostic" on a cause (harness picker).
    pub async fn start_diagnose(&self, org: OrgId, ticket: TicketId, cause: CauseId, harness: Option<HarnessKind>, by: UserId) -> Result<RunRecord, AppError> {
        let c = self.d.tickets.get_cause(org, cause).await?;
        if c.ticket_id != ticket {
            return Err(AppError::NotFound("cause".into()));
        }
        let mut req = CreateRun::new(org, RunKind::Diagnose, RunSubject::Cause(cause));
        req.harness = harness;
        req.requested_by = Some(by);
        self.create_run(req).await
    }

    /// Learn run at `sha` (default: the repo's default head).
    pub async fn start_learn(&self, org: OrgId, repo: RepoId, sha: Option<CommitSha>, harness: Option<HarnessKind>, by: UserId) -> Result<RunRecord, AppError> {
        let sha = match sha {
            Some(s) => s,
            None => self.d.repos.default_head(org, repo).await.map_err(repo_err)?.ok_or_else(|| AppError::Invalid("the repo has no default head yet".into()))?,
        };
        let mut req = CreateRun::new(org, RunKind::Learn, RunSubject::RepoVersion(RepoVersionKey::new(repo, sha)));
        req.harness = harness;
        req.requested_by = Some(by);
        self.create_run(req).await
    }
}
