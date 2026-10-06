//! Verify specs and run results (domain.md "Run" result table; runs.md, executor.md and fix.md
//! Additions).

use serde::{Deserialize, Serialize};

use crate::domain::citation::Citation;
use crate::domain::error::DomainError;
use crate::domain::fix::{ApplyMode, RegressionFile};
use crate::domain::ids::{ArtifactId, hex_array};
use crate::domain::path::RepoPath;
use crate::domain::run::RunKind;
use crate::domain::signature::{FailureSignature, SignatureKind, Verdict};
use crate::domain::triage::TriageResult;

/// How a repro script is run, derived from its file name (executor.md `submit_diagnosis`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
pub enum ScriptKind {
    /// `*.spec.{ts,js,mjs}` / `*.test.{ts,js}`, run by `BrowserRunner`.
    Playwright,
    /// `*.sh`, run with `bash`.
    Shell,
}

impl ScriptKind {
    pub fn from_file_name(name: &str) -> Option<ScriptKind> {
        const PLAYWRIGHT: [&str; 5] = [".spec.ts", ".spec.js", ".spec.mjs", ".test.ts", ".test.js"];
        let base = name.rsplit('/').next().unwrap_or(name);
        if PLAYWRIGHT.iter().any(|ext| base.len() > ext.len() && base.ends_with(ext)) {
            Some(ScriptKind::Playwright)
        } else if base.len() > ".sh".len() && base.ends_with(".sh") {
            Some(ScriptKind::Shell)
        } else {
            None
        }
    }

    /// Signature kinds observable for this script kind (`assertion_text` only with Playwright,
    /// `exit_code` only with Shell).
    pub fn supports(self, kind: SignatureKind) -> bool {
        match kind {
            SignatureKind::AssertionText => self == ScriptKind::Playwright,
            SignatureKind::ExitCode => self == ScriptKind::Shell,
            SignatureKind::HttpStatus | SignatureKind::ErrorString => true,
        }
    }
}

