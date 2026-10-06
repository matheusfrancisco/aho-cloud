//! `TicketStore` (storage.md "`TicketStore`"; triage.md, fix.md, knowledge.md additions): tickets,
//! thread snapshots, causes and citations, fix briefs, fix pull requests, resolutions and the
//! resolved-ticket memory.
//!
//! Every ticket write bumps `rev`. CAS parameters (`expect`) compare the stored variant only;
//! `expected_rev` compares `rev`. A miss is `Conflict`; a missing row is `NotFound`. Mutating
//! methods enqueue `jobs` in the same transaction and append an org feed entry (`Ticket` for
//! ticket, PR and resolution writes, `Cause` for cause and brief writes).
//!
//! Addition beyond storage.md: `update_pull_request` (fix.md `OpenPullRequest` persists branch,
//! head commit, number and url with the state; optional ticket transition in the same
//! transaction).

use async_trait::async_trait;
use mage_core::domain::fix::{FixPrState, FixPullRequest};
use mage_core::domain::{
    Cause, CauseId, CauseStatus, ContentHash, CustomerId, FixBrief, FixPullRequestId, OrgId, RepoId, Resolution, RunId, Ticket, TicketId,
    TicketStatus, TicketTarget,
};
use mage_core::ports::common::{Page, PageRequest, Versioned};
use mage_core::ports::jobs::NewJob;
use time::OffsetDateTime;

pub use crate::domain::knowledge::{SimilarMatch, SimilarResolved};
use crate::domain::intake::ThreadSnapshot;
use crate::error::AppError;

/// Optional ticket status CAS applied with a cause / PR write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TicketTransition {
    pub expect: TicketStatus,
    pub next: TicketStatus,
}

/// The triage outcome written by `apply_triage`.
#[derive(Debug, Clone, PartialEq)]
pub struct TriageWrite {
    pub ticket: TicketId,
    /// `Triaging`.
    pub expect: TicketStatus,
    /// Must equal the stored content hash.
    pub content_hash: ContentHash,
    /// Must equal `tickets.triage_run` (fencing).
    pub triage_run: RunId,
    /// New causes of this triage run (ids, ranks and citations as given).
    pub causes: Vec<Cause>,
    /// `Triaged`.
    pub next: TicketStatus,
}

/// `list_tickets` filter; every `Some` narrows the result.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TicketFilter {
    pub status: Option<Vec<TicketStatus>>,
    pub customer: Option<CustomerId>,
    /// Target repo.
    pub repo: Option<RepoId>,
    /// Full-text query over title and body (`websearch_to_tsquery('simple', …)`).
    pub text: Option<String>,
}

