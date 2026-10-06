//! Fix path (fix.md): F1 fix brief, F2 Mage fixes (fix run + fix gate), F3 pull request.
//!
//! | Use case | Module | Driven by |
//! |---|---|---|
//! | `BuildFixBrief` | [`build_brief`] | job `fix.brief` |
//! | `StartFixRun` | [`start`] | `POST …/causes/{cause}/fix` |
//! | `AcceptFixResult` (impl `runs::FixGate`) | [`accept`] | runs `SubmitResult` / `EvaluateGate` |
//! | `ApplyFixOutcome` (impl `runs::KindOutcome`) | [`outcome`] | job `runs.apply_outcome` |
//! | `OpenPullRequest`, `RequestPullRequest` | [`open_pr`] | job `fix.open_pr`, `POST …/pull-request` |
//! | `OnPullRequestEvent` | [`pr_event`] | job `fix.pr_event` |
//! | `GetFixBrief` | [`query`] | `GET …/fix-brief` |
//!
//! Proof tokens are never stored: [`proof::FixProof`] re-derives `FixGatePassed` from the stored
//! `FixResult`, the gate's `VerifyResult` and the change set kept on the gate row.
//! Notifications only ever go out as `notify.ticket` jobs.

pub mod accept;
pub mod brief;
pub mod build_brief;
pub mod media;
pub mod open_pr;
pub mod outcome;
pub mod pr_body;
pub mod pr_event;
pub mod proof;
pub mod query;
pub mod start;
pub mod text;

#[cfg(test)]
mod golden;
#[cfg(test)]
pub(crate) mod tests;

use std::sync::{Arc, OnceLock};

use mage_core::domain::recipe::RecipeSpec;
use mage_core::domain::{ArtifactId, ArtifactKind, Notice, OrgId, RepoId, RecipeHash, RunEventKind, RunId, ScriptKind, TicketId};
use mage_core::ports::blob::{ArtifactStore, BlobError, BlobKey};
use mage_core::ports::clock::Clock;
use mage_core::ports::ids::IdGen;
use mage_core::ports::jobs::{JobError, JobQueue, NewJob};
use mage_core::ports::knowledge::KnowledgeStore;
use mage_core::ports::repos::{RepoStore, RepoStoreError};
use mage_core::domain::JobPayload;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use url::Url;

use crate::app::knowledge_query::SourceReader;
use crate::app::runs::RunManager;
use crate::error::AppError;
use crate::ports::org::OrgStore;
use crate::ports::pull_requests::PullRequests;
use crate::ports::runs::{ArtifactRecord, RunEventDraft, RunStore, RunWrite};
use crate::ports::secretscan::SecretScanner;
use crate::ports::ticket::TicketStore;

pub use accept::AcceptFixResult;
pub use build_brief::{BriefOutcome, BuildFixBrief};
pub use open_pr::{OpenPrOutcome, OpenPullRequest, RequestPullRequest};
pub use outcome::ApplyFixOutcome;
pub use pr_event::{OnPullRequestEvent, PrEventOutcome};
pub use proof::{FixGateDetails, FixProof};
pub use query::GetFixBrief;
pub use start::{StartFix, StartFixRun};

/// Fix-area configuration (`[server.fix]`).
#[derive(Debug, Clone, Deserialize)]
pub struct FixConfig {
    /// Public UI origin for links in briefs, PRs and regression headers (`{ui_url}/runs/{id}`).
    pub public_url: Url,
    /// GitHub web root for permalinks.
    #[serde(default = "default_github_web")]
    pub github_web: Url,
    /// Repro scripts up to this size are inlined into the brief (fix.md: 64 KiB).
    #[serde(default = "default_script_inline_max")]
    pub script_inline_max_bytes: u64,
    /// How long `fix.open_pr` waits between looks at a running rebase verify.
    #[serde(default = "default_rebase_poll_secs")]
    pub rebase_poll_secs: u64,
    /// Signed download URL lifetime for brief links in the API.
    #[serde(default = "default_brief_url_ttl_secs")]
    pub brief_url_ttl_secs: u64,
}

