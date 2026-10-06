//! Per-org fix policy (fix.md Additions: `FixSettings` on `OrgStore`, area `fix` of
//! `org_settings`). Every field has a serde default, so stored JSON may omit any of them.

use serde::{Deserialize, Serialize};

use crate::domain::error::DomainError;
use crate::domain::fix::policy::{LockfilePolicy, PatchLimits};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(default)]
pub struct FixSettings {
    /// F1 fix briefs.
    pub brief: bool,
    pub brief_in_linear: BriefInLinear,
    /// F2 fix runs.
    pub run: bool,
    /// F3 pull requests.
    pub pull_requests: bool,
    pub open_pr: OpenPrMode,
    pub pr: PrSettings,
    pub lockfiles: LockfilePolicy,
    /// Allow `.github/workflows/**` and `.github/actions/**` edits (still needs the fix app's
    /// `workflows` permission).
    pub allow_workflow_edits: bool,
    pub limits: PatchLimits,
    /// Fresh-fork repro runs per fix gate.
    pub repetitions: u8,
    pub regression: RegressionMode,
}

impl Default for FixSettings {
    fn default() -> Self {
        Self {
            brief: true,
            brief_in_linear: BriefInLinear::default(),
            run: false,
            pull_requests: false,
            open_pr: OpenPrMode::default(),
            pr: PrSettings::default(),
            lockfiles: LockfilePolicy::default(),
            allow_workflow_edits: false,
            limits: PatchLimits::default(),
            repetitions: 2,
            regression: RegressionMode::default(),
        }
    }
}

impl FixSettings {
    /// Rejects settings that would disable a gate check: zero repetitions or a zero limit.
    pub fn validate(&self) -> Result<(), DomainError> {
        if self.repetitions == 0 {
            return Err(DomainError::invalid("fix.repetitions must be at least 1"));
        }
        let l = &self.limits;
        if l.max_patch_bytes == 0 || l.max_files == 0 || l.max_changed_lines == 0 || l.max_file_bytes == 0 {
            return Err(DomainError::invalid("fix.limits must all be at least 1"));
        }
        Ok(())
    }
}

/// How much of a fix brief goes into the Linear comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
pub enum BriefInLinear {
    Off,
    #[default]
    Summary,
    Full,
}

/// Whether `fix.open_pr` runs on user request or automatically on `FixReady`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
pub enum OpenPrMode {
    #[default]
    Manual,
    Auto,
}

/// Parent commit policy for the PR commit (fix.md "Branch, base and rebase policy").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
pub enum BranchFrom {
    /// Parent = the gated `base_sha`; no re-gate.
    GatedSha,
    /// Parent = current default-branch head; re-gate (rebase flow) when it moved.
    #[default]
    DefaultHead,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(default)]
pub struct PrSettings {
    pub branch_from: BranchFrom,
    pub draft: bool,
    /// Added to `mage`, `mage:fix`.
    pub labels: Vec<String>,
    pub reviewers: PrReviewers,
}

impl Default for PrSettings {
    fn default() -> Self {
        Self { branch_from: BranchFrom::default(), draft: true, labels: Vec::new(), reviewers: PrReviewers::default() }
    }
}

/// GitHub logins and team slugs to request reviews from.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ts_rs::TS)]
#[serde(default)]
pub struct PrReviewers {
    pub users: Vec<String>,
    pub teams: Vec<String>,
}

/// Org choice for regression test placement; resolved with the recipe into
/// `RegressionPlacement`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
pub enum RegressionMode {
    Off,
    #[default]
    MageDir,
    RepoTests,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_json_yields_documented_defaults() {
        let s: FixSettings = serde_json::from_str("{}").unwrap();
        assert!(s.brief && !s.run && !s.pull_requests && !s.allow_workflow_edits);
        assert_eq!(s.brief_in_linear, BriefInLinear::Summary);
        assert_eq!(s.open_pr, OpenPrMode::Manual);
        assert_eq!(s.pr.branch_from, BranchFrom::DefaultHead);
        assert!(s.pr.draft);
        assert_eq!(s.lockfiles, LockfilePolicy::WithManifest);
        assert_eq!(s.repetitions, 2);
        assert_eq!(s.regression, RegressionMode::MageDir);
        assert_eq!(s.limits, PatchLimits { max_patch_bytes: 1 << 20, max_files: 50, max_changed_lines: 5000, max_file_bytes: 256 << 10 });
        assert_eq!(s, FixSettings::default());
    }

    #[test]
    fn partial_json_keeps_other_defaults() {
        let s: FixSettings = serde_json::from_str(
            r#"{"run": true, "pr": {"branch_from": "gated_sha"}, "lockfiles": "include", "limits": {"max_files": 10}}"#,
        )
        .unwrap();
        assert!(s.run);
        assert_eq!(s.pr.branch_from, BranchFrom::GatedSha);
        assert!(s.pr.draft);
        assert_eq!(s.lockfiles, LockfilePolicy::Include);
        assert_eq!(s.limits.max_files, 10);
        assert_eq!(s.limits.max_changed_lines, 5000);
    }

    #[test]
    fn validate_rejects_disabled_checks() {
        assert_eq!(FixSettings::default().validate(), Ok(()));
        let zero_reps = FixSettings { repetitions: 0, ..FixSettings::default() };
        assert!(zero_reps.validate().is_err());
        let mut zero_limit = FixSettings::default();
        zero_limit.limits.max_files = 0;
        assert!(zero_limit.validate().is_err());
    }
}
