//! What the run manager needs from the modules that own a run kind (triage.md, fix.md). The
//! owners implement these traits on their use cases (`triage::EvidenceGate`,
//! `triage::ApplyTriageOutcome`, `fix::AcceptFixResult`, `fix::ApplyFixOutcome`); the run manager
//! owns every commit, gate row, status transition and command.

use async_trait::async_trait;

use mage_core::domain::{FixResult, GateFailure, OrgId, RunId, TriageResult, VerifySpec};
use mage_core::wire::runner::{Rejection, TriageSubmission};

use crate::error::AppError;
use crate::ports::runs::{GateRecord, RunRecord};

/// The inline Evidence gate's decision (triage.md "Decision table").
#[derive(Debug, Clone, PartialEq)]
pub enum EvidenceDecision {
    /// Malformed submission: 422 with `rejection`; no gate row, no attempt consumed.
    Invalid { rejection: Rejection },
    /// Gate passed: `Succeeded` with `result`; `message` is the agent's tool result; `notices`
    /// become `Notice` events.
    Accept { result: TriageResult, details: serde_json::Value, message: String, notices: Vec<String> },
    /// Gate attempt failed with attempts left: the run stays `Running`; 422 with `rejection`
    /// (`rejection.message` is the feedback stored on the gate row; `gate` is filled in by runs).
    Reject { details: serde_json::Value, rejection: Rejection, reason: String, attempts_left: u8 },
    /// Final attempt with zero valid causes: `Failed` in one commit; 422 with `rejection`.
    Fail { details: serde_json::Value, rejection: Rejection, reason: String },
}

/// Triage's part of `SubmitResult` and of the salvage rule.
#[async_trait]
pub trait TriageGate: Send + Sync {
    /// `EvidenceGate::evaluate_submission` for gate attempt `attempt` (1-based).
    async fn evaluate(&self, org: OrgId, run: &RunRecord, attempt: u8, submission: TriageSubmission) -> Result<EvidenceDecision, AppError>;

    /// `triage::salvage` over the run's gate rows (newest Evidence row decides). `Some` ends the
    /// run `Succeeded` with the salvaged causes instead of `TimedOut` / `Errored(NoResult)`.
    fn salvage(&self, run: &RunRecord, gates: &[GateRecord]) -> Option<TriageResult>;
}

/// `AcceptFixResult`'s answer for one `submit_fix`.
#[derive(Debug, Clone, PartialEq)]
pub enum FixAcceptance {
    /// Patch policy / parse / secret-scan violation: a failed Fix gate attempt without a Verify run.
    Rejected { reason: String, details: String },
    /// Start the Fix gate: a Verify run with `verify`; `details` is stored on the gate row and
    /// handed back to [`FixGate::judge`].
    Accepted { verify: VerifySpec, details: serde_json::Value },
}

/// Fix's part of `SubmitResult` and `EvaluateGate`.
#[async_trait]
pub trait FixGate: Send + Sync {
    /// fix.md `AcceptFixResult` for gate attempt `attempt`; the patch artifact was checked to be a
    /// complete `Patch` of this run.
    async fn accept(&self, org: OrgId, run: &RunRecord, attempt: u8, result: &FixResult) -> Result<FixAcceptance, AppError>;

    /// `gates::fix` for the finished Verify run of `gate`. `Ok(Err(_))` is a gate failure;
    /// `Err(_)` an error re-deriving the inputs (the job fails).
    async fn judge(&self, org: OrgId, fix_run: &RunRecord, gate: &GateRecord, verify_run: &RunRecord)
    -> Result<Result<(), GateFailure>, AppError>;
}

/// A kind owner's `runs.apply_outcome` projection (idempotent; called at creation and at every
/// terminal transition).
#[async_trait]
pub trait KindOutcome: Send + Sync {
    async fn apply(&self, org: OrgId, run: RunId) -> Result<(), AppError>;
}

/// The owners' implementations, injected into the run manager.
#[derive(Clone)]
pub struct RunHooks {
    pub triage_gate: std::sync::Arc<dyn TriageGate>,
    pub triage_outcome: std::sync::Arc<dyn KindOutcome>,
    pub fix_gate: std::sync::Arc<dyn FixGate>,
    /// Fix runs, and terminal Verify runs without a gate row (`TestBaseline`, rebase verify).
    pub fix_outcome: std::sync::Arc<dyn KindOutcome>,
}
