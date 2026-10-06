//! `Gate`, `GateKind`, `GateStatus` (domain.md "Gate"). Proof tokens and gate functions live in
//! [`crate::domain::gates`].

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::domain::error::DomainError;
use crate::domain::ids::{GateId, OrgId, RunId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
pub enum GateKind {
    Evidence,
    Boot,
    Repro,
    Fix,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GateStatus {
    Pending,
    Passed {
        #[serde(with = "time::serde::rfc3339")]
        #[ts(type = "string")]
        at: OffsetDateTime,
    },
    Failed {
        reason: String,
        #[serde(with = "time::serde::rfc3339")]
        #[ts(type = "string")]
        at: OffsetDateTime,
    },
}

/// Decision on a pending gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GateEvent {
    Pass {
        #[serde(with = "time::serde::rfc3339")]
        at: OffsetDateTime,
    },
    Fail {
        reason: String,
        #[serde(with = "time::serde::rfc3339")]
        at: OffsetDateTime,
    },
}

impl GateEvent {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Pass { .. } => "pass",
            Self::Fail { .. } => "fail",
        }
    }
}

impl GateStatus {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Passed { .. } => "passed",
            Self::Failed { .. } => "failed",
        }
    }

    /// A gate is decided once: `Pending` → `Passed` / `Failed`.
    pub fn apply(self, event: GateEvent) -> Result<GateStatus, DomainError> {
        match (self, event) {
            (GateStatus::Pending, GateEvent::Pass { at }) => Ok(GateStatus::Passed { at }),
            (GateStatus::Pending, GateEvent::Fail { reason, at }) => Ok(GateStatus::Failed { reason, at }),
            (from, event) => Err(DomainError::InvalidTransition { from: from.name(), event: event.name() }),
        }
    }
}

/// One gate attempt on a run. `verify_run` is `None` for the inline Evidence gate and for Fix
/// policy rejections recorded before any Verify run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
pub struct Gate {
    pub id: GateId,
    pub org_id: OrgId,
    pub run_id: RunId,
    pub kind: GateKind,
    /// 1-based attempt number of this kind on the run.
    pub attempt: u8,
    pub status: GateStatus,
    pub verify_run: Option<RunId>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn gate_status_full_transition_table() {
        let at = datetime!(2026-03-01 12:00 UTC);
        let states = [GateStatus::Pending, GateStatus::Passed { at }, GateStatus::Failed { reason: "r".into(), at }];
        let events = [GateEvent::Pass { at }, GateEvent::Fail { reason: "boom".into(), at }];
        for s in &states {
            for e in &events {
                let got = s.clone().apply(e.clone());
                match (s, e) {
                    (GateStatus::Pending, GateEvent::Pass { .. }) => assert_eq!(got, Ok(GateStatus::Passed { at })),
                    (GateStatus::Pending, GateEvent::Fail { .. }) => {
                        assert_eq!(got, Ok(GateStatus::Failed { reason: "boom".into(), at }))
                    }
                    _ => assert_eq!(got, Err(DomainError::InvalidTransition { from: s.name(), event: e.name() })),
                }
            }
        }
    }
}
