//! Repo and RepoVersion (domain.md; knowledge.md and intake.md Additions).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::domain::error::DomainError;
use crate::domain::ids::{CommitSha, InstallationId, OrgId, RepoId, RepoVersionKey, TicketId, TreeOid, hex_array};

/// Whether the GitHub App can still read the repo. `Lost` stops triage without deleting rows that
/// citations reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
pub enum RepoAccess {
    Granted,
    Lost,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
pub struct Repo {
    pub id: RepoId,
    pub org_id: OrgId,
    /// GitHub repository id, stable across renames.
    pub external_id: u64,
    pub owner: String,
    pub name: String,
    pub default_branch: String,
    pub installation_id: InstallationId,
    pub access: RepoAccess,
    #[serde(with = "time::serde::rfc3339")]
    #[ts(type = "string")]
    pub created_at: OffsetDateTime,
}

/// Why a commit is indexed. Drives priority and retention (knowledge.md).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, ts_rs::TS)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum VersionReason {
    DefaultHead,
    /// Tag name.
    Release(String),
    /// Environment name.
    Deployed(String),
    OnDemand(TicketId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
pub enum Layer {
    Files,
    Symbols,
    Refs,
    ErrorSites,
    AppMap,
    Semantic,
    History,
}

impl Layer {
    pub const ALL: [Layer; 7] =
        [Layer::Files, Layer::Symbols, Layer::Refs, Layer::ErrorSites, Layer::AppMap, Layer::Semantic, Layer::History];
}

/// State of one layer for one version. A query may use a layer only when it is `Ready`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LayerState {
    Pending,
    Running,
    Ready {
        #[serde(with = "time::serde::rfc3339")]
        #[ts(type = "string")]
        at: OffsetDateTime,
    },
    Failed {
        reason: String,
    },
    Skipped {
        reason: String,
    },
}

/// Events of the `LayerState` table (knowledge.md "Layer states").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LayerEvent {
    /// First job for the layer runs, or a job retry.
    Start,
    Complete {
        #[serde(with = "time::serde::rfc3339")]
        at: OffsetDateTime,
    },
    /// Job dead-lettered.
    Fail { reason: String },
    /// Not applicable, superseded, or prerequisite failed.
    Skip { reason: String },
    /// Manual reindex, rev bump, or recipe validated (for `Refs`).
    Reset,
}

impl LayerEvent {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Complete { .. } => "complete",
            Self::Fail { .. } => "fail",
            Self::Skip { .. } => "skip",
            Self::Reset => "reset",
        }
    }
}

impl LayerState {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Ready { .. } => "ready",
            Self::Failed { .. } => "failed",
            Self::Skipped { .. } => "skipped",
        }
    }

    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready { .. })
    }

    pub fn apply(self, event: LayerEvent) -> Result<LayerState, DomainError> {
        use LayerEvent as E;
        use LayerState as S;
        match (self, event) {
            (S::Pending, E::Start) | (S::Running, E::Start) => Ok(S::Running),
            (S::Pending, E::Skip { reason }) => Ok(S::Skipped { reason }),
            (S::Running, E::Complete { at }) | (S::Ready { .. }, E::Complete { at }) => Ok(S::Ready { at }),
            (S::Running, E::Fail { reason }) => Ok(S::Failed { reason }),
            (S::Failed { .. } | S::Skipped { .. } | S::Ready { .. }, E::Reset) => Ok(S::Pending),
            (from, event) => Err(DomainError::InvalidTransition { from: from.name(), event: event.name() }),
        }
    }
}

pub type LayerMap = BTreeMap<Layer, LayerState>;

/// Indexer extraction revisions (knowledge.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ParserRev(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ErrorsRev(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AppMapRev(pub u32);

/// Revisions of the Ready layers' facts; readers pick a consistent rev.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct IndexRevs {
    pub parser: ParserRev,
    pub errors: ErrorsRev,
    pub appmap: AppMapRev,
}

/// Source tarball of a version's tree (`tar.zst`), shared by identical trees.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceBundleRef {
    pub key: String,
    pub tree: TreeOid,
    pub bytes: u64,
    #[serde(with = "hex_array")]
    pub sha256: [u8; 32],
}

/// An indexed commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoVersion {
    pub org_id: OrgId,
    pub repo_id: RepoId,
    pub sha: CommitSha,
    pub reasons: BTreeSet<VersionReason>,
    pub layers: LayerMap,
    pub bundle: Option<SourceBundleRef>,
    pub revs: Option<IndexRevs>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

impl RepoVersion {
    /// New version with every layer `Pending`.
    pub fn new(org_id: OrgId, key: RepoVersionKey, reason: VersionReason, created_at: OffsetDateTime) -> Self {
        Self {
            org_id,
            repo_id: key.repo_id,
            sha: key.sha,
            reasons: BTreeSet::from([reason]),
            layers: Layer::ALL.iter().map(|l| (*l, LayerState::Pending)).collect(),
            bundle: None,
            revs: None,
            created_at,
        }
    }

    pub fn key(&self) -> RepoVersionKey {
        RepoVersionKey::new(self.repo_id, self.sha)
    }

    pub fn layer(&self, layer: Layer) -> Option<&LayerState> {
        self.layers.get(&layer)
    }

