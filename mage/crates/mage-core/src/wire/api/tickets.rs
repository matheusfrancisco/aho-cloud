//! Tickets, causes and the fix routes (server.md catalog; triage.md `RequestRetriage`; fix.md F1–F3).

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use ts_rs::TS;

use crate::domain::fix::FixPullRequest;
use crate::domain::{
    ArtifactId, Cause, CauseId, CommitSha, Confidence, Customer, CustomerId, HarnessKind, Repo, RepoId, Resolution, RunId, Ticket,
    TicketId, TicketStatus, TicketTarget,
};
use crate::wire::api::RunSummary;

/// `GET …/tickets?status=&repo=&customer=&q=&cursor=&limit=`. Response: `Paged<TicketSummary>`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
pub struct TicketListQuery {
    #[ts(optional)]
    pub status: Option<TicketStatus>,
    #[ts(optional)]
    pub repo: Option<RepoId>,
    #[ts(optional)]
    pub customer: Option<CustomerId>,
    /// Text filter over identifier, title and body.
    #[ts(optional)]
    pub q: Option<String>,
    #[ts(optional)]
    pub cursor: Option<String>,
    #[ts(optional)]
    pub limit: Option<u32>,
}

/// The best-ranked live cause of a ticket.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct TopCause {
    pub cause_id: CauseId,
    pub title: String,
    pub confidence: Confidence,
}

/// A row of the tickets page.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct TicketSummary {
    pub id: TicketId,
    /// Linear identifier, e.g. `ENG-412`.
    pub identifier: String,
    pub title: String,
    pub status: TicketStatus,
    pub target: Option<TicketTarget>,
    /// The target sha is the default-branch head, not a known deployment.
    pub assumed: bool,
    pub top_cause: Option<TopCause>,
    pub customer: Option<CustomerId>,
    #[serde(with = "time::serde::rfc3339")]
    #[ts(type = "string")]
    pub updated_at: OffsetDateTime,
}

/// `GET …/tickets/{ticket}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct TicketDetail {
    /// Content, status, Linear source (identifier + URL) and Slack thread.
    pub ticket: Ticket,
    pub assumed: bool,
    /// Repo of the target, for GitHub permalinks `blob/{sha}/{path}#L{a}-L{b}`.
    pub repo: Option<Repo>,
    pub customer: Option<Customer>,
    /// Ranked causes of the current triage first, superseded ones after (`superseded_at` set).
    pub causes: Vec<Cause>,
    pub runs: Vec<RunSummary>,
    pub pull_requests: Vec<FixPullRequest>,
    pub resolution: Option<Resolution>,
}

/// `PUT …/tickets/{ticket}/target`: user override (`TargetOrigin::Override`). Response:
/// `TicketTarget`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SetTargetRequest {
    pub repo_id: RepoId,
    pub sha: CommitSha,
}

/// `POST …/tickets/{ticket}/resolve`. Response: `Resolution`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ResolveTicketRequest {
    #[serde(default)]
    #[ts(optional)]
    pub confirmed_cause: Option<CauseId>,
    pub root_cause: String,
    #[serde(default)]
    #[ts(optional)]
    pub fix_pr_url: Option<String>,
    #[serde(default)]
    #[ts(optional)]
    pub repro_script: Option<ArtifactId>,
}

/// `POST …/causes/{cause}/diagnose` and `POST …/causes/{cause}/fix`: optional harness override of
/// the org default. Response: `RunStarted`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct StartHarnessRunRequest {
    #[serde(default)]
    #[ts(optional)]
    pub harness: Option<HarnessKind>,
}

/// `GET …/causes/{cause}/fix-brief?version=` (latest when absent).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
pub struct FixBriefQuery {
    #[ts(optional)]
    pub version: Option<u32>,
}

/// A rendered fix brief (fix.md "Storage and exposure").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct FixBriefView {
    pub markdown: String,
    pub version: u32,
    /// Newest version, for the version picker.
    pub latest_version: u32,
    pub diagnose_run: RunId,
    #[serde(with = "time::serde::rfc3339")]
    #[ts(type = "string")]
    pub created_at: OffsetDateTime,
    /// Download link of the immutable `brief-v{n}.md` artifact.
    pub artifact_url: String,
}
