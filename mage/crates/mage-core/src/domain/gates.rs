//! Proof tokens and the pure gate functions that mint them (domain.md "Gate"; runs.md, fix.md).
//!
//! Tokens have private fields and no public constructor: the only way to obtain one is a passing
//! gate function in this module (or `evidence::check` for `EvidenceChecked`). They are never
//! serialized; consumers re-derive them from stored results (runs.md "EvaluateGate").
//!
//! domain.md writes the gate functions as `gates::repro(&DiagnoseResult, &VerifyResult)` etc. A
//! token must bind the run, gate and subject it proves, which the results alone do not carry, so
//! each function also takes a small input struct naming the gate row and the Verify spec that ran.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::domain::fix::{ApplyMode, ChangeSet, ChangeSetDigest};
use crate::domain::ids::{CauseId, CommitSha, GateId, RecipeHash, RepoId, RunId};
use crate::domain::signature::{FailureSignature, Verdict};
use crate::domain::verify::{AttemptKind, DiagnoseResult, FixResult, VerifyAttempt, VerifyOutcome, VerifyResult, VerifySpec};

/// Why a gate failed. `reason` is rendered into the agent feedback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{code}: {reason}")]
pub struct GateFailure {
    pub code: GateFailureCode,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateFailureCode {
    /// The Verify run ended with an infrastructure error (normally retried before a gate fn runs).
    VerifyErrored,
    /// The Verify spec or results do not belong to this gate (wrong variant, ids, artifacts).
    SpecMismatch,
    /// Boot: services did not come up healthy / recipe step failed.
    BootFailed,
    /// Repro: the diagnosis does not claim `Reproduced` or lacks a signature.
    NotClaimed,
    /// Number of script repetitions differs from the spec.
    WrongAttemptCount,
    /// Repro: a repetition did not observe the expected signature.
    SignatureNotObserved,
    /// Fix: `apply_patch(Exact)` rejected the patch.
    PatchDoesNotApply,
    /// Fix rebase: `apply_patch(AllowOffset)` rejected the patch on the new base.
    RebaseConflict,
    /// Fix: the runner applied different bytes than the change set pins, or no fix checks.
    DigestMismatch,
    /// Fix: the script failed or still observed the signature.
    ReproStillFails,
    TestsFailed,
    LintFailed,
    NotApplicableToSource,
    /// Fix rebase: the unpatched script already passes on the new base.
    AlreadyFixedUpstream,
}

impl fmt::Display for GateFailureCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::VerifyErrored => "verify_errored",
            Self::SpecMismatch => "spec_mismatch",
            Self::BootFailed => "boot_failed",
            Self::NotClaimed => "not_claimed",
            Self::WrongAttemptCount => "wrong_attempt_count",
            Self::SignatureNotObserved => "signature_not_observed",
            Self::PatchDoesNotApply => "patch_does_not_apply",
            Self::RebaseConflict => "rebase_conflict",
            Self::DigestMismatch => "digest_mismatch",
            Self::ReproStillFails => "repro_still_fails",
            Self::TestsFailed => "tests_failed",
            Self::LintFailed => "lint_failed",
            Self::NotApplicableToSource => "not_applicable_to_source",
            Self::AlreadyFixedUpstream => "already_fixed_upstream",
        })
    }
}

fn fail(code: GateFailureCode, reason: impl Into<String>) -> GateFailure {
    GateFailure { code, reason: reason.into() }
}

/// Proof that a recipe booted cleanly at `sha` (Boot gate). Consumed by `Recipe::validate`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootGatePassed {
    gate: GateId,
    learn_run: RunId,
    repo_id: RepoId,
    recipe: RecipeHash,
    sha: CommitSha,
}

impl BootGatePassed {
    pub fn gate(&self) -> GateId {
        self.gate
    }
    pub fn learn_run(&self) -> RunId {
        self.learn_run
    }
    pub fn repo_id(&self) -> RepoId {
        self.repo_id
    }
    pub fn recipe(&self) -> RecipeHash {
        self.recipe
    }
    pub fn sha(&self) -> CommitSha {
        self.sha
    }
}

