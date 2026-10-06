//! Triage run inputs, frozen into `RunSpec.triage` (triage.md "TriageInput snapshot" and
//! Additions: `TriageSpec`, `TriageInput`, `TriageMode`, `PriorHypothesis`). Stored with the run so
//! it is replayable and eval cases can reuse the exact input.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use mage_core::domain::{CommitSha, ContentHash, Layer, TicketId, TicketTarget, TriageEvalId, TriageTrigger};
use mage_core::wire::runner::RepoRef;
use mage_core::wire::token::MemoryCutoff;

use crate::domain::knowledge::CandidateSet;

/// Live triage of a ticket, or one replay of an eval case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TriageMode {
    Live,
    Eval {
        eval: TriageEvalId,
        repeat: u8,
        /// Variant ablation: layers every Knowledge MCP tool treats as not Ready.
        #[serde(default)]
        disabled_layers: Vec<Layer>,
    },
}

/// The triage part of a Triage run's `RunSpec`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TriageSpec {
    pub mode: TriageMode,
    pub trigger: TriageTrigger,
    pub input: TriageInput,
    /// `TRIAGE_PROMPT_VERSION` at render time.
    pub prompt_version: u16,
    /// 1..=5.
    pub max_causes: u8,
    /// Preferred or optional layers that were not `Ready` (plus eval-disabled layers).
    pub degraded_layers: Vec<Layer>,
}

impl TriageSpec {
    pub fn is_eval(&self) -> bool {
        matches!(self.mode, TriageMode::Eval { .. })
    }

    /// `KnowledgeScope.memory` of the run's knowledge token: the ticket itself is never a similar
    /// ticket; eval replays also cut memory at the input snapshot (no answer leakage).
    pub fn memory_cutoff(&self, ticket: TicketId) -> MemoryCutoff {
        let as_of = match self.mode {
            TriageMode::Live => None,
            TriageMode::Eval { .. } => Some(self.input.snapshot_at),
        };
        MemoryCutoff { exclude_tickets: vec![ticket], as_of }
    }

    /// `KnowledgeScope.disabled_layers` of the run's knowledge token (empty outside eval).
    pub fn disabled_layers(&self) -> &[Layer] {
        match &self.mode {
            TriageMode::Live => &[],
            TriageMode::Eval { disabled_layers, .. } => disabled_layers,
        }
    }
}

/// What the agent is told about the ticket, snapshotted when the run is created.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TriageInput {
    pub content_hash: ContentHash,
    /// Linear identifier, e.g. `SUP-123`.
    pub identifier: String,
    pub title: String,
    /// Intake-normalized markdown (description, comments, Slack thread).
    pub body: String,
    pub error_strings: Vec<String>,
    pub target: TicketTarget,
    pub repo: RepoRef,
    pub customer: Option<CustomerRef>,
    /// Previous deployment to the same environment, if known.
    pub prior_sha: Option<CommitSha>,
    /// Retriage only: earlier causes that were tested, with their verdicts.
    pub prior_hypotheses: Vec<PriorHypothesis>,
    /// `Retriever::triage_candidates` output; `None` when retrieval was unavailable or disabled.
    pub seeds: Option<CandidateSet>,
    #[serde(with = "time::serde::rfc3339")]
    pub snapshot_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomerRef {
    pub name: String,
    pub environment: Option<String>,
}

/// A cause of an earlier triage that a diagnostic run tested.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriorHypothesis {
    pub title: String,
    pub verdict: String,
    pub summary: String,
}
