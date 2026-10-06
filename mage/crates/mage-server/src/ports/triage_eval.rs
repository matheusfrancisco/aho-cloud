//! `TriageEvalStore` (triage.md "Evaluation harness"): eval definitions, per-(ticket, repeat)
//! cases and their results, and frozen ground-truth labels.
//!
//! Additions beyond triage.md: `cases` and `case_run` (`StartEvalCase` reads the case's input run
//! and the run already recorded for a repeat).

use std::collections::BTreeSet;
use std::time::Duration;

use async_trait::async_trait;
use mage_core::domain::{OrgId, RepoPath, RunId, RunStatus, TicketId, TriageEvalId, Usage, UserId};
use time::OffsetDateTime;

use crate::error::AppError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTriageEval {
    pub name: String,
    pub corpus: String,
    pub variant_toml: String,
    pub repeat: u8,
    pub created_by: UserId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalCase {
    pub ticket: TicketId,
    /// The ticket's first triage run (its stored input is replayed).
    pub input_run: Option<RunId>,
    pub repeats: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LabelStrength {
    Strong,
    Medium,
}

impl LabelStrength {
    pub const fn as_str(self) -> &'static str {
        match self {
            LabelStrength::Strong => "strong",
            LabelStrength::Medium => "medium",
        }
    }

    pub fn parse(s: &str) -> Option<LabelStrength> {
        [LabelStrength::Strong, LabelStrength::Medium].into_iter().find(|v| v.as_str() == s)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LabelSource {
    FixPr,
    ConfirmedCause,
    Manual,
}

impl LabelSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            LabelSource::FixPr => "fix_pr",
            LabelSource::ConfirmedCause => "confirmed_cause",
            LabelSource::Manual => "manual",
        }
    }

    pub fn parse(s: &str) -> Option<LabelSource> {
        [LabelSource::FixPr, LabelSource::ConfirmedCause, LabelSource::Manual].into_iter().find(|v| v.as_str() == s)
    }
}

/// Ground truth `G(ticket)`: repo paths the cause lives in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroundTruth {
    pub ticket: TicketId,
    pub paths: BTreeSet<RepoPath>,
    pub strength: LabelStrength,
    pub source: LabelSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalCaseResult {
    pub eval: TriageEvalId,
    pub ticket: TicketId,
    pub repeat: u8,
    pub run: RunId,
    pub status: RunStatus,
    pub hit_rank: Option<u16>,
    pub primary_hit_rank: Option<u16>,
    pub causes: u8,
    pub usage: Usage,
    pub queue_wait: Duration,
    pub agent_time: Duration,
    pub gate_attempts: u8,
    pub dropped_causes: u8,
    pub adjusted_citations: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriageEval {
    pub id: TriageEvalId,
    pub name: String,
    pub variant_toml: String,
    pub created_at: OffsetDateTime,
    /// (ticket, repeat) rows.
    pub cases: u32,
    /// Rows with a recorded result.
    pub done: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EvalStoreError {
    #[error("not found")]
    NotFound,
    #[error("conflict")]
    Conflict,
    #[error("invalid: {0}")]
    Invalid(String),
    #[error("unavailable: {0}")]
    Unavailable(String),
}

#[async_trait]
pub trait TriageEvalStore: Send + Sync {
    async fn create_eval(&self, org: OrgId, eval: NewTriageEval) -> Result<TriageEvalId, EvalStoreError>;
    /// One case row per (ticket, repeat in `0..repeats`); existing rows are kept. Unknown eval →
    /// `NotFound`.
    async fn add_cases(&self, org: OrgId, eval: TriageEvalId, cases: &[EvalCase]) -> Result<(), EvalStoreError>;
    /// Records the run of a case; idempotent for the same run, another run → `Conflict`.
    async fn record_case_run(&self, org: OrgId, eval: TriageEvalId, ticket: TicketId, repeat: u8, run: RunId) -> Result<(), EvalStoreError>;
    /// Writes the result (and its run) on the case; the case counts as done.
    async fn record_case_result(&self, org: OrgId, result: &EvalCaseResult) -> Result<(), EvalStoreError>;
    async fn eval(&self, org: OrgId, eval: TriageEvalId) -> Result<TriageEval, EvalStoreError>;
    /// Done cases, ordered by (ticket, repeat).
    async fn case_results(&self, org: OrgId, eval: TriageEvalId) -> Result<Vec<EvalCaseResult>, EvalStoreError>;
    /// Insert or replace the ticket's label.
    async fn put_label(&self, org: OrgId, label: &GroundTruth) -> Result<(), EvalStoreError>;
    /// Labels of those tickets that have one, ordered by ticket.
    async fn labels(&self, org: OrgId, tickets: &[TicketId]) -> Result<Vec<GroundTruth>, EvalStoreError>;
    /// One entry per ticket (repeats = its case rows), ordered by ticket.
    async fn cases(&self, org: OrgId, eval: TriageEvalId) -> Result<Vec<EvalCase>, EvalStoreError>;
    /// The run recorded for the case; unknown case → `NotFound`.
    async fn case_run(&self, org: OrgId, eval: TriageEvalId, ticket: TicketId, repeat: u8) -> Result<Option<RunId>, EvalStoreError>;
}

impl From<EvalStoreError> for AppError {
    fn from(err: EvalStoreError) -> AppError {
        match err {
            EvalStoreError::NotFound => AppError::NotFound("triage eval".into()),
            EvalStoreError::Conflict => AppError::Conflict("triage eval case".into()),
            EvalStoreError::Invalid(msg) => AppError::Invalid(msg),
            EvalStoreError::Unavailable(msg) => AppError::Unavailable(msg),
        }
    }
}