/// Proof that a clean re-run observed the diagnosed failure on every repetition.
/// Consumed by `Cause::mark_reproduced`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReproGatePassed {
    gate: GateId,
    diagnose_run: RunId,
    cause: CauseId,
    signature: FailureSignature,
}

impl ReproGatePassed {
    pub fn gate(&self) -> GateId {
        self.gate
    }
    pub fn diagnose_run(&self) -> RunId {
        self.diagnose_run
    }
    pub fn cause(&self) -> CauseId {
        self.cause
    }
    pub fn signature(&self) -> &FailureSignature {
        &self.signature
    }
}

/// Proof that exactly `change_set` fixed the failure in a clean environment. Required by
/// `Ticket::fix_ready` and `CommitPlan::build`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixGatePassed {
    gate: GateId,
    fix_run: RunId,
    cause: CauseId,
    change_set: ChangeSet,
    digest: ChangeSetDigest,
}

impl FixGatePassed {
    pub fn gate(&self) -> GateId {
        self.gate
    }
    pub fn fix_run(&self) -> RunId {
        self.fix_run
    }
    pub fn cause(&self) -> CauseId {
        self.cause
    }
    pub fn base_sha(&self) -> CommitSha {
        self.change_set.base_sha
    }
    pub fn change_set(&self) -> &ChangeSet {
        &self.change_set
    }
    pub fn digest(&self) -> ChangeSetDigest {
        self.digest
    }
}

/// The Boot gate row being decided and what was booted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootGate {
    pub gate: GateId,
    pub learn_run: RunId,
    pub repo_id: RepoId,
    pub recipe: RecipeHash,
    pub sha: CommitSha,
}

/// The Repro gate row being decided. `spec` is the Verify run's spec (`VerifySpec::Repro`).
#[derive(Debug, Clone, Copy)]
pub struct ReproGate<'a> {
    pub gate: GateId,
    pub diagnose_run: RunId,
    pub cause: CauseId,
    pub spec: &'a VerifySpec,
}

/// The Fix gate (or rebase Verify) being decided.
///
/// `change_set` is what the token will bind: for a normal gate the change set built by
/// `AcceptFixResult` (base sha, the Fix run's patch); for the rebase flow the new base `H` and the
/// rebased patch artifact named by `VerifyResult.rebased_patch`, with its sha256 from the artifact
/// row. `lint_required` comes from the recipe's `[fix].lint_mode = "required"` with `lint` set.
#[derive(Debug, Clone, Copy)]
pub struct FixGate<'a> {
    pub gate: GateId,
    pub fix_run: RunId,
    pub cause: CauseId,
    pub change_set: &'a ChangeSet,
    pub spec: &'a VerifySpec,
    pub lint_required: bool,
}

fn outcome_failure(outcome: &VerifyOutcome, rejected: GateFailureCode) -> Option<GateFailure> {
    match outcome {
        VerifyOutcome::Completed => None,
        VerifyOutcome::BootFailed { reason } => Some(fail(GateFailureCode::BootFailed, reason.clone())),
        VerifyOutcome::PatchRejected { reason } => Some(fail(rejected, reason.clone())),
        VerifyOutcome::Errored { reason } => Some(fail(GateFailureCode::VerifyErrored, reason.clone())),
    }
}

fn script_attempts(verify: &VerifyResult) -> impl Iterator<Item = &VerifyAttempt> {
    verify.attempts.iter().filter(|a| a.kind == AttemptKind::Script)
}

fn check_repetitions(verify: &VerifyResult, repetitions: u8) -> Result<(), GateFailure> {
    let n = script_attempts(verify).count();
    if repetitions == 0 || n != usize::from(repetitions) {
        return Err(fail(GateFailureCode::WrongAttemptCount, format!("expected {repetitions} script repetitions, got {n}")));
    }
    Ok(())
}

