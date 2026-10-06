//! `ServerLink`: everything the runner says to the server over `/runner/v1` (runs.md "`ServerLink`
//! (owned here)"). The adapter (`adapters::driven::link`) owns transient retries, per-call
//! deadlines and the runner-token lifecycle; use cases only see final outcomes.

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use mage_core::domain::{ArtifactId, RunId, RunStatus};
use mage_core::wire::runner::{
    ArtifactSpec, Capabilities, CommandBatch, Download, DownloadRef, EventsAck, FinishReport, HeartbeatAck, HeldRun, LeaseRequest,
    OutboundEvent, RegistrationAck, Rejection, RunAssignment, RunnerPhase, SubmitAck, UploadGrant, WireRunResult,
};
use uuid::Uuid;

use crate::error::AppError;

/// 409 answers of `/runner/v1` (runs.md "Errors").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkConflict {
    /// Fence mismatch, or the lease expired and was taken: self-fence.
    LeaseLost,
    /// The run is already terminal: graceful teardown, `finish(Done)`.
    RunTerminal(RunStatus),
    /// e.g. a result while `AwaitingGate`: keep state, tool error to the agent.
    InvalidState(RunStatus),
    /// Idempotency key reused with a different body (runner bug).
    IdempotencyMismatch,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LinkError {
    #[error("not found")]
    NotFound,
    #[error("conflict: {0:?}")]
    Conflict(LinkConflict),
    #[error("invalid: {0}")]
    Invalid(String),
    /// 422; returned to the agent as a tool error.
    #[error("rejected: {}", .0.message)]
    Rejected(Rejection),
    /// After one re-auth attempt.
    #[error("unauthorized")]
    Unauthorized,
    /// Runner revoked / run out of scope / download not allowed.
    #[error("forbidden")]
    Forbidden,
    /// 413: batch or artifact over the cap (split the batch, or skip the artifact).
    #[error("too large: {0}")]
    TooLarge(String),
    /// Retries exhausted for this call's deadline.
    #[error("unavailable: {0}")]
    Unavailable(String),
}

impl LinkError {
    pub const fn is_lease_lost(&self) -> bool {
        matches!(self, LinkError::Conflict(LinkConflict::LeaseLost))
    }
}

impl From<LinkError> for AppError {
    fn from(e: LinkError) -> Self {
        match e {
            LinkError::NotFound => AppError::NotFound("server: run or artifact".into()),
            LinkError::Conflict(c) => AppError::Conflict(format!("server: {c:?}")),
            LinkError::Invalid(m) => AppError::Invalid(m),
            LinkError::Rejected(r) => AppError::Invalid(r.message),
            LinkError::TooLarge(m) => AppError::Invalid(format!("too large: {m}")),
            LinkError::Unauthorized => AppError::Unavailable("server: unauthorized".into()),
            LinkError::Forbidden => AppError::Unavailable("server: forbidden".into()),
            LinkError::Unavailable(m) => AppError::Unavailable(m),
        }
    }
}

#[async_trait]
pub trait ServerLink: Send + Sync {
    /// `POST /register`. The adapter supplies the grant from its credential file (Bootstrap on
    /// first start, when it holds no `runner_id`; an Assertion after), its per-process
    /// `instance_id` and the runner version. `held` = `RunnerState::active_runs` at startup.
    /// Later token refreshes re-register with the same capabilities and instance.
    async fn register(&self, capabilities: &Capabilities, held: &[HeldRun]) -> Result<RegistrationAck, LinkError>;
    /// Long-poll up to `req.wait` (server caps at 25 s). `Ok(None)` = nothing to do.
    async fn lease(&self, req: &LeaseRequest) -> Result<Option<RunAssignment>, LinkError>;
    async fn heartbeat(&self, run: RunId, attempt: u16, phase: RunnerPhase) -> Result<HeartbeatAck, LinkError>;
    async fn push_events(&self, run: RunId, attempt: u16, events: &[OutboundEvent]) -> Result<EventsAck, LinkError>;
    async fn poll_commands(&self, run: RunId, attempt: u16, after: u64, wait: Duration) -> Result<CommandBatch, LinkError>;
    async fn request_upload(&self, run: RunId, attempt: u16, spec: &ArtifactSpec) -> Result<UploadGrant, LinkError>;
    /// PUT the file to the signed URL (streamed). The runner token is never sent to the blob host.
    async fn upload(&self, grant: &UploadGrant, file: &Path) -> Result<(), LinkError>;
    async fn complete_upload(&self, run: RunId, attempt: u16, artifact: ArtifactId) -> Result<(), LinkError>;
    async fn refresh_downloads(&self, run: RunId, attempt: u16, refs: &[DownloadRef]) -> Result<Vec<Download>, LinkError>;
    /// GET a signed URL to `dest`, verifying length and sha256; refreshes an expired URL once.
    async fn download(&self, run: RunId, attempt: u16, dl: &Download, dest: &Path) -> Result<(), LinkError>;
    async fn submit_result(&self, run: RunId, attempt: u16, submission_id: Uuid, result: &WireRunResult) -> Result<SubmitAck, LinkError>;
    async fn finish(&self, run: RunId, attempt: u16, report: &FinishReport) -> Result<RunStatus, LinkError>;
}
