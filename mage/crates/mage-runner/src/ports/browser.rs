//! `BrowserRunner` (executor.md "`BrowserRunner`"): run a Playwright script against an `Env` with
//! recording. The adapter (`adapters::driven::playwright`) runs everything through `Env::exec`,
//! `Env::write`, `Env::read` and `Env::export`, so it works unchanged on every executor.

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use mage_core::domain::ArtifactKind;

use crate::error::AppError;
use crate::ports::executor::{Env, ExecError, Exit, RelPath};

#[async_trait]
pub trait BrowserRunner: Send + Sync {
    async fn run(&self, env: &dyn Env, script: &BrowserScript, opts: &RecordOpts) -> Result<BrowserOutcome, BrowserError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserScript {
    pub source: ScriptSource,
    /// Test title filter (Playwright `grep`).
    pub grep: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptSource {
    /// A script in the env's workspace (session runs: the agent's own file).
    Workspace(RelPath),
    /// Pinned bytes (Verify runs: the downloaded artifact, never the agent's copy).
    Inline { file_name: String, bytes: Vec<u8> },
}

impl ScriptSource {
    /// File name the script keeps inside the invocation dir; its extension decides `ScriptKind`.
    pub fn file_name(&self) -> &str {
        match self {
            ScriptSource::Workspace(rel) => rel.as_str().rsplit('/').next().unwrap_or(rel.as_str()),
            ScriptSource::Inline { file_name, .. } => file_name.rsplit('/').next().unwrap_or(file_name),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordOpts {
    pub video: bool,
    pub trace: bool,
    pub screenshots: ScreenshotMode,
    pub har: bool,
    /// Whole invocation (`globalTimeout`).
    pub timeout: Duration,
    /// Per test.
    pub test_timeout: Duration,
    /// Runner-local dir; exported artifacts land here.
    pub outbox: PathBuf,
    /// Artifact name prefix, e.g. `explore-3`, `verify-r1` (lowercase; artifact names are
    /// `<label>.<file>`).
    pub label: String,
}

impl RecordOpts {
    /// Everything off: the `submit_diagnosis` precheck run.
    pub fn none(timeout: Duration, outbox: PathBuf, label: impl Into<String>) -> RecordOpts {
        RecordOpts {
            video: false,
            trace: false,
            screenshots: ScreenshotMode::Off,
            har: false,
            timeout,
            test_timeout: timeout,
            outbox,
            label: label.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScreenshotMode {
    Off,
    OnFailure,
    On,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserOutcome {
    /// True only for `BrowserStatus::Passed`.
    pub passed: bool,
    pub status: BrowserStatus,
    /// First failure, ANSI stripped, ≤ 4 KiB.
    pub error_text: Option<String>,
    pub failures: Vec<TestFailure>,
    pub video: Option<LocalArtifact>,
    pub trace: Option<LocalArtifact>,
    pub screenshots: Vec<LocalArtifact>,
    pub har: Option<LocalArtifact>,
    pub report: Option<LocalArtifact>,
    /// HAR statuses ≥ 400 plus `mage.http_status` annotations, in order, deduplicated.
    pub http_statuses: Vec<u16>,
    pub stdout_tail: String,
    pub duration: Duration,
}

impl BrowserOutcome {
    /// Every local file of this outcome (video, trace, screenshots, HAR, report).
    pub fn artifacts(&self) -> impl Iterator<Item = &LocalArtifact> {
        self.video
            .iter()
            .chain(self.trace.iter())
            .chain(self.screenshots.iter())
            .chain(self.har.iter())
            .chain(self.report.iter())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserStatus {
    Passed,
    Failed,
    TimedOut,
    Interrupted,
    NoTests,
    Crashed { exit: Exit },
}

impl BrowserStatus {
    /// Wire spelling used by `playwright_run` and `VerifyAttempt.status`.
    pub fn as_str(&self) -> &'static str {
        match self {
            BrowserStatus::Passed => "passed",
            BrowserStatus::Failed => "failed",
            BrowserStatus::TimedOut => "timed_out",
            BrowserStatus::Interrupted => "interrupted",
            BrowserStatus::NoTests => "no_tests",
            BrowserStatus::Crashed { .. } => "crashed",
        }
    }
}

/// One failing result's error. `message` is ANSI-stripped; `title` is the title path joined with
/// ` › `; `location` is `<file>:<line>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestFailure {
    pub title: String,
    pub message: String,
    pub location: Option<String>,
}

/// A file exported to the outbox, ready for `ServerLink::request_upload`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalArtifact {
    pub kind: ArtifactKind,
    /// Artifact name (`[a-z0-9][a-z0-9._-]{0,127}`).
    pub name: String,
    pub path: PathBuf,
    pub bytes: u64,
    pub sha256: [u8; 32],
    pub content_type: String,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum BrowserError {
    /// Script missing.
    #[error("not found: {0}")]
    NotFound(String),
    #[error("conflict: {0}")]
    Conflict(String),
    /// Not a Playwright script kind.
    #[error("invalid: {0}")]
    Invalid(String),
    /// Toolchain or browsers missing.
    #[error("unavailable: {0}")]
    Unavailable(String),
    #[error(transparent)]
    Env(#[from] ExecError),
}

impl From<BrowserError> for AppError {
    fn from(err: BrowserError) -> Self {
        match err {
            BrowserError::NotFound(m) => AppError::NotFound(m),
            BrowserError::Conflict(m) => AppError::Conflict(m),
            BrowserError::Invalid(m) => AppError::Invalid(m),
            BrowserError::Unavailable(m) => AppError::Unavailable(m),
            BrowserError::Env(e) => AppError::from(e),
        }
    }
}
