//! Runs, run events and artifacts (server.md catalog; runs.md "StreamEvents", "CancelRun").

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use ts_rs::TS;

use crate::domain::ids::hex_array;
use crate::domain::{
    ArtifactId, ArtifactKind, Budget, CauseId, Gate, HarnessKind, RepoId, Run, RunError, RunEvent, RunId, RunKind, RunStatus, RunSubject,
    TicketId, Usage,
};

/// `GET …/runs?kind=&status=&ticket=&cause=&repo=&cursor=&limit=`. Response: `Paged<RunSummary>`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
pub struct RunListQuery {
    #[ts(optional)]
    pub kind: Option<RunKind>,
    #[ts(optional)]
    pub status: Option<RunStatus>,
    #[ts(optional)]
    pub ticket: Option<TicketId>,
    #[ts(optional)]
    pub cause: Option<CauseId>,
    #[ts(optional)]
    pub repo: Option<RepoId>,
    #[ts(optional)]
    pub cursor: Option<String>,
    #[ts(optional)]
    pub limit: Option<u32>,
}

/// A run without its result, for lists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct RunSummary {
    pub id: RunId,
    pub kind: RunKind,
    pub status: RunStatus,
    pub subject: RunSubject,
    pub harness: Option<HarnessKind>,
    pub attempt: u16,
    pub parent: Option<RunId>,
    pub usage: Usage,
    pub budget: Budget,
    pub error: Option<RunError>,
    #[serde(with = "time::serde::rfc3339")]
    #[ts(type = "string")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    #[ts(type = "string | null")]
    pub started_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    #[ts(type = "string | null")]
    pub finished_at: Option<OffsetDateTime>,
}

impl RunSummary {
    pub fn of(run: &Run) -> Self {
        RunSummary {
            id: run.id,
            kind: run.kind,
            status: run.status,
            subject: run.subject,
            harness: run.harness,
            attempt: run.attempt,
            parent: run.parent,
            usage: run.usage,
            budget: run.budget,
            error: run.error.clone(),
            created_at: run.created_at,
            started_at: run.started_at,
            finished_at: run.finished_at,
        }
    }
}

/// `GET …/runs/{run}`: status, attempt, usage vs budget, result, gates and their Verify runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct RunDetail {
    pub run: Run,
    /// Every gate attempt, oldest first.
    pub gates: Vec<Gate>,
    /// Verify runs whose parent is this run (the UI opens their streams from `Notice` events).
    pub verify_runs: Vec<RunSummary>,
}

/// `GET …/runs/{run}/events?after=&limit=`: history before opening the SSE stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
pub struct RunEventsQuery {
    /// Events with `seq > after` (default 0).
    #[ts(optional)]
    pub after: Option<u64>,
    #[ts(optional)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct RunEventPage {
    pub events: Vec<RunEvent>,
    /// More events exist after the last one returned.
    pub more: bool,
}

/// `POST …/runs/{run}/steer` (status `Running` only; text ≤ [`SteerRequest::MAX_TEXT_BYTES`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SteerRequest {
    pub text: String,
}

impl SteerRequest {
    pub const MAX_TEXT_BYTES: usize = 8 * 1024;
}

/// Response of `POST …/runs/{run}/steer`: the queued command's `seq`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct CommandQueued {
    pub seq: u64,
}

/// Response of `POST …/runs/{run}/cancel` (a terminal run is a no-op).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct RunStatusView {
    pub status: RunStatus,
}

/// Artifact metadata (`GET …/artifacts/{artifact}`, `GET …/runs/{run}/artifacts`). Bytes come from
/// `…/artifacts/{artifact}/content` with `Range`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ArtifactView {
    pub id: ArtifactId,
    pub run_id: RunId,
    pub kind: ArtifactKind,
    pub name: String,
    pub content_type: String,
    pub bytes: u64,
    #[serde(with = "hex_array")]
    #[ts(type = "string")]
    pub sha256: [u8; 32],
    /// Upload completed and size verified.
    pub complete: bool,
    #[serde(with = "time::serde::rfc3339")]
    #[ts(type = "string")]
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ArtifactList {
    pub artifacts: Vec<ArtifactView>,
}

/// `POST …/artifacts/{artifact}/share` (`Trace` only): a 10-minute capability URL for the trace
/// origin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ShareLink {
    /// `{trace_url}/trace/index.html?trace=…`, opened in a new tab.
    pub url: String,
    #[serde(with = "time::serde::rfc3339")]
    #[ts(type = "string")]
    pub expires_at: OffsetDateTime,
}
