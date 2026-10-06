//! `RunSpec` and `RunRequirements` (runs.md Additions): the inputs a run was created with, frozen
//! at creation for reproducibility and lease matching, and persisted by `RunStore` (`runs.spec`).

use serde::{Deserialize, Serialize};

use mage_core::domain::{CommitSha, HarnessKind, RecipeHash, RepoId, RunKind, TicketId, VerifySpec};
use mage_core::wire::runner::{Arch, Capabilities, CredentialModeKind, ExecutorKind, FixSpec, SlotClass};

use crate::domain::triage::TriageSpec;

/// What a runner must offer to lease the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRequirements {
    /// Pinned to the parent runner's arch for Verify runs.
    pub arch: Option<Arch>,
    pub executor: ExecutorKind,
    /// `None` for Verify (no harness).
    pub harness: Option<(HarnessKind, CredentialModeKind)>,
    /// Playwright and browsers installed.
    pub browser: bool,
}

impl RunRequirements {
    /// Lease filter: executor, pinned arch, harness kind + credential mode, browser.
    pub fn satisfied_by(&self, caps: &Capabilities) -> bool {
        caps.executor == self.executor
            && self.arch.is_none_or(|arch| arch == caps.arch)
            && self.harness.is_none_or(|(kind, mode)| caps.offers_harness(kind, mode))
            && (!self.browser || caps.browser)
    }
}

/// Frozen inputs of one run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunSpec {
    pub repo_id: Option<RepoId>,
    pub sha: Option<CommitSha>,
    pub ticket_id: Option<TicketId>,
    pub recipe_hash: Option<RecipeHash>,
    /// Initial prompt (empty for Verify).
    pub prompt: String,
    /// Forwarded as `HarnessAssignment.system_append` (empty for Verify).
    pub system_append: String,
    /// Model id (empty for Verify).
    pub model: String,
    pub requirements: RunRequirements,
    /// `Some` for Verify only.
    pub verify: Option<VerifySpec>,
    /// `Some` for Triage only.
    pub triage: Option<TriageSpec>,
    /// `Some` for Fix only.
    pub fix: Option<FixSpec>,
    /// W3C `traceparent` of the run.
    pub trace: Option<String>,
    pub slot: SlotClass,
    /// Lower leases first (Verify 0, Triage 1, Diagnose 2, Fix 3, Learn 4).
    pub priority: i16,
}

impl RunSpec {
    /// Scheduling priority per kind: lower values are leased first.
    pub const fn priority_of(kind: RunKind) -> i16 {
        match kind {
            RunKind::Verify => 0,
            RunKind::Triage => 1,
            RunKind::Diagnose => 2,
            RunKind::Fix => 3,
            RunKind::Learn => 4,
        }
    }

    pub fn credential_mode(&self) -> Option<CredentialModeKind> {
        self.requirements.harness.map(|(_, mode)| mode)
    }

    /// The assignment carries a gateway token, so `usage_metered` is authoritative.
    pub fn gateway_metered(&self) -> bool {
        self.credential_mode() == Some(CredentialModeKind::Gateway)
    }
}

#[cfg(test)]
mod tests {
    use mage_core::wire::runner::{HarnessAvailability, Os, SlotCounts};

    use super::*;

    fn caps(browser: bool, modes: Vec<CredentialModeKind>) -> Capabilities {
        Capabilities {
            arch: Arch::Aarch64,
            os: Os::MacOs,
            executor: ExecutorKind::Host,
            harnesses: vec![HarnessAvailability { kind: HarnessKind::Omp, version: "1".into(), credential_modes: modes }],
            browser,
            slots: SlotCounts { light: 1, work: 1, verify: 1 },
        }
    }

    #[test]
    fn requirements_match_each_capability_dimension() {
        let base = RunRequirements {
            arch: None,
            executor: ExecutorKind::Host,
            harness: Some((HarnessKind::Omp, CredentialModeKind::Gateway)),
            browser: false,
        };
        assert!(base.satisfied_by(&caps(false, vec![CredentialModeKind::Gateway])));
        // credential mode not advertised
        assert!(!base.satisfied_by(&caps(false, vec![CredentialModeKind::LocalLogin])));
        // other harness kind
        let claude = RunRequirements { harness: Some((HarnessKind::ClaudeCode, CredentialModeKind::Gateway)), ..base };
        assert!(!claude.satisfied_by(&caps(true, vec![CredentialModeKind::Gateway])));
        // browser required
        let browser = RunRequirements { browser: true, ..base };
        assert!(!browser.satisfied_by(&caps(false, vec![CredentialModeKind::Gateway])));
        assert!(browser.satisfied_by(&caps(true, vec![CredentialModeKind::Gateway])));
        // arch pin
        let pinned = RunRequirements { arch: Some(Arch::X86_64), harness: None, ..base };
        assert!(!pinned.satisfied_by(&caps(true, vec![])));
        // executor
        let aho = RunRequirements { executor: ExecutorKind::Aho, harness: None, ..base };
        assert!(!aho.satisfied_by(&caps(true, vec![])));
    }
}