/// Boot gate: the Verify run completed and every boot attempt was healthy.
pub fn boot(input: &BootGate, verify: &VerifyResult) -> Result<BootGatePassed, GateFailure> {
    if let Some(f) = outcome_failure(&verify.outcome, GateFailureCode::SpecMismatch) {
        return Err(f);
    }
    let mut boots = verify.attempts.iter().filter(|a| a.kind == AttemptKind::Boot).peekable();
    if boots.peek().is_none() {
        return Err(fail(GateFailureCode::BootFailed, "no boot attempt recorded"));
    }
    if let Some(a) = boots.find(|a| !a.health_ok) {
        return Err(fail(GateFailureCode::BootFailed, format!("health check failed on attempt {}", a.index)));
    }
    Ok(BootGatePassed { gate: input.gate, learn_run: input.learn_run, repo_id: input.repo_id, recipe: input.recipe, sha: input.sha })
}

/// Repro gate: the diagnosis claims `Reproduced`, and the clean re-run observed a matching
/// signature on every repetition.
pub fn repro(input: &ReproGate<'_>, diagnose: &DiagnoseResult, verify: &VerifyResult) -> Result<ReproGatePassed, GateFailure> {
    let VerifySpec::Repro { script, signature: spec_signature, repetitions } = input.spec else {
        return Err(fail(GateFailureCode::SpecMismatch, "verify spec is not a repro spec"));
    };
    if diagnose.verdict != Verdict::Reproduced {
        return Err(fail(GateFailureCode::NotClaimed, "diagnosis does not claim reproduced"));
    }
    let Some(signature) = &diagnose.signature else {
        return Err(fail(GateFailureCode::NotClaimed, "diagnosis has no failure signature"));
    };
    if spec_signature != signature || diagnose.script.as_ref() != Some(script) {
        return Err(fail(GateFailureCode::SpecMismatch, "verify spec does not match the diagnosis script and signature"));
    }
    if let Some(f) = outcome_failure(&verify.outcome, GateFailureCode::SpecMismatch) {
        return Err(f);
    }
    check_repetitions(verify, *repetitions)?;
    if let Some(a) = script_attempts(verify).find(|a| !signature.matches_any(&a.observed)) {
        return Err(fail(GateFailureCode::SignatureNotObserved, format!("repetition {}: expected signature not observed", a.index)));
    }
    Ok(ReproGatePassed { gate: input.gate, diagnose_run: input.diagnose_run, cause: input.cause, signature: signature.clone() })
}

