//! Re-deriving `FixGatePassed` (fix.md "Proof tokens are never persisted or deserialized"): the
//! token always comes from `gates::fix` over the stored `FixResult`, the Verify run's spec and
//! result, and the change set `AcceptFixResult` stored on the gate row ([`FixGateDetails`]).
//!
//! Sources of proof for a Fix run: the passed Fix gate's Verify run, or (F3 rebase flow) a
//! finished rebase Verify run at the moved default head, whose change set binds the new base and
//! the rebased patch artifact.

use std::sync::Arc;

use mage_core::domain::fix::{ApplyMode, ChangeSet};
use mage_core::domain::gates::{self, FixGate};
use mage_core::domain::{
    CauseId, CommitSha, FixGatePassed, FixResult, GateFailure, GateKind, GateStatus, RunKind, RunResult, RunStatus, RunSubject, VerifyResult,
    VerifySpec,
};
use mage_core::domain::{OrgId, RunId};
use serde::{Deserialize, Serialize};

use crate::error::AppError;
use crate::ports::runs::{GateRecord, RunQuery, RunRecord, RunStore};

/// Stored on the Fix `GateRecord.details` by `AcceptFixResult`; read back to re-derive the token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FixGateDetails {
    pub change_set: ChangeSet,
    /// Recipe `[fix].lint` set with `lint_mode = "required"`.
    pub lint_required: bool,
}

impl FixGateDetails {
    pub fn to_json(&self) -> Result<serde_json::Value, AppError> {
        serde_json::to_value(self).map_err(|e| AppError::Internal(format!("fix gate details: {e}")))
    }

    pub fn from_gate(gate: &GateRecord) -> Result<FixGateDetails, AppError> {
        let details = gate.details.clone().ok_or_else(|| AppError::Internal(format!("fix gate {} has no change set", gate.gate.id)))?;
        serde_json::from_value(details).map_err(|e| AppError::Internal(format!("fix gate {} details: {e}", gate.gate.id)))
    }
}

/// The passed Fix gate of a Fix run and everything it was derived from.
#[derive(Debug, Clone)]
pub struct GateProof {
    pub token: FixGatePassed,
    pub fix_run: RunRecord,
    pub fix: FixResult,
    pub gate: GateRecord,
    pub details: FixGateDetails,
    pub verify_run: RunRecord,
    pub verify: VerifyResult,
}

pub(crate) fn fix_result(run: &RunRecord) -> Result<FixResult, AppError> {
    match &run.run.result {
        Some(RunResult::Fix(f)) => Ok(f.clone()),
        _ => Err(AppError::Invalid(format!("run {} has no fix result", run.id()))),
    }
}

pub(crate) fn verify_result(run: &RunRecord) -> Result<VerifyResult, AppError> {
    match &run.run.result {
        Some(RunResult::Verify(v)) => Ok(v.clone()),
        _ => Err(AppError::Invalid(format!("verify run {} has no result", run.id()))),
    }
}

pub(crate) fn cause_of(run: &RunRecord) -> Result<CauseId, AppError> {
    match run.run.subject {
        RunSubject::Cause(c) => Ok(c),
        _ => Err(AppError::Invalid(format!("run {} is not about a cause", run.id()))),
    }
}

/// `true` for the F3 rebase Verify spec (`AllowOffset` + upstream check).
pub fn is_rebase_spec(spec: Option<&VerifySpec>) -> bool {
    matches!(spec, Some(VerifySpec::Fix { mode: ApplyMode::AllowOffset, upstream_check: true, .. }))
}

#[derive(Clone)]
pub struct FixProof {
    runs: Arc<dyn RunStore>,
}

impl FixProof {
    pub fn new(runs: Arc<dyn RunStore>) -> Self {
        Self { runs }
    }

    /// `gates::fix` for the Verify run of a Fix gate (runs' `EvaluateGate`).
    pub fn evaluate_gate(fix_run: &RunRecord, gate: &GateRecord, verify_run: &RunRecord) -> Result<Result<FixGatePassed, GateFailure>, AppError> {
        let fix = fix_result(fix_run)?;
        let verify = verify_result(verify_run)?;
        let details = FixGateDetails::from_gate(gate)?;
        let spec = verify_run.spec.verify.as_ref().ok_or_else(|| AppError::Internal(format!("verify run {} has no spec", verify_run.id())))?;
        let input = FixGate {
            gate: gate.gate.id,
            fix_run: fix_run.id(),
            cause: cause_of(fix_run)?,
            change_set: &details.change_set,
            spec,
            lint_required: details.lint_required,
        };
        Ok(gates::fix(&input, &fix, &verify))
    }

