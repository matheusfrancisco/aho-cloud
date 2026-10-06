//! `Ticket` and `TicketStatus` (domain.md "Ticket"; intake.md, triage.md and fix.md Additions).

use std::ops::Deref;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use url::Url;

use crate::domain::error::DomainError;
use crate::domain::gates::FixGatePassed;
use crate::domain::ids::{CommitSha, ContentHash, CustomerId, OrgId, RepoId, RunId, TicketId};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TicketSource {
    Linear {
        issue_id: String,
        /// Human identifier, e.g. `ENG-412`.
        identifier: String,
        #[ts(type = "string")]
        url: Url,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, ts_rs::TS)]
pub struct SlackThreadRef {
    pub team: String,
    pub channel: String,
    pub ts: String,
}

/// Where a ticket's target sha came from (intake.md). Decides stickiness on re-ingest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TargetOrigin {
    /// Set by a user in the UI; never replaced by intake.
    Override,
    /// A directive in the issue text.
    Directive,
    Deployment {
        environment: String,
    },
    /// Repo default branch head (the sha is assumed).
    DefaultHead,
}

/// The commit a ticket is diagnosed at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
pub struct TicketTarget {
    pub repo_id: RepoId,
    pub sha: CommitSha,
    pub origin: TargetOrigin,
}

impl TicketTarget {
    /// `assumed == (origin == DefaultHead)`: the deployed version is unknown.
    pub fn assumed(&self) -> bool {
        self.origin == TargetOrigin::DefaultHead
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
pub enum TicketStatus {
    New,
    Triaging,
    Triaged,
    TriageFailed,
    Diagnosing,
    Diagnosed,
    Fixing,
    FixReady,
    PrOpen,
    Resolved,
    Closed,
}

/// Events of the `TicketStatus` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TicketEvent {
    /// Triage started for `content_hash` (also content changed / manual retriage / requeue while
    /// `Triaging`); `run` becomes the triage fencing token.
    TriageStarted {
        run: RunId,
        content_hash: ContentHash,
    },
    TriageSucceeded {
        run: RunId,
    },
    TriageFailed {
        run: RunId,
    },
    DiagnoseStarted,
    /// All diagnose runs are terminal.
    DiagnosesFinished {
        any_reproduced: bool,
    },
    FixStarted,
    /// Only via [`Ticket::fix_ready`] semantics: requires the gate's proof.
    FixGatePassed(FixGatePassed),
    /// Fix run failed, timed out, was cancelled or errored.
    FixEnded,
    PrOpened,
    PrClosedUnmerged,
    ResolvedByUser,
    ClosedUpstream,
}

impl TicketEvent {
    pub fn name(&self) -> &'static str {
        match self {
            Self::TriageStarted { .. } => "triage_started",
            Self::TriageSucceeded { .. } => "triage_succeeded",
            Self::TriageFailed { .. } => "triage_failed",
            Self::DiagnoseStarted => "diagnose_started",
            Self::DiagnosesFinished { .. } => "diagnoses_finished",
            Self::FixStarted => "fix_started",
            Self::FixGatePassed(_) => "fix_gate_passed",
            Self::FixEnded => "fix_ended",
            Self::PrOpened => "pr_opened",
            Self::PrClosedUnmerged => "pr_closed_unmerged",
            Self::ResolvedByUser => "resolved_by_user",
            Self::ClosedUpstream => "closed_upstream",
        }
    }
}

