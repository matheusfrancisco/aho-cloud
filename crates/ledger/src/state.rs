//! The folded view of a ledger: every release, what is active where, the policy, the standing
//! grant. Produced only by replaying entries through [`crate::rules::apply`].

use std::collections::BTreeMap;

use cloud_contract::Manifest;
use cloud_contract::diff::{SurfaceDiff, diff};
use cloud_contract::ledger::{ActivationReason, Approver, Proposer, Provenance, Stage, TaintFinding};
use cloud_contract::policy::{ReleasePolicy, Requirement, StandingOrder};
use serde::{Deserialize, Serialize};

/// The project's release history folded to "now". Serializable because it is also the value the
/// `deploy` [`aho_log::CustomFold`] threads between entries.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerState {
    pub releases: BTreeMap<String, ReleaseState>,
    /// Release currently active at each stage. Once a stage has a release it always has one.
    pub active: BTreeMap<Stage, String>,
    pub policy: ReleasePolicy,
    /// The latest grant; a new grant replaces it.
    pub standing: Option<StandingGrant>,
    /// Target of the last executed standing-order rollback not yet activated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) standing_rollback: Option<String>,
    /// The org's member count, as last recorded. Caps every approval quorum; `None` (never
    /// recorded) leaves the policy as written.
    #[serde(default)]
    pub members: Option<u16>,
}

/// One proposed release and everything recorded about it since.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseState {
    pub manifest: Manifest,
    pub manifest_hash: String,
    pub provenance: Provenance,
    pub proposer: Proposer,
    pub image_digest: Option<String>,
    /// Latest observed `(listeners, egress hosts)` from a staging run.
    pub observed: Option<(Vec<u16>, Vec<String>)>,
    /// Unresolved findings: each clears when its secret is rotated after it was found.
    pub taint: Vec<TaintFinding>,
    /// Every approval ever recorded, oldest first. Each activation at a stage spends that stage's
    /// approvals; use [`LedgerState::approval_status`] for the ones that still count.
    pub approvals: Vec<(Stage, Approver)>,
    /// `(stage, user_id, reason)` of the rejection that ended this release.
    pub rejected: Option<(Stage, String, String)>,
    pub activations: Vec<(Stage, ActivationReason)>,
    /// Per stage, how many of this release's approvals for it the last activation there spent.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) spent: BTreeMap<Stage, usize>,
}

/// How far a release is from its requirement at one stage.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalStatus {
    pub requirement: Requirement,
    /// Unspent approvals for the stage.
    pub approvals: u8,
    /// Whether an org owner is among those approvals.
    pub owner_present: bool,
    pub met: bool,
}

/// Bounded verbs on-call may use without asking, until `expires_at` (Unix seconds).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StandingGrant {
    pub orders: Vec<StandingOrder>,
    pub expires_at: u64,
}

impl ReleaseState {
    pub(crate) fn new(manifest: Manifest, manifest_hash: String, provenance: Provenance, proposer: Proposer) -> Self {
        Self {
            manifest,
            manifest_hash,
            provenance,
            proposer,
            image_digest: None,
            observed: None,
            taint: Vec::new(),
            approvals: Vec::new(),
            rejected: None,
            activations: Vec::new(),
            spent: BTreeMap::new(),
        }
    }

    /// Approvals for `stage` recorded since the last activation there.
    pub(crate) fn unspent(&self, stage: Stage) -> impl Iterator<Item = &Approver> {
        let spent = self.spent.get(&stage).copied().unwrap_or(0);
        self.approvals.iter().filter(move |(s, _)| *s == stage).skip(spent).map(|(_, a)| a)
    }

    /// Whether this release has ever been active at `stage`.
    pub(crate) fn was_active(&self, stage: Stage) -> bool {
        self.activations.iter().any(|(s, _)| *s == stage)
    }

    /// Records an activation at `stage`, spending the stage's approvals.
    pub(crate) fn activated(&mut self, stage: Stage, reason: ActivationReason) {
        let count = self.approvals.iter().filter(|(s, _)| *s == stage).count();
        self.spent.insert(stage, count);
        self.activations.push((stage, reason));
    }
}

impl LedgerState {
    /// The release active at Live, if any.
    pub(crate) fn live(&self) -> Option<(&str, &ReleaseState)> {
        let id = self.active.get(&Stage::Live)?;
        self.releases.get(id).map(|r| (id.as_str(), r))
    }

    /// What `release` changes against the release active at Live (everything, if none is).
    pub fn diff_against_live(&self, release: &str) -> Option<SurfaceDiff> {
        let r = self.releases.get(release)?;
        Some(diff(self.live().map(|(_, l)| &l.manifest), &r.manifest))
    }

    /// Approvals `release` needs to be activated at `stage`: none at Preview, otherwise the policy
    /// applied to [`Self::diff_against_live`], capped at the recorded member count.
    pub fn requirement(&self, release: &str, stage: Stage) -> Option<Requirement> {
        let d = self.diff_against_live(release)?;
        Some(match stage {
            Stage::Preview => Requirement { approvals: 0, owner_required: false },
            Stage::Staging | Stage::Live => {
                let mut r = self.policy.requirement(&d);
                // A quorum larger than the org can never be met: cap it at the humans there are.
                if let Some(members) = self.members {
                    r.approvals = r.approvals.min(u8::try_from(members).unwrap_or(u8::MAX)).max(1);
                }
                r
            }
        })
    }

    /// Unspent approvals for `release` at `stage` measured against [`Self::requirement`].
    pub fn approval_status(&self, release: &str, stage: Stage) -> Option<ApprovalStatus> {
        let requirement = self.requirement(release, stage)?;
        let r = self.releases.get(release)?;
        let (mut approvals, mut owner_present) = (0u8, false);
        for a in r.unspent(stage) {
            approvals = approvals.saturating_add(1);
            owner_present |= a.owner;
        }
        let met = approvals >= requirement.approvals && (owner_present || !requirement.owner_required);
        Some(ApprovalStatus { requirement, approvals, owner_present, met })
    }
}