    /// The token of a `Succeeded` Fix run's passed gate.
    pub async fn gate_proof(&self, org: OrgId, fix_run: RunId) -> Result<GateProof, AppError> {
        let run = self.runs.get(org, fix_run).await?;
        if run.run.kind != RunKind::Fix {
            return Err(AppError::Invalid(format!("run {fix_run} is not a fix run")));
        }
        if run.run.status != RunStatus::Succeeded {
            return Err(AppError::Invalid("fix gate not passed".into()));
        }
        let gates = self.runs.gates(org, fix_run).await?;
        let gate = gates
            .into_iter()
            .filter(|g| g.gate.kind == GateKind::Fix && matches!(g.gate.status, GateStatus::Passed { .. }))
            .max_by_key(|g| g.gate.attempt)
            .ok_or_else(|| AppError::Invalid("fix gate not passed".into()))?;
        let verify_id = gate.gate.verify_run.ok_or_else(|| AppError::Invalid("fix gate not passed".into()))?;
        let verify_run = self.runs.get(org, verify_id).await?;
        let token = match Self::evaluate_gate(&run, &gate, &verify_run)? {
            Ok(token) => token,
            Err(failure) => {
                tracing::error!(%fix_run, gate = %gate.gate.id, %failure, "stored fix gate no longer re-derives");
                return Err(AppError::Invalid("fix gate not passed".into()));
            }
        };
        Ok(GateProof {
            token,
            fix: fix_result(&run)?,
            details: FixGateDetails::from_gate(&gate)?,
            verify: verify_result(&verify_run)?,
            fix_run: run,
            gate,
            verify_run,
        })
    }

    /// Rebase Verify runs of `fix_run` at `head` (newest first).
    pub async fn rebase_runs(&self, org: OrgId, fix_run: RunId, head: CommitSha) -> Result<Vec<RunRecord>, AppError> {
        let q = RunQuery { kind: Some(RunKind::Verify), parent: Some(fix_run), newest_first: true, ..RunQuery::default() };
        let runs = self.runs.list(org, &q).await?;
        Ok(runs.into_iter().filter(|r| r.spec.sha == Some(head) && is_rebase_spec(r.spec.verify.as_ref())).collect())
    }

    /// `gates::fix` for a finished rebase Verify run: the change set binds `H` (the run's sha) and
    /// the rebased patch artifact (sha256 from its row); the gate id is the original passed gate.
    pub async fn rebase_proof(&self, org: OrgId, proof: &GateProof, verify_run: &RunRecord) -> Result<Result<FixGatePassed, GateFailure>, AppError> {
        let verify = verify_result(verify_run)?;
        let spec = verify_run.spec.verify.as_ref().ok_or_else(|| AppError::Internal(format!("verify run {} has no spec", verify_run.id())))?;
        let head = verify_run.spec.sha.ok_or_else(|| AppError::Internal(format!("rebase verify {} has no sha", verify_run.id())))?;
        let input = |change_set: &ChangeSet| -> Result<FixGatePassed, GateFailure> {
            gates::fix(
                &FixGate {
                    gate: proof.gate.gate.id,
                    fix_run: proof.fix_run.id(),
                    cause: proof.token.cause(),
                    change_set,
                    spec,
                    lint_required: proof.details.lint_required,
                },
                &proof.fix,
                &verify,
            )
        };
        let Some(rebased) = verify.rebased_patch else {
            // The run stopped before re-diffing. `gates::fix` checks the rebased patch before the
            // outcome and the upstream attempt, so classify the stop here.
            return Ok(Err(unrebased_failure(spec, &verify)));
        };
        let rec = self.runs.artifact(org, rebased).await?;
        let change_set = ChangeSet {
            base_sha: head,
            patch: rebased,
            patch_sha256: rec.artifact.sha256,
            regression: proof.details.change_set.regression.clone(),
        };
        Ok(input(&change_set))
    }
}

/// Why a rebase Verify run ended without a rebased patch: the unpatched script already passes on
/// the new base (upstream fixed), the patch did not apply (conflict), or boot / infra failed.
fn unrebased_failure(spec: &VerifySpec, verify: &VerifyResult) -> GateFailure {
    use mage_core::domain::{AttemptKind, GateFailureCode as Code, VerifyOutcome};
    let fail = |code: Code, reason: &str| GateFailure { code, reason: reason.to_owned() };
    if let VerifySpec::Fix { signature, upstream_check: true, .. } = spec {
        let upstream = verify.attempts.iter().find(|a| a.kind == AttemptKind::UpstreamScript);
        if upstream.is_some_and(|a| a.script_passed && !signature.matches_any(&a.observed)) {
            return fail(Code::AlreadyFixedUpstream, "the unpatched repro script passes on the new base");
        }
    }
    match &verify.outcome {
        VerifyOutcome::PatchRejected { reason } => fail(Code::RebaseConflict, reason),
        VerifyOutcome::BootFailed { reason } => fail(Code::BootFailed, reason),
        VerifyOutcome::Errored { reason } => fail(Code::VerifyErrored, reason),
        VerifyOutcome::Completed => fail(Code::SpecMismatch, "rebase verify reported no rebased patch"),
    }
}