impl TicketStatus {
    pub fn name(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Triaging => "triaging",
            Self::Triaged => "triaged",
            Self::TriageFailed => "triage_failed",
            Self::Diagnosing => "diagnosing",
            Self::Diagnosed => "diagnosed",
            Self::Fixing => "fixing",
            Self::FixReady => "fix_ready",
            Self::PrOpen => "pr_open",
            Self::Resolved => "resolved",
            Self::Closed => "closed",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Resolved | Self::Closed)
    }

    /// One row of the table. Retriage is refused while `Diagnosing` or `Fixing` (and from every
    /// later state). Terminal states accept no event.
    pub fn apply(self, event: &TicketEvent) -> Result<TicketStatus, DomainError> {
        use TicketEvent as E;
        use TicketStatus as S;
        let next = match (self, event) {
            (S::New | S::Triaged | S::TriageFailed | S::Triaging, E::TriageStarted { .. }) => Some(S::Triaging),
            (S::Triaging, E::TriageSucceeded { .. }) => Some(S::Triaged),
            (S::Triaging, E::TriageFailed { .. }) => Some(S::TriageFailed),
            (S::Triaged, E::DiagnoseStarted) => Some(S::Diagnosing),
            (S::Diagnosing, E::DiagnosesFinished { any_reproduced: true }) => Some(S::Diagnosed),
            (S::Diagnosing, E::DiagnosesFinished { any_reproduced: false }) => Some(S::Triaged),
            (S::Diagnosed | S::FixReady, E::FixStarted) => Some(S::Fixing),
            (S::Fixing, E::FixGatePassed(_)) => Some(S::FixReady),
            (S::Fixing, E::FixEnded) => Some(S::Diagnosed),
            (S::FixReady, E::PrOpened) => Some(S::PrOpen),
            (S::PrOpen, E::PrClosedUnmerged) => Some(S::FixReady),
            (from, E::ResolvedByUser | E::ClosedUpstream) if !from.is_terminal() => {
                Some(if matches!(event, E::ResolvedByUser) { S::Resolved } else { S::Closed })
            }
            _ => None,
        };
        next.ok_or(DomainError::InvalidTransition { from: self.name(), event: event.name() })
    }
}

/// Ingested, user-visible content of a ticket (written by intake).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TicketContent {
    pub title: String,
    pub body: String,
    pub content_hash: ContentHash,
    pub slack_thread: Option<SlackThreadRef>,
    pub customer: Option<CustomerId>,
    pub target: Option<TicketTarget>,
    pub error_strings: Vec<String>,
}

/// Fields of a ticket (read through `Ticket`'s `Deref`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
pub struct TicketData {
    pub id: TicketId,
    pub org_id: OrgId,
    pub source: TicketSource,
    pub title: String,
    pub body: String,
    pub content_hash: ContentHash,
    pub slack_thread: Option<SlackThreadRef>,
    pub customer: Option<CustomerId>,
    pub target: Option<TicketTarget>,
    pub error_strings: Vec<String>,
    pub status: TicketStatus,
    /// Content hash the current triage consumed (written when triage starts).
    pub triaged_hash: Option<ContentHash>,
    /// Fencing token: only this triage run may change the ticket's triage outcome.
    pub triage_run: Option<RunId>,
    #[serde(with = "time::serde::rfc3339")]
    #[ts(type = "string")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    #[ts(type = "string")]
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(transparent)]
pub struct Ticket(TicketData);

impl Deref for Ticket {
    type Target = TicketData;

    fn deref(&self) -> &TicketData {
        &self.0
    }
}

impl Ticket {
    pub fn new(id: TicketId, org_id: OrgId, source: TicketSource, content: TicketContent, created_at: OffsetDateTime) -> Ticket {
        let TicketContent { title, body, content_hash, slack_thread, customer, target, error_strings } = content;
        Ticket(TicketData {
            id,
            org_id,
            source,
            title,
            body,
            content_hash,
            slack_thread,
            customer,
            target,
            error_strings,
            status: TicketStatus::New,
            triaged_hash: None,
            triage_run: None,
            created_at,
            updated_at: created_at,
        })
    }

    /// Rebuild a stored ticket (storage adapters only).
    pub fn rehydrate(data: TicketData) -> Ticket {
        Ticket(data)
    }

    pub fn into_data(self) -> TicketData {
        self.0
    }

    /// Re-ingested content. Terminal tickets are frozen.
    pub fn update_content(&mut self, content: TicketContent, at: OffsetDateTime) -> Result<(), DomainError> {
        if self.0.status.is_terminal() {
            return Err(DomainError::invalid(format!("ticket is {}", self.0.status.name())));
        }
        let TicketContent { title, body, content_hash, slack_thread, customer, target, error_strings } = content;
        let d = &mut self.0;
        d.title = title;
        d.body = body;
        d.content_hash = content_hash;
        d.slack_thread = slack_thread;
        d.customer = customer;
        d.target = target;
        d.error_strings = error_strings;
        d.updated_at = at;
        Ok(())
    }

    /// Content differs from what the current triage consumed.
    pub fn content_changed_since_triage(&self) -> bool {
        self.0.triaged_hash != Some(self.0.content_hash)
    }