    /// The domain rule: a query may read `layer` only when it is `Ready`.
    pub fn require_ready(&self, layer: Layer) -> Result<(), DomainError> {
        match self.layers.get(&layer) {
            Some(state) if state.is_ready() => Ok(()),
            Some(state) => Err(DomainError::Invalid(format!("layer {layer:?} is {} at {}", state.name(), self.key()))),
            None => Err(DomainError::Invalid(format!("layer {layer:?} unknown at {}", self.key()))),
        }
    }

    /// Applies a layer event (absent layers start as `Pending`); returns the new state.
    pub fn apply_layer_event(&mut self, layer: Layer, event: LayerEvent) -> Result<&LayerState, DomainError> {
        let current = self.layers.get(&layer).cloned().unwrap_or(LayerState::Pending);
        let next = current.apply(event)?;
        let slot = self.layers.entry(layer).or_insert(LayerState::Pending);
        *slot = next;
        Ok(slot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn t1() -> OffsetDateTime {
        datetime!(2026-01-01 00:00 UTC)
    }

    fn t2() -> OffsetDateTime {
        datetime!(2026-01-02 00:00 UTC)
    }

    fn states() -> Vec<LayerState> {
        vec![
            LayerState::Pending,
            LayerState::Running,
            LayerState::Ready { at: t1() },
            LayerState::Failed { reason: "f".into() },
            LayerState::Skipped { reason: "s".into() },
        ]
    }

    fn events() -> Vec<LayerEvent> {
        vec![
            LayerEvent::Start,
            LayerEvent::Complete { at: t2() },
            LayerEvent::Fail { reason: "dead".into() },
            LayerEvent::Skip { reason: "symbols failed".into() },
            LayerEvent::Reset,
        ]
    }

    /// Expected result of every (state, event) pair; `None` = InvalidTransition.
    fn expected(state: &LayerState, event: &LayerEvent) -> Option<LayerState> {
        use LayerEvent as E;
        use LayerState as S;
        match (state, event) {
            (S::Pending, E::Start) => Some(S::Running),
            (S::Pending, E::Skip { .. }) => Some(S::Skipped { reason: "symbols failed".into() }),
            (S::Running, E::Start) => Some(S::Running),
            (S::Running, E::Complete { .. }) => Some(S::Ready { at: t2() }),
            (S::Running, E::Fail { .. }) => Some(S::Failed { reason: "dead".into() }),
            (S::Ready { .. }, E::Complete { .. }) => Some(S::Ready { at: t2() }),
            (S::Ready { .. }, E::Reset) => Some(S::Pending),
            (S::Failed { .. }, E::Reset) => Some(S::Pending),
            (S::Skipped { .. }, E::Reset) => Some(S::Pending),
            _ => None,
        }
    }

    #[test]
    fn layer_state_full_transition_table() {
        let mut legal = 0;
        for s in states() {
            for e in events() {
                let got = s.clone().apply(e.clone());
                match expected(&s, &e) {
                    Some(to) => {
                        legal += 1;
                        assert_eq!(got, Ok(to), "{} + {}", s.name(), e.name());
                    }
                    None => assert_eq!(
                        got,
                        Err(DomainError::InvalidTransition { from: s.name(), event: e.name() }),
                        "{} + {} must be invalid",
                        s.name(),
                        e.name()
                    ),
                }
            }
        }
        // Rows of knowledge.md's table (Failed/Skipped Reset counted per state).
        assert_eq!(legal, 9);
    }

    #[test]
    fn new_version_has_all_layers_pending_and_requires_ready() {
        let key = RepoVersionKey::new(RepoId::from_uuid(uuid::Uuid::from_u128(1)), CommitSha::from_bytes([7; 20]));
        let org = OrgId::from_uuid(uuid::Uuid::from_u128(9));
        let mut v = RepoVersion::new(org, key, VersionReason::DefaultHead, t1());
        assert_eq!(v.layers.len(), Layer::ALL.len());
        assert!(v.require_ready(Layer::Files).is_err());
        v.apply_layer_event(Layer::Files, LayerEvent::Start).unwrap();
        assert!(v.require_ready(Layer::Files).is_err());
        let state = v.apply_layer_event(Layer::Files, LayerEvent::Complete { at: t2() }).unwrap();
        assert_eq!(state, &LayerState::Ready { at: t2() });
        assert!(v.require_ready(Layer::Files).is_ok());
        // Illegal event leaves the state untouched.
        assert!(v.apply_layer_event(Layer::Files, LayerEvent::Fail { reason: "x".into() }).is_err());
        assert!(v.require_ready(Layer::Files).is_ok());
    }

    #[test]
    fn version_reason_order_and_serde() {
        let set: BTreeSet<VersionReason> =
            [VersionReason::Release("v1".into()), VersionReason::DefaultHead, VersionReason::DefaultHead].into_iter().collect();
        assert_eq!(set.len(), 2);
        assert_eq!(
            serde_json::to_value(VersionReason::Deployed("prod".into())).unwrap(),
            serde_json::json!({"type": "deployed", "data": "prod"})
        );
        assert_eq!(serde_json::to_value(Layer::ErrorSites).unwrap(), serde_json::json!("error_sites"));
    }
}