/// What a Verify run checks (runs.md Additions; Fix fields from fix.md).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum VerifySpec {
    Boot,
    Repro {
        script: ArtifactId,
        signature: FailureSignature,
        repetitions: u8,
    },
    Fix {
        patch: ArtifactId,
        script: ArtifactId,
        signature: FailureSignature,
        repetitions: u8,
        regression: Option<RegressionFile>,
        mode: ApplyMode,
        upstream_check: bool,
    },
    TestBaseline,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum VerifyOutcome {
    Completed,
    /// Recipe-caused (build, migrate, seed, health); counts as a gate failure.
    BootFailed {
        reason: String,
    },
    PatchRejected {
        reason: String,
    },
    /// Infrastructure; costs no gate attempt.
    Errored {
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
pub enum AttemptKind {
    Script,
    /// Fix rebase flow: the original repro script run unpatched on the new base.
    UpstreamScript,
    Boot,
}

/// Maximum observations kept per attempt.
pub const MAX_OBSERVATIONS: usize = 16;

/// One repetition of a Verify run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
pub struct VerifyAttempt {
    pub index: u8,
    pub kind: AttemptKind,
    pub script_exit: Option<i32>,
    /// Script exited 0 (Shell) or the Playwright run passed.
    pub script_passed: bool,
    /// Playwright `BrowserStatus`, when the script is a Playwright test.
    pub status: Option<String>,
    /// Observations with `matcher = Exact`, at most 16, matching candidates first.
    pub observed: Vec<FailureSignature>,
    pub health_ok: bool,
    pub duration_ms: u64,
}

/// Facts the runner reports for `VerifySpec::Fix` / `TestBaseline` (fix.md Additions).
/// `gates::fix` is the only judge; these are not a verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
pub struct FixChecks {
    /// SHA-256 of the patch bytes actually applied (`None` for `TestBaseline`).
    #[serde(with = "hex_array::option", default)]
    #[ts(type = "string | null")]
    pub applied_patch_sha256: Option<[u8; 32]>,
    /// Rebuild after apply succeeded and services were healthy.
    pub boot: bool,
    pub test_exit: Option<i32>,
    pub lint_exit: Option<i32>,
    /// `apply_patch(AllowOffset)` on the Source baseline returned `Applied`.
    pub source_applies: Option<bool>,
    /// Runner's view of the upstream attempt; the server recomputes it from the attempts.
    pub upstream_already_fixed: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
pub struct VerifyResult {
    pub outcome: VerifyOutcome,
    /// First candidate of the last attempt (display only).
    pub observed: Option<FailureSignature>,
    pub attempts: Vec<VerifyAttempt>,
    pub artifacts: Vec<ArtifactId>,
    #[serde(default)]
    pub fix_checks: Option<FixChecks>,
    /// Rebase flow: the patch re-diffed against the new base, used for every later check.
    #[serde(default)]
    pub rebased_patch: Option<ArtifactId>,
}

/// Result of a Diagnose run. `script` and `signature` are required unless the verdict is
/// `Inconclusive` (executor.md Additions).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
pub struct DiagnoseResult {
    pub verdict: Verdict,
    pub script: Option<ArtifactId>,
    pub signature: Option<FailureSignature>,
    pub summary: String,
    #[serde(default)]
    pub suggested_fix: Option<String>,
    #[serde(default)]
    pub fix_sites: Vec<Citation>,
}

impl DiagnoseResult {
    /// Shape rules checked on submission: script and signature present unless `Inconclusive`;
    /// the signature itself valid (`FailureSignature::validate`).
    pub fn validate(&self) -> Result<(), DomainError> {
        let inconclusive = matches!(self.verdict, Verdict::Inconclusive { .. });
        if !inconclusive {
            if self.script.is_none() {
                return Err(DomainError::invalid("script is required unless inconclusive"));
            }
            if self.signature.is_none() {
                return Err(DomainError::invalid("signature is required unless inconclusive"));
            }
        }
        if let Some(signature) = &self.signature {
            signature.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ts_rs::TS)]
pub struct FixStats {
    pub files: u32,
    pub added: u32,
    pub removed: u32,
}

/// Result of a Fix run. The patch is whatever differs in the fork, never the agent's claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
pub struct FixResult {
    pub patch: ArtifactId,
    pub summary: String,
    /// Paths dropped by the exclusion policy (shown to the reviewer).
    #[serde(default)]
    pub excluded: Vec<RepoPath>,
    #[serde(default)]
    pub stats: FixStats,
}

/// Result of a Learn run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
pub struct RecipeProposal {
    pub toml: String,
    pub notes: String,
}

/// `Run.result`, one variant per `RunKind`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RunResult {
    Learn(RecipeProposal),
    Triage(TriageResult),
    Diagnose(DiagnoseResult),
    Verify(VerifyResult),
    Fix(FixResult),
}

impl RunResult {
    pub fn kind(&self) -> RunKind {
        match self {
            Self::Learn(_) => RunKind::Learn,
            Self::Triage(_) => RunKind::Triage,
            Self::Diagnose(_) => RunKind::Diagnose,
            Self::Verify(_) => RunKind::Verify,
            Self::Fix(_) => RunKind::Fix,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::signature::Matcher;

    #[test]
    fn script_kind_by_extension() {
        let cases = [
            ("repro.spec.ts", Some(ScriptKind::Playwright)),
            ("repro.spec.js", Some(ScriptKind::Playwright)),
            ("repro.spec.mjs", Some(ScriptKind::Playwright)),
            ("e2e/repro.test.ts", Some(ScriptKind::Playwright)),
            ("repro.test.js", Some(ScriptKind::Playwright)),
            ("repro.test.mjs", None),
            ("repro.sh", Some(ScriptKind::Shell)),
            (".sh", None),
            (".spec.ts", None),
            ("repro.ts", None),
            ("repro.py", None),
        ];
        for (name, want) in cases {
            assert_eq!(ScriptKind::from_file_name(name), want, "{name}");
        }
    }

    #[test]
    fn signature_compatibility() {
        assert!(ScriptKind::Playwright.supports(SignatureKind::AssertionText));
        assert!(!ScriptKind::Shell.supports(SignatureKind::AssertionText));
        assert!(ScriptKind::Shell.supports(SignatureKind::ExitCode));
        assert!(!ScriptKind::Playwright.supports(SignatureKind::ExitCode));
        assert!(ScriptKind::Shell.supports(SignatureKind::HttpStatus));
        assert!(ScriptKind::Playwright.supports(SignatureKind::ErrorString));
    }

    fn diag(verdict: Verdict, script: bool, signature: Option<FailureSignature>) -> DiagnoseResult {
        DiagnoseResult {
            verdict,
            script: script.then(|| ArtifactId::from_uuid(uuid::Uuid::from_u128(3))),
            signature,
            summary: "s".into(),
            suggested_fix: None,
            fix_sites: vec![],
        }
    }

    #[test]
    fn diagnose_result_requires_script_and_signature_unless_inconclusive() {
        let sig = FailureSignature { kind: SignatureKind::HttpStatus, value: "500".into(), matcher: Matcher::Exact };
        assert!(diag(Verdict::Reproduced, true, Some(sig.clone())).validate().is_ok());
        assert!(diag(Verdict::Reproduced, false, Some(sig.clone())).validate().is_err());
        assert!(diag(Verdict::NotReproduced, true, None).validate().is_err());
        assert!(diag(Verdict::Inconclusive { reason: "no boot".into() }, false, None).validate().is_ok());
        let bad = FailureSignature { matcher: Matcher::Regex, value: "(".into(), ..sig };
        assert!(diag(Verdict::Inconclusive { reason: "x".into() }, false, Some(bad)).validate().is_err());
    }

    #[test]
    fn run_result_is_tagged_by_kind() {
        let r = RunResult::Learn(RecipeProposal { toml: "version = 1".into(), notes: String::new() });
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["kind"], "learn");
        assert_eq!(serde_json::from_value::<RunResult>(json).unwrap().kind(), RunKind::Learn);
    }
}
