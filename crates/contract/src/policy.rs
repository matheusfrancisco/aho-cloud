//! Who must approve a release, and what on-call may do without asking.

use std::collections::BTreeSet;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::diff::{Category, SurfaceDiff};

/// An org's approval rule. Changing it is itself an owner-approved action.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
pub struct ReleasePolicy {
    /// Approvals for a release that widens nothing (code, narrowing, neutral changes).
    pub non_widening: u8,
    /// Approvals for a release that widens anything.
    pub widening: u8,
    /// Widening in these categories additionally needs an org owner among the approvers.
    pub owner_only: BTreeSet<Category>,
}

impl Default for ReleasePolicy {
    fn default() -> Self {
        Self { non_widening: 1, widening: 2, owner_only: [Category::Domain, Category::Auth].into() }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
pub struct Requirement {
    pub approvals: u8,
    pub owner_required: bool,
}

impl ReleasePolicy {
    pub fn requirement(&self, diff: &SurfaceDiff) -> Requirement {
        let widened = diff.widened();
        Requirement {
            approvals: if widened.is_empty() { self.non_widening } else { self.widening }.max(1),
            owner_required: widened.iter().any(|c| self.owner_only.contains(c)),
        }
    }
}

/// Bounded verbs the customer pre-approves at handoff for on-call, human or agent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum StandingOrder {
    Restart,
    /// Replica count per process may move within `[min, max]`.
    Scale {
        min: u16,
        max: u16,
    },
    MaintenancePage,
    /// Activate any earlier release whose surface fits within the live one
    /// ([`crate::diff::fits_within`]).
    RollbackWithinCeiling,
}

impl StandingOrder {
    pub fn permits(&self, action: &OnCallAction) -> bool {
        match (self, action) {
            (StandingOrder::Restart, OnCallAction::Restart) => true,
            (StandingOrder::Scale { min, max }, OnCallAction::Scale { replicas }) => (min..=max).contains(&replicas),
            (StandingOrder::MaintenancePage, OnCallAction::MaintenancePage { .. }) => true,
            (StandingOrder::RollbackWithinCeiling, OnCallAction::Rollback { fits_within_ceiling }) => *fits_within_ceiling,
            _ => false,
        }
    }
}

/// What on-call wants to do right now.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum OnCallAction {
    Restart,
    Scale { replicas: u16 },
    MaintenancePage { on: bool },
    Rollback { fits_within_ceiling: bool },
}