/// `similar_resolved` query (knowledge.md "`similar_tickets`").
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SimilarQuery {
    /// Resolved tickets whose target repo is one of these.
    pub repos: Vec<RepoId>,
    pub text: String,
    pub error_strings: Vec<String>,
    /// Never returned (memory cutoff: the ticket under triage / eval).
    pub exclude: Vec<TicketId>,
    /// Only resolutions at or before this time (memory cutoff).
    pub as_of: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TicketStoreError {
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
pub trait TicketStore: Send + Sync {
    // tickets (intake.md)
    async fn get_ticket(&self, org: OrgId, id: TicketId) -> Result<Versioned<Ticket>, TicketStoreError>;
    async fn get_by_linear_issue(&self, org: OrgId, issue_id: &str) -> Result<Option<Versioned<Ticket>>, TicketStoreError>;
    /// Newest first (ticket id descending).
    async fn list_tickets(&self, org: OrgId, filter: &TicketFilter, page: PageRequest) -> Result<Page<Ticket>, TicketStoreError>;
    /// Non-terminal tickets of `repo` whose target is the assumed default head.
    async fn list_open_assumed(&self, org: OrgId, repo: RepoId) -> Result<Vec<TicketId>, TicketStoreError>;
    /// `None` = insert (rev 1; duplicate id or Linear issue → `Conflict`), `Some(rev)` = full
    /// update under a rev CAS. Writes the ticket as given (status, triage fields, timestamps).
    /// Returns the new rev.
    async fn save_ingested(&self, org: OrgId, ticket: &Ticket, expected_rev: Option<u64>, jobs: &[NewJob]) -> Result<u64, TicketStoreError>;
    /// Status CAS; no domain transition check (the use case applied the domain event).
    async fn transition(&self, org: OrgId, ticket: TicketId, expect: TicketStatus, next: TicketStatus, jobs: &[NewJob])
    -> Result<Ticket, TicketStoreError>;
    /// Rev CAS; returns the new rev.
    async fn set_target(&self, org: OrgId, ticket: TicketId, target: Option<TicketTarget>, expected_rev: u64, jobs: &[NewJob])
    -> Result<u64, TicketStoreError>;
    async fn get_thread_snapshot(&self, org: OrgId, ticket: TicketId) -> Result<Option<ThreadSnapshot>, TicketStoreError>;
    /// Replaces the ticket's snapshot (does not bump the ticket rev). Unknown ticket → `NotFound`.
    async fn put_thread_snapshot(&self, org: OrgId, ticket: TicketId, s: &ThreadSnapshot) -> Result<(), TicketStoreError>;

    // triage (triage.md)
    /// `status ∈ expect AND content_hash = content_hash` → `Triaging`, recording `triaged_hash`
    /// and `triage_run`.
    async fn begin_triage(
        &self,
        org: OrgId,
        ticket: TicketId,
        expect: &[TicketStatus],
        content_hash: ContentHash,
        triage_run: RunId,
    ) -> Result<Ticket, TicketStoreError>;
    /// Fenced on status, `triage_run` and content hash; supersedes the ticket's current `Untested`
    /// causes, inserts `w.causes` with their citations, moves the ticket to `w.next`.
    async fn apply_triage(&self, org: OrgId, w: TriageWrite, jobs: &[NewJob]) -> Result<Ticket, TicketStoreError>;
    /// `Triaging → TriageFailed` fenced on `triage_run`.
    async fn fail_triage(&self, org: OrgId, ticket: TicketId, triage_run: RunId, jobs: &[NewJob]) -> Result<Ticket, TicketStoreError>;

    // causes
    /// Current causes by rank, then (if asked) superseded ones, most recently superseded first.
    async fn causes(&self, org: OrgId, ticket: TicketId, include_superseded: bool) -> Result<Vec<Cause>, TicketStoreError>;
    async fn get_cause(&self, org: OrgId, id: CauseId) -> Result<Cause, TicketStoreError>;
    /// Cause CAS by status variant, optional ticket CAS by status, one transaction.
    async fn set_cause_status(
        &self,
        org: OrgId,
        id: CauseId,
        expect: &CauseStatus,
        next: CauseStatus,
        ticket: Option<TicketTransition>,
        jobs: &[NewJob],
    ) -> Result<Cause, TicketStoreError>;

    // fix briefs and pull requests (fix.md)
    /// Stores `brief` with `version = max + 1` for its cause (assigned under a row lock on the
    /// cause; the given version is ignored) and returns it.
    async fn put_fix_brief(&self, org: OrgId, brief: FixBrief, jobs: &[NewJob]) -> Result<FixBrief, TicketStoreError>;
    async fn latest_fix_brief(&self, org: OrgId, cause: CauseId) -> Result<Option<FixBrief>, TicketStoreError>;
    /// Oldest version first.
    async fn fix_briefs(&self, org: OrgId, cause: CauseId) -> Result<Vec<FixBrief>, TicketStoreError>;
    /// Insert if absent by `(org, ticket, change_set)`; otherwise returns the stored row unchanged.
    /// `bool` = created; `jobs` are enqueued only when created.
    async fn upsert_pull_request(&self, org: OrgId, pr: FixPullRequest, jobs: &[NewJob])
    -> Result<(FixPullRequest, bool), TicketStoreError>;
    /// State CAS by variant; writes the state and its payload (`CommitCreated` → head commit,
    /// `Opened` → number, `Closed` → merged).
    async fn set_pull_request_state(
        &self,
        org: OrgId,
        id: FixPullRequestId,
        expect: &FixPrState,
        next: FixPrState,
        jobs: &[NewJob],
    ) -> Result<FixPullRequest, TicketStoreError>;
    /// State CAS by variant on `pr.id`; writes branch, head commit, number, url and state from
    /// `pr`, plus the optional ticket CAS, one transaction.
    async fn update_pull_request(
        &self,
        org: OrgId,
        pr: &FixPullRequest,
        expect: &FixPrState,
        ticket: Option<TicketTransition>,
        jobs: &[NewJob],
    ) -> Result<FixPullRequest, TicketStoreError>;
    /// The PR row with this number whose fix run targets `repo`.
    async fn pull_request_by_number(&self, org: OrgId, repo: RepoId, number: u64) -> Result<Option<FixPullRequest>, TicketStoreError>;
    /// The ticket's PR rows, oldest first (`created_at`, id).
    async fn pull_requests(&self, org: OrgId, ticket: TicketId) -> Result<Vec<FixPullRequest>, TicketStoreError>;

    // resolutions and memory
    /// Resolution row + ticket `expect → Resolved`, one transaction. Already resolved → `Conflict`.
    async fn resolve(&self, org: OrgId, resolution: Resolution, expect: TicketStatus, jobs: &[NewJob]) -> Result<Ticket, TicketStoreError>;
    async fn resolution(&self, org: OrgId, ticket: TicketId) -> Result<Option<Resolution>, TicketStoreError>;
    /// Resolved tickets of `q.repos` that match the text and/or share error-string tokens, best
    /// first, at most `limit`.
    async fn similar_resolved(&self, org: OrgId, q: &SimilarQuery, limit: u16) -> Result<Vec<SimilarResolved>, TicketStoreError>;
}

impl From<TicketStoreError> for AppError {
    fn from(err: TicketStoreError) -> AppError {
        match err {
            TicketStoreError::NotFound => AppError::NotFound("ticket data".into()),
            TicketStoreError::Conflict => AppError::Conflict("ticket changed concurrently".into()),
            TicketStoreError::Invalid(msg) => AppError::Invalid(msg),
            TicketStoreError::Unavailable(msg) => AppError::Unavailable(msg),
        }
    }
}

/// Upper bound of `similar_resolved`'s `limit`.
pub const SIMILAR_LIMIT_MAX: u16 = 50;

/// Error-string tokens used by the `similar_resolved` overlap score: lowercase runs of
/// `[a-z0-9_]` of at least 3 characters that are not all digits, deduplicated, sorted.
pub fn error_tokens<'a>(errors: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut out: Vec<String> = errors
        .into_iter()
        .flat_map(|e| e.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')))
        .filter(|t| t.len() >= 3 && !t.bytes().all(|b| b.is_ascii_digit()))
        .map(str::to_ascii_lowercase)
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// Share of the query's error tokens found among a candidate's (0 when the query has none).
pub fn error_overlap(query: &[String], candidate: &[String]) -> f32 {
    if query.is_empty() {
        return 0.0;
    }
    let hits = query.iter().filter(|t| candidate.binary_search(t).is_ok()).count();
    hits as f32 / query.len() as f32
}

/// `similar_resolved` score: half text relevance (`0..=1`), half error-token overlap (`0..=1`).
pub fn similarity_score(text: f32, errors: f32) -> f32 {
    0.5 * text.clamp(0.0, 1.0) + 0.5 * errors.clamp(0.0, 1.0)
}

/// A PR row's state payload must agree with its columns (`CommitCreated { commit }` ⇒
/// `head_commit == Some(commit)`, `Opened { number }` ⇒ `number == Some(number)`), as
/// `FixPullRequest::apply` keeps them; the stored row holds each value once. `Invalid` otherwise
/// (`upsert_pull_request`, `update_pull_request`).
pub fn check_pull_request_payload(pr: &FixPullRequest) -> Result<(), TicketStoreError> {
    let consistent = match &pr.state {
        FixPrState::CommitCreated { commit } => pr.head_commit == Some(*commit),
        FixPrState::Opened { number } => pr.number == Some(*number),
        FixPrState::Planned | FixPrState::BranchCreated | FixPrState::Closed { .. } => true,
    };
    if consistent {
        Ok(())
    } else {
        Err(TicketStoreError::Invalid(format!("pull request {}: {} state disagrees with its head commit / number", pr.id, pr.state.name())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_tokens_drop_short_and_numeric_runs() {
        let tokens = error_tokens(["TypeError: Cannot read properties of undefined (reading 'id') at 42", "ECONNRESET on db_pool"]);
        assert_eq!(tokens, ["cannot", "db_pool", "econnreset", "properties", "read", "reading", "typeerror", "undefined"]);
        assert_eq!(error_overlap(&error_tokens(["ECONNRESET timeout"]), &tokens), 0.5);
        assert_eq!(error_overlap(&[], &tokens), 0.0);
    }
}