/// Fix gate (fix.md "Fix gate", checks in order; also the rebase flow).
pub fn fix(input: &FixGate<'_>, fix: &FixResult, verify: &VerifyResult) -> Result<FixGatePassed, GateFailure> {
    let VerifySpec::Fix { patch, signature, repetitions, regression, mode, upstream_check, .. } = input.spec else {
        return Err(fail(GateFailureCode::SpecMismatch, "verify spec is not a fix spec"));
    };
    let change_set = input.change_set;
    if *patch != fix.patch {
        return Err(fail(GateFailureCode::SpecMismatch, "verify spec patch is not the fix run's patch"));
    }
    if *regression != change_set.regression {
        return Err(fail(GateFailureCode::SpecMismatch, "regression file differs from the change set"));
    }
    match (verify.rebased_patch, mode) {
        (Some(rebased), ApplyMode::AllowOffset) if rebased == change_set.patch => {}
        (None, ApplyMode::Exact) if change_set.patch == fix.patch => {}
        _ => {
            return Err(fail(GateFailureCode::SpecMismatch, "change set patch does not match the verified (or rebased) patch"));
        }
    }

    let rejected = match mode {
        ApplyMode::Exact => GateFailureCode::PatchDoesNotApply,
        ApplyMode::AllowOffset => GateFailureCode::RebaseConflict,
    };
    // A rejected patch may still carry an upstream attempt; upstream already fixed wins.
    if *upstream_check {
        if let Some(up) = verify.attempts.iter().find(|a| a.kind == AttemptKind::UpstreamScript) {
            if up.script_passed && !signature.matches_any(&up.observed) {
                return Err(fail(GateFailureCode::AlreadyFixedUpstream, "the unpatched repro script passes on the new base"));
            }
        } else if verify.outcome == VerifyOutcome::Completed {
            return Err(fail(GateFailureCode::SpecMismatch, "upstream check requested but not run"));
        }
    }
    if let Some(f) = outcome_failure(&verify.outcome, rejected) {
        return Err(f);
    }

    let Some(checks) = &verify.fix_checks else {
        return Err(fail(GateFailureCode::DigestMismatch, "verify run reported no fix checks"));
    };
    if checks.applied_patch_sha256 != Some(change_set.patch_sha256) {
        return Err(fail(GateFailureCode::DigestMismatch, "applied patch sha256 differs from the change set"));
    }
    if !checks.boot {
        return Err(fail(GateFailureCode::BootFailed, "rebuild or services failed after applying the patch"));
    }
    check_repetitions(verify, *repetitions)?;
    for a in script_attempts(verify) {
        if !a.script_passed {
            return Err(fail(GateFailureCode::ReproStillFails, format!("repetition {}: repro script did not pass", a.index)));
        }
        if signature.matches_any(&a.observed) {
            return Err(fail(GateFailureCode::ReproStillFails, format!("repetition {}: failure signature still observed", a.index)));
        }
    }
    if checks.test_exit != Some(0) {
        return Err(fail(
            GateFailureCode::TestsFailed,
            match checks.test_exit {
                Some(code) => format!("test command exited {code}"),
                None => "test command did not run".to_string(),
            },
        ));
    }
    if input.lint_required && checks.lint_exit != Some(0) {
        return Err(fail(
            GateFailureCode::LintFailed,
            match checks.lint_exit {
                Some(code) => format!("lint exited {code}"),
                None => "lint did not run".to_string(),
            },
        ));
    }
    if checks.source_applies != Some(true) {
        return Err(fail(GateFailureCode::NotApplicableToSource, "patch does not apply to the source tree at the base sha"));
    }
    Ok(FixGatePassed {
        gate: input.gate,
        fix_run: input.fix_run,
        cause: input.cause,
        change_set: change_set.clone(),
        digest: change_set.digest(),
    })
}