#[allow(clippy::expect_used)]
fn default_github_web() -> Url {
    Url::parse("https://github.com/").expect("static URL")
}

fn default_script_inline_max() -> u64 {
    64 * 1024
}

fn default_rebase_poll_secs() -> u64 {
    120
}

fn default_brief_url_ttl_secs() -> u64 {
    15 * 60
}

impl FixConfig {
    pub fn new(public_url: Url) -> Self {
        Self {
            public_url,
            github_web: default_github_web(),
            script_inline_max_bytes: default_script_inline_max(),
            rebase_poll_secs: default_rebase_poll_secs(),
            brief_url_ttl_secs: default_brief_url_ttl_secs(),
        }
    }
}

/// The run manager, bound after construction: `RunManager` holds the fix hooks
/// (`AcceptFixResult`, `ApplyFixOutcome`) and the fix use cases call back into it.
#[derive(Clone, Default)]
pub struct LateRunManager(Arc<OnceLock<RunManager>>);

impl LateRunManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind once at composition; later calls are ignored.
    pub fn set(&self, rm: RunManager) {
        let _ = self.0.set(rm);
    }

    pub fn get(&self) -> Result<&RunManager, AppError> {
        self.0.get().ok_or_else(|| AppError::Internal("fix: run manager not bound".into()))
    }
}

/// Everything the fix use cases share.
#[derive(Clone)]
pub struct FixDeps {
    pub tickets: Arc<dyn TicketStore>,
    pub runs: Arc<dyn RunStore>,
    pub repos: Arc<dyn RepoStore>,
    pub orgs: Arc<dyn OrgStore>,
    pub knowledge: Arc<dyn KnowledgeStore>,
    pub reader: SourceReader,
    pub artifacts: Arc<dyn ArtifactStore>,
    pub jobs: Arc<dyn JobQueue>,
    pub pull_requests: Arc<dyn PullRequests>,
    pub scanner: Arc<dyn SecretScanner>,
    pub run_manager: LateRunManager,
    pub clock: Arc<dyn Clock>,
    pub ids: Arc<dyn IdGen>,
    pub config: FixConfig,
}

pub(crate) fn repo_err(err: RepoStoreError) -> AppError {
    match err {
        RepoStoreError::NotFound => AppError::NotFound("repo or recipe".into()),
        RepoStoreError::Conflict => AppError::Conflict("repo store conflict".into()),
        RepoStoreError::Invalid(m) => AppError::Invalid(m),
        RepoStoreError::Unavailable(m) => AppError::Unavailable(m),
    }
}

pub(crate) fn blob_err(err: BlobError) -> AppError {
    match err {
        BlobError::NotFound => AppError::NotFound("artifact bytes".into()),
        BlobError::Conflict => AppError::Conflict("artifact bytes".into()),
        BlobError::Invalid(m) => AppError::Invalid(m),
        BlobError::Unavailable(m) => AppError::Unavailable(m),
        BlobError::TooLarge { limit } => AppError::Invalid(format!("artifact larger than {limit} bytes")),
    }
}

pub(crate) fn job_err(err: JobError) -> AppError {
    match err {
        JobError::NotFound => AppError::NotFound("job".into()),
        JobError::Conflict | JobError::LeaseLost => AppError::Conflict("job".into()),
        JobError::Invalid(m) => AppError::Invalid(m),
        JobError::Unavailable(m) => AppError::Unavailable(m),
    }
}

/// `notify.ticket` for `notice` (dedupe `notify:{ticket}:{notice key}`).
pub(crate) fn notify(org: OrgId, ticket: TicketId, notice: Notice) -> NewJob {
    NewJob::new(JobPayload::NotifyTicket { org_id: org, ticket_id: ticket, notice })
}

/// `fix.open_pr` with its documented dedupe key `fix.open_pr:{ticket}:{digest hex}`.
pub(crate) fn open_pr_job(org: OrgId, ticket: TicketId, fix_run: RunId, digest_hex: &str) -> NewJob {
    NewJob::new(JobPayload::FixOpenPr { org_id: org, ticket_id: ticket, fix_run_id: fix_run }).dedupe(format!("fix.open_pr:{ticket}:{digest_hex}"))
}