    /// Applies a status event. `TriageStarted` records `triage_run`/`triaged_hash`;
    /// `TriageSucceeded`/`TriageFailed` must come from the current `triage_run`.
    pub fn apply(&mut self, event: TicketEvent, at: OffsetDateTime) -> Result<(), DomainError> {
        let stale_run = match &event {
            TicketEvent::TriageSucceeded { run } | TicketEvent::TriageFailed { run } => self.0.triage_run != Some(*run),
            _ => false,
        };
        if stale_run {
            return Err(DomainError::invalid("triage run is not the ticket's current triage run"));
        }
        let next = self.0.status.apply(&event)?;
        if let TicketEvent::TriageStarted { run, content_hash } = event {
            self.0.triage_run = Some(run);
            self.0.triaged_hash = Some(content_hash);
        }
        self.0.status = next;
        self.0.updated_at = at;
        Ok(())
    }

    /// `Fixing → FixReady`, only with the fix gate's proof.
    pub fn fix_ready(&mut self, proof: &FixGatePassed, at: OffsetDateTime) -> Result<(), DomainError> {
        self.apply(TicketEvent::FixGatePassed(proof.clone()), at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::fix::ChangeSet;
    use crate::domain::gates::testing::fix_gate_passed;
    use crate::domain::ids::{ArtifactId, CauseId, GateId};
    use time::macros::datetime;
    use uuid::Uuid;

    const T0: OffsetDateTime = datetime!(2026-04-01 09:00 UTC);
    const T1: OffsetDateTime = datetime!(2026-04-01 09:10 UTC);

    const ALL: [TicketStatus; 11] = [
        TicketStatus::New,
        TicketStatus::Triaging,
        TicketStatus::Triaged,
        TicketStatus::TriageFailed,
        TicketStatus::Diagnosing,
        TicketStatus::Diagnosed,
        TicketStatus::Fixing,
        TicketStatus::FixReady,
        TicketStatus::PrOpen,
        TicketStatus::Resolved,
        TicketStatus::Closed,
    ];

    fn run(n: u128) -> RunId {
        RunId::from_uuid(Uuid::from_u128(n))
    }

    fn token() -> FixGatePassed {
        fix_gate_passed(
            GateId::from_uuid(Uuid::from_u128(1)),
            run(2),
            CauseId::from_uuid(Uuid::from_u128(3)),
            ChangeSet {
                base_sha: CommitSha::from_bytes([1; 20]),
                patch: ArtifactId::from_uuid(Uuid::from_u128(4)),
                patch_sha256: [2; 32],
                regression: None,
            },
        )
    }

    fn events() -> Vec<TicketEvent> {
        vec![
            TicketEvent::TriageStarted { run: run(10), content_hash: ContentHash::from_bytes([1; 32]) },
            TicketEvent::TriageSucceeded { run: run(10) },
            TicketEvent::TriageFailed { run: run(10) },
            TicketEvent::DiagnoseStarted,
            TicketEvent::DiagnosesFinished { any_reproduced: true },
            TicketEvent::DiagnosesFinished { any_reproduced: false },
            TicketEvent::FixStarted,
            TicketEvent::FixGatePassed(token()),
            TicketEvent::FixEnded,
            TicketEvent::PrOpened,
            TicketEvent::PrClosedUnmerged,
            TicketEvent::ResolvedByUser,
            TicketEvent::ClosedUpstream,
        ]
    }

    /// domain.md table plus triage.md (`Triaging → Triaging`) and fix.md (`FixReady → Fixing`,
    /// `PrOpen → FixReady`) Additions, written independently of `apply`.
    fn expected(from: TicketStatus, event: &TicketEvent) -> Option<TicketStatus> {
        use TicketEvent as E;
        use TicketStatus as S;
        if from.is_terminal() {
            return None;
        }
        Some(match (from, event) {
            (_, E::ResolvedByUser) => S::Resolved,
            (_, E::ClosedUpstream) => S::Closed,
            (S::New, E::TriageStarted { .. }) => S::Triaging,
            (S::Triaged, E::TriageStarted { .. }) => S::Triaging,
            (S::TriageFailed, E::TriageStarted { .. }) => S::Triaging,
            (S::Triaging, E::TriageStarted { .. }) => S::Triaging,
            (S::Triaging, E::TriageSucceeded { .. }) => S::Triaged,
            (S::Triaging, E::TriageFailed { .. }) => S::TriageFailed,
            (S::Triaged, E::DiagnoseStarted) => S::Diagnosing,
            (S::Diagnosing, E::DiagnosesFinished { any_reproduced: true }) => S::Diagnosed,
            (S::Diagnosing, E::DiagnosesFinished { any_reproduced: false }) => S::Triaged,
            (S::Diagnosed, E::FixStarted) => S::Fixing,
            (S::FixReady, E::FixStarted) => S::Fixing,
            (S::Fixing, E::FixGatePassed(_)) => S::FixReady,
            (S::Fixing, E::FixEnded) => S::Diagnosed,
            (S::FixReady, E::PrOpened) => S::PrOpen,
            (S::PrOpen, E::PrClosedUnmerged) => S::FixReady,
            _ => return None,
        })
    }

    #[test]
    fn ticket_status_full_transition_table() {
        for from in ALL {
            for event in events() {
                let got = from.apply(&event);
                match expected(from, &event) {
                    Some(to) => assert_eq!(got, Ok(to), "{} + {}", from.name(), event.name()),
                    None => assert_eq!(
                        got,
                        Err(DomainError::InvalidTransition { from: from.name(), event: event.name() }),
                        "{} + {} must be invalid",
                        from.name(),
                        event.name()
                    ),
                }
            }
        }
    }

    #[test]
    fn retriage_refused_while_diagnosing_or_fixing() {
        let ev = TicketEvent::TriageStarted { run: run(1), content_hash: ContentHash::from_bytes([0; 32]) };
        assert!(TicketStatus::Diagnosing.apply(&ev).is_err());
        assert!(TicketStatus::Fixing.apply(&ev).is_err());
    }

    fn ticket() -> Ticket {
        Ticket::new(
            TicketId::from_uuid(Uuid::from_u128(1)),
            OrgId::from_uuid(Uuid::from_u128(2)),
            TicketSource::Linear {
                issue_id: "i1".into(),
                identifier: "ENG-412".into(),
                url: Url::parse("https://linear.app/acme/issue/ENG-412").unwrap(),
            },
            TicketContent {
                title: "Invoice export fails".into(),
                body: "body".into(),
                content_hash: ContentHash::from_bytes([5; 32]),
                slack_thread: None,
                customer: None,
                target: None,
                error_strings: vec![],
            },
            T0,
        )
    }

    #[test]
    fn triage_fencing_and_hash_tracking() {
        let mut t = ticket();
        assert!(t.content_changed_since_triage());
        t.apply(TicketEvent::TriageStarted { run: run(1), content_hash: t.content_hash }, T1).unwrap();
        assert_eq!((t.status, t.triage_run, t.updated_at), (TicketStatus::Triaging, Some(run(1)), T1));
        assert!(!t.content_changed_since_triage());
        // Retriage while triaging moves the fence.
        t.apply(TicketEvent::TriageStarted { run: run(2), content_hash: t.content_hash }, T1).unwrap();
        // The superseded run cannot finish the triage.
        assert!(t.apply(TicketEvent::TriageSucceeded { run: run(1) }, T1).is_err());
        assert_eq!(t.status, TicketStatus::Triaging);
        t.apply(TicketEvent::TriageSucceeded { run: run(2) }, T1).unwrap();
        assert_eq!(t.status, TicketStatus::Triaged);
    }

    #[test]
    fn fix_ready_requires_fixing() {
        let mut t = ticket();
        assert!(t.fix_ready(&token(), T1).is_err());
        let mut data = t.into_data();
        data.status = TicketStatus::Fixing;
        let mut t = Ticket::rehydrate(data);
        t.fix_ready(&token(), T1).unwrap();
        assert_eq!(t.status, TicketStatus::FixReady);
    }

    #[test]
    fn terminal_ticket_content_is_frozen() {
        let mut t = ticket();
        t.apply(TicketEvent::ClosedUpstream, T1).unwrap();
        let content = TicketContent {
            title: "x".into(),
            body: String::new(),
            content_hash: ContentHash::from_bytes([9; 32]),
            slack_thread: None,
            customer: None,
            target: None,
            error_strings: vec![],
        };
        assert!(t.update_content(content, T1).is_err());
    }

    #[test]
    fn target_assumed_follows_origin() {
        let mut target = TicketTarget {
            repo_id: RepoId::from_uuid(Uuid::from_u128(1)),
            sha: CommitSha::from_bytes([0; 20]),
            origin: TargetOrigin::DefaultHead,
        };
        assert!(target.assumed());
        target.origin = TargetOrigin::Deployment { environment: "prod".into() };
        assert!(!target.assumed());
    }
}