/// Token constructors for tests in other domain modules (e.g. `fix::CommitPlan`, `Cause`).
#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    pub(crate) fn fix_gate_passed(gate: GateId, fix_run: RunId, cause: CauseId, change_set: ChangeSet) -> FixGatePassed {
        let digest = change_set.digest();
        FixGatePassed { gate, fix_run, cause, change_set, digest }
    }

    pub(crate) fn repro_gate_passed(gate: GateId, diagnose_run: RunId, cause: CauseId, signature: FailureSignature) -> ReproGatePassed {
        ReproGatePassed { gate, diagnose_run, cause, signature }
    }

    pub(crate) fn boot_gate_passed(gate: GateId, learn_run: RunId, repo_id: RepoId, recipe: RecipeHash, sha: CommitSha) -> BootGatePassed {
        BootGatePassed { gate, learn_run, repo_id, recipe, sha }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::fix::RegressionFile;
    use crate::domain::ids::ArtifactId;
    use crate::domain::path::RepoPath;
    use crate::domain::signature::{Matcher, SignatureKind};
    use crate::domain::verify::{FixChecks, FixStats};
    use uuid::Uuid;

    fn art(n: u128) -> ArtifactId {
        ArtifactId::from_uuid(Uuid::from_u128(n))
    }
    fn gate_id() -> GateId {
        GateId::from_uuid(Uuid::from_u128(100))
    }
    fn run(n: u128) -> RunId {
        RunId::from_uuid(Uuid::from_u128(n))
    }
    fn cause() -> CauseId {
        CauseId::from_uuid(Uuid::from_u128(200))
    }
    fn sig() -> FailureSignature {
        FailureSignature { kind: SignatureKind::HttpStatus, value: "500".into(), matcher: Matcher::Exact }
    }
    fn obs(v: &str) -> FailureSignature {
        FailureSignature { kind: SignatureKind::HttpStatus, value: v.into(), matcher: Matcher::Exact }
    }
    fn attempt(index: u8, kind: AttemptKind, passed: bool, observed: Vec<FailureSignature>) -> VerifyAttempt {
        VerifyAttempt {
            index,
            kind,
            script_exit: Some(if passed { 0 } else { 1 }),
            script_passed: passed,
            status: None,
            observed,
            health_ok: true,
            duration_ms: 10,
        }
    }
    fn verify(outcome: VerifyOutcome, attempts: Vec<VerifyAttempt>) -> VerifyResult {
        VerifyResult { outcome, observed: None, attempts, artifacts: vec![], fix_checks: None, rebased_patch: None }
    }

    // ---- boot ----

    fn boot_input() -> BootGate {
        BootGate {
            gate: gate_id(),
            learn_run: run(1),
            repo_id: RepoId::from_uuid(Uuid::from_u128(5)),
            recipe: RecipeHash::from_bytes([1; 32]),
            sha: CommitSha::from_bytes([2; 20]),
        }
    }

    #[test]
    fn boot_passes_on_completed_healthy_boot() {
        let v = verify(VerifyOutcome::Completed, vec![attempt(1, AttemptKind::Boot, true, vec![])]);
        let token = boot(&boot_input(), &v).unwrap();
        assert_eq!(token.recipe(), boot_input().recipe);
        assert_eq!(token.sha(), boot_input().sha);
    }

    #[test]
    fn boot_failures() {
        let failed = verify(VerifyOutcome::BootFailed { reason: "migrate".into() }, vec![]);
        assert_eq!(boot(&boot_input(), &failed).unwrap_err().code, GateFailureCode::BootFailed);
        let errored = verify(VerifyOutcome::Errored { reason: "disk".into() }, vec![]);
        assert_eq!(boot(&boot_input(), &errored).unwrap_err().code, GateFailureCode::VerifyErrored);
        let none = verify(VerifyOutcome::Completed, vec![]);
        assert_eq!(boot(&boot_input(), &none).unwrap_err().code, GateFailureCode::BootFailed);
        let mut unhealthy = attempt(1, AttemptKind::Boot, true, vec![]);
        unhealthy.health_ok = false;
        let v = verify(VerifyOutcome::Completed, vec![unhealthy]);
        assert_eq!(boot(&boot_input(), &v).unwrap_err().code, GateFailureCode::BootFailed);
    }

    // ---- repro ----

    fn repro_spec(reps: u8) -> VerifySpec {
        VerifySpec::Repro { script: art(1), signature: sig(), repetitions: reps }
    }
    fn diagnose(verdict: Verdict) -> DiagnoseResult {
        DiagnoseResult {
            verdict,
            script: Some(art(1)),
            signature: Some(sig()),
            summary: "s".into(),
            suggested_fix: None,
            fix_sites: vec![],
        }
    }
    fn repro_input(spec: &VerifySpec) -> ReproGate<'_> {
        ReproGate { gate: gate_id(), diagnose_run: run(2), cause: cause(), spec }
    }

    #[test]
    fn repro_passes_when_every_repetition_matches() {
        let spec = repro_spec(2);
        let v = verify(
            VerifyOutcome::Completed,
            vec![
                attempt(1, AttemptKind::Script, false, vec![obs("200"), obs("500")]),
                attempt(2, AttemptKind::Script, false, vec![obs("500")]),
            ],
        );
        let token = repro(&repro_input(&spec), &diagnose(Verdict::Reproduced), &v).unwrap();
        assert_eq!(token.cause(), cause());
        assert_eq!(token.diagnose_run(), run(2));
        assert_eq!(token.gate(), gate_id());
    }

    #[test]
    fn repro_fails_when_one_repetition_misses() {
        let spec = repro_spec(2);
        let v = verify(
            VerifyOutcome::Completed,
            vec![attempt(1, AttemptKind::Script, false, vec![obs("500")]), attempt(2, AttemptKind::Script, false, vec![obs("200")])],
        );
        let err = repro(&repro_input(&spec), &diagnose(Verdict::Reproduced), &v).unwrap_err();
        assert_eq!(err.code, GateFailureCode::SignatureNotObserved);
        assert!(err.reason.contains("repetition 2"));
    }

    #[test]
    fn repro_rejections() {
        let spec = repro_spec(1);
        let ok = verify(VerifyOutcome::Completed, vec![attempt(1, AttemptKind::Script, false, vec![obs("500")])]);
        let input = repro_input(&spec);
        assert_eq!(repro(&input, &diagnose(Verdict::NotReproduced), &ok).unwrap_err().code, GateFailureCode::NotClaimed);
        let mut no_sig = diagnose(Verdict::Reproduced);
        no_sig.signature = None;
        assert_eq!(repro(&input, &no_sig, &ok).unwrap_err().code, GateFailureCode::NotClaimed);
        let mut other_script = diagnose(Verdict::Reproduced);
        other_script.script = Some(art(9));
        assert_eq!(repro(&input, &other_script, &ok).unwrap_err().code, GateFailureCode::SpecMismatch);
        let boot_failed = verify(VerifyOutcome::BootFailed { reason: "unhealthy".into() }, vec![]);
        assert_eq!(repro(&input, &diagnose(Verdict::Reproduced), &boot_failed).unwrap_err().code, GateFailureCode::BootFailed);
        let two = verify(
            VerifyOutcome::Completed,
            vec![attempt(1, AttemptKind::Script, false, vec![obs("500")]), attempt(2, AttemptKind::Script, false, vec![obs("500")])],
        );
        assert_eq!(repro(&input, &diagnose(Verdict::Reproduced), &two).unwrap_err().code, GateFailureCode::WrongAttemptCount);
        let wrong_spec = VerifySpec::Boot;
        assert_eq!(repro(&repro_input(&wrong_spec), &diagnose(Verdict::Reproduced), &ok).unwrap_err().code, GateFailureCode::SpecMismatch);
    }

    // ---- fix ----

    const PATCH_SHA: [u8; 32] = [7; 32];
    const REBASED_SHA: [u8; 32] = [8; 32];

    fn regression() -> RegressionFile {
        RegressionFile { path: RepoPath::new(".mage/regressions/eng-1-null.spec.ts").unwrap(), script: art(1), script_sha256: [3; 32] }
    }
    fn change_set() -> ChangeSet {
        ChangeSet { base_sha: CommitSha::from_bytes([4; 20]), patch: art(10), patch_sha256: PATCH_SHA, regression: Some(regression()) }
    }
    fn fix_spec(mode: ApplyMode, upstream_check: bool, reps: u8) -> VerifySpec {
        VerifySpec::Fix {
            patch: art(10),
            script: art(1),
            signature: sig(),
            repetitions: reps,
            regression: Some(regression()),
            mode,
            upstream_check,
        }
    }
    fn fix_result() -> FixResult {
        FixResult { patch: art(10), summary: "fix".into(), excluded: vec![], stats: FixStats::default() }
    }
    fn checks() -> FixChecks {
        FixChecks {
            applied_patch_sha256: Some(PATCH_SHA),
            boot: true,
            test_exit: Some(0),
            lint_exit: Some(0),
            source_applies: Some(true),
            upstream_already_fixed: None,
        }
    }
    fn passing_verify() -> VerifyResult {
        VerifyResult {
            outcome: VerifyOutcome::Completed,
            observed: None,
            attempts: vec![attempt(1, AttemptKind::Script, true, vec![obs("200")]), attempt(2, AttemptKind::Script, true, vec![])],
            artifacts: vec![],
            fix_checks: Some(checks()),
            rebased_patch: None,
        }
    }
    fn fix_input<'a>(cs: &'a ChangeSet, spec: &'a VerifySpec, lint_required: bool) -> FixGate<'a> {
        FixGate { gate: gate_id(), fix_run: run(3), cause: cause(), change_set: cs, spec, lint_required }
    }

    #[test]
    fn fix_passes_and_binds_change_set() {
        let cs = change_set();
        let spec = fix_spec(ApplyMode::Exact, false, 2);
        let token = fix(&fix_input(&cs, &spec, true), &fix_result(), &passing_verify()).unwrap();
        assert_eq!(token.base_sha(), cs.base_sha);
        assert_eq!(token.digest(), cs.digest());
        assert_eq!(token.change_set(), &cs);
        assert_eq!(token.fix_run(), run(3));
        assert_eq!(token.cause(), cause());
    }

    /// Each check failing alone (fix.md Testing).
    #[test]
    fn fix_each_check_failing_alone() {
        let cs = change_set();
        let spec = fix_spec(ApplyMode::Exact, false, 2);
        let input = fix_input(&cs, &spec, true);
        type Mutate = fn(&mut VerifyResult);
        let cases: Vec<(&str, Mutate, GateFailureCode)> = vec![
            ("apply", |v| v.outcome = VerifyOutcome::PatchRejected { reason: "hunk 1".into() }, GateFailureCode::PatchDoesNotApply),
            ("digest", |v| v.fix_checks.as_mut().unwrap().applied_patch_sha256 = Some([9; 32]), GateFailureCode::DigestMismatch),
            ("no checks", |v| v.fix_checks = None, GateFailureCode::DigestMismatch),
            ("rebuild", |v| v.fix_checks.as_mut().unwrap().boot = false, GateFailureCode::BootFailed),
            ("boot outcome", |v| v.outcome = VerifyOutcome::BootFailed { reason: "build".into() }, GateFailureCode::BootFailed),
            ("script exit", |v| v.attempts[0].script_passed = false, GateFailureCode::ReproStillFails),
            ("signature on 1 of 2", |v| v.attempts[1].observed = vec![obs("500")], GateFailureCode::ReproStillFails),
            (
                "repetitions",
                |v| {
                    v.attempts.pop();
                },
                GateFailureCode::WrongAttemptCount,
            ),
            ("tests", |v| v.fix_checks.as_mut().unwrap().test_exit = Some(1), GateFailureCode::TestsFailed),
            ("tests not run", |v| v.fix_checks.as_mut().unwrap().test_exit = None, GateFailureCode::TestsFailed),
            ("lint", |v| v.fix_checks.as_mut().unwrap().lint_exit = Some(2), GateFailureCode::LintFailed),
            ("source", |v| v.fix_checks.as_mut().unwrap().source_applies = Some(false), GateFailureCode::NotApplicableToSource),
            ("source unknown", |v| v.fix_checks.as_mut().unwrap().source_applies = None, GateFailureCode::NotApplicableToSource),
            ("errored", |v| v.outcome = VerifyOutcome::Errored { reason: "fork".into() }, GateFailureCode::VerifyErrored),
        ];
        for (name, mutate, code) in cases {
            let mut v = passing_verify();
            mutate(&mut v);
            assert_eq!(fix(&input, &fix_result(), &v).unwrap_err().code, code, "{name}");
        }
    }

    #[test]
    fn fix_advisory_lint_failure_does_not_fail() {
        let cs = change_set();
        let spec = fix_spec(ApplyMode::Exact, false, 2);
        let mut v = passing_verify();
        v.fix_checks.as_mut().unwrap().lint_exit = Some(1);
        assert!(fix(&fix_input(&cs, &spec, false), &fix_result(), &v).is_ok());
        assert_eq!(fix(&fix_input(&cs, &spec, true), &fix_result(), &v).unwrap_err().code, GateFailureCode::LintFailed);
    }

    #[test]
    fn fix_rejects_change_set_or_spec_mismatch() {
        let spec = fix_spec(ApplyMode::Exact, false, 2);
        // Change set names another patch artifact.
        let other = ChangeSet { patch: art(11), ..change_set() };
        assert_eq!(
            fix(&fix_input(&other, &spec, false), &fix_result(), &passing_verify()).unwrap_err().code,
            GateFailureCode::SpecMismatch
        );
        // Change set without the regression file the gate ran with.
        let no_reg = ChangeSet { regression: None, ..change_set() };
        assert_eq!(
            fix(&fix_input(&no_reg, &spec, false), &fix_result(), &passing_verify()).unwrap_err().code,
            GateFailureCode::SpecMismatch
        );
        // Spec for another patch.
        let cs = change_set();
        let other_fix = FixResult { patch: art(12), ..fix_result() };
        assert_eq!(fix(&fix_input(&cs, &spec, false), &other_fix, &passing_verify()).unwrap_err().code, GateFailureCode::SpecMismatch);
        // Not a fix spec.
        let boot_spec = VerifySpec::Boot;
        assert_eq!(
            fix(&fix_input(&cs, &boot_spec, false), &fix_result(), &passing_verify()).unwrap_err().code,
            GateFailureCode::SpecMismatch
        );
    }

    fn rebased_change_set() -> ChangeSet {
        ChangeSet { base_sha: CommitSha::from_bytes([5; 20]), patch: art(20), patch_sha256: REBASED_SHA, regression: Some(regression()) }
    }
    fn rebased_verify() -> VerifyResult {
        let mut v = passing_verify();
        v.rebased_patch = Some(art(20));
        v.fix_checks.as_mut().unwrap().applied_patch_sha256 = Some(REBASED_SHA);
        v.attempts.insert(0, attempt(0, AttemptKind::UpstreamScript, false, vec![obs("500")]));
        v
    }

    #[test]
    fn fix_rebase_binds_new_base_and_rebased_patch() {
        let cs = rebased_change_set();
        let spec = fix_spec(ApplyMode::AllowOffset, true, 2);
        let token = fix(&fix_input(&cs, &spec, false), &fix_result(), &rebased_verify()).unwrap();
        assert_eq!(token.base_sha(), CommitSha::from_bytes([5; 20]));
        assert_eq!(token.change_set().patch, art(20));
        // The original change set does not match the rebased verify result.
        let original = change_set();
        assert_eq!(
            fix(&fix_input(&original, &spec, false), &fix_result(), &rebased_verify()).unwrap_err().code,
            GateFailureCode::SpecMismatch
        );
    }

    #[test]
    fn fix_rebase_outcomes() {
        let cs = rebased_change_set();
        let spec = fix_spec(ApplyMode::AllowOffset, true, 2);
        let input = fix_input(&cs, &spec, false);
        // Unpatched script passes without the signature → already fixed upstream.
        let mut upstream = rebased_verify();
        upstream.attempts[0] = attempt(0, AttemptKind::UpstreamScript, true, vec![obs("200")]);
        assert_eq!(fix(&input, &fix_result(), &upstream).unwrap_err().code, GateFailureCode::AlreadyFixedUpstream);
        // Unpatched script passes but still shows the signature → not fixed upstream.
        let mut still = rebased_verify();
        still.attempts[0] = attempt(0, AttemptKind::UpstreamScript, true, vec![obs("500")]);
        assert!(fix(&input, &fix_result(), &still).is_ok());
        // AllowOffset rejection → rebase conflict.
        let mut conflict = rebased_verify();
        conflict.outcome = VerifyOutcome::PatchRejected { reason: "src/a.ts".into() };
        conflict.fix_checks = None;
        assert_eq!(fix(&input, &fix_result(), &conflict).unwrap_err().code, GateFailureCode::RebaseConflict);
        // Upstream check requested but no upstream attempt.
        let mut missing = rebased_verify();
        missing.attempts.remove(0);
        assert_eq!(fix(&input, &fix_result(), &missing).unwrap_err().code, GateFailureCode::SpecMismatch);
    }
}