impl FixDeps {
    /// Artifact row plus its bytes, checked against the row's sha256 (the runner's claim is
    /// re-verified whenever the server acts on the bytes).
    pub(crate) async fn artifact_bytes(&self, org: OrgId, id: ArtifactId, max: u64) -> Result<(ArtifactRecord, Vec<u8>), AppError> {
        let rec = self.runs.artifact(org, id).await?;
        let key = BlobKey::parse(&rec.artifact.key).map_err(blob_err)?;
        let bytes = self.artifacts.get(&key, max).await.map_err(blob_err)?;
        let sha: [u8; 32] = Sha256::digest(&bytes).into();
        if sha != rec.artifact.sha256 {
            return Err(AppError::Invalid(format!("artifact {id} bytes do not match their recorded sha256")));
        }
        Ok((rec, bytes))
    }

    pub(crate) async fn recipe_spec(&self, org: OrgId, repo: RepoId, hash: RecipeHash) -> Result<RecipeSpec, AppError> {
        let recipe = self.repos.get_recipe(org, repo, hash).await.map_err(repo_err)?;
        RecipeSpec::parse(&recipe.body).map_err(|e| AppError::Internal(format!("stored recipe {hash} no longer parses: {e}")))
    }

    /// A `Notice` event on `run` (its current attempt).
    pub(crate) async fn notice(&self, org: OrgId, run: RunId, text: String) -> Result<(), AppError> {
        let rec = self.runs.get(org, run).await?;
        let event = RunEventDraft { run_id: run, attempt: rec.run.attempt, at: self.clock.now(), kind: RunEventKind::Notice(text) };
        self.runs.commit(org, RunWrite { events: vec![event], ..RunWrite::default() }).await?;
        Ok(())
    }
}

/// A repro script artifact as the fix path uses it.
#[derive(Debug, Clone)]
pub struct ScriptInfo {
    pub artifact: ArtifactId,
    pub name: String,
    pub kind: ScriptKind,
    /// Extension used for the regression file (`spec.ts`, `sh`, …).
    pub ext: String,
    pub bytes: u64,
    pub sha256: [u8; 32],
}

impl ScriptInfo {
    pub fn from_record(rec: &ArtifactRecord) -> Result<ScriptInfo, AppError> {
        if rec.artifact.kind != ArtifactKind::Script {
            return Err(AppError::Invalid(format!("artifact {} is not a script", rec.artifact.id)));
        }
        let kind = ScriptKind::from_file_name(&rec.name).ok_or_else(|| AppError::Invalid(format!("{:?} is not a repro script name", rec.name)))?;
        const EXTS: [&str; 6] = ["spec.ts", "spec.js", "spec.mjs", "test.ts", "test.js", "sh"];
        let ext = EXTS.iter().find(|e| rec.name.ends_with(&format!(".{e}"))).map(|e| (*e).to_owned()).unwrap_or_else(|| "sh".into());
        Ok(ScriptInfo { artifact: rec.artifact.id, name: rec.name.clone(), kind, ext, bytes: rec.artifact.bytes, sha256: rec.artifact.sha256 })
    }

    /// Where the runner places it in a fork and the brief tells humans to save it.
    pub fn repro_path(&self, cause: mage_core::domain::CauseId) -> String {
        format!(".mage/repro/{cause}/{}", self.name)
    }
}

/// Recipe literal values allowed by the secret scanner (fake keys and the fake test login).
pub(crate) fn recipe_fake_values(recipe: &RecipeSpec) -> Vec<String> {
    let mut out: Vec<String> = recipe.env.values().filter(|v| !v.contains("${")).cloned().collect();
    for svc in recipe.services.values() {
        out.extend(svc.env.values().filter(|v| !v.contains("${")).cloned());
    }
    if let Some(t) = &recipe.test {
        out.extend(t.env.values().filter(|v| !v.contains("${")).cloned());
    }
    if let Some(login) = recipe.browser.as_ref().and_then(|b| b.login.as_ref()) {
        out.push(login.username.clone());
        out.push(login.password.clone());
    }
    out.sort();
    out.dedup();
    out
}
