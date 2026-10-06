//! `Resolution` (domain.md "FixBrief and Resolution"): how a ticket was resolved; feeds
//! `similar_tickets` memory.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use url::Url;

use crate::domain::cause::Cause;
use crate::domain::error::{DomainError, ensure_same_org};
use crate::domain::ids::{ArtifactId, CauseId, OrgId, TicketId, UserId};
use crate::domain::ticket::Ticket;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
pub struct Resolution {
    pub ticket_id: TicketId,
    pub org_id: OrgId,
    pub confirmed_cause: Option<CauseId>,
    pub root_cause: String,
    #[ts(type = "string | null")]
    pub fix_pr_url: Option<Url>,
    pub repro_script: Option<ArtifactId>,
    pub resolved_by: UserId,
    #[serde(with = "time::serde::rfc3339")]
    #[ts(type = "string")]
    pub at: OffsetDateTime,
}

impl Resolution {
    /// The confirmed cause must belong to the ticket (same org, same ticket); the root cause text
    /// must be non-blank.
    pub fn new(
        ticket: &Ticket,
        confirmed_cause: Option<&Cause>,
        root_cause: String,
        fix_pr_url: Option<Url>,
        repro_script: Option<ArtifactId>,
        resolved_by: UserId,
        at: OffsetDateTime,
    ) -> Result<Resolution, DomainError> {
        if let Some(cause) = confirmed_cause {
            ensure_same_org(ticket.org_id, cause.org_id)?;
            if cause.ticket_id != ticket.id {
                return Err(DomainError::invalid("confirmed cause belongs to another ticket"));
            }
        }
        if root_cause.trim().is_empty() {
            return Err(DomainError::invalid("root cause is empty"));
        }
        Ok(Resolution {
            ticket_id: ticket.id,
            org_id: ticket.org_id,
            confirmed_cause: confirmed_cause.map(|c| c.id),
            root_cause,
            fix_pr_url,
            repro_script,
            resolved_by,
            at,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::cause::{CauseData, CauseStatus};
    use crate::domain::citation::{Confidence, ReproMethod};
    use crate::domain::ids::{ContentHash, RunId};
    use crate::domain::ticket::{TicketContent, TicketSource};
    use time::macros::datetime;
    use uuid::Uuid;

    fn ticket(org: u128, id: u128) -> Ticket {
        Ticket::new(
            TicketId::from_uuid(Uuid::from_u128(id)),
            OrgId::from_uuid(Uuid::from_u128(org)),
            TicketSource::Linear {
                issue_id: "i".into(),
                identifier: "ENG-2".into(),
                url: Url::parse("https://linear.app/x/issue/ENG-2").unwrap(),
            },
            TicketContent {
                title: "t".into(),
                body: String::new(),
                content_hash: ContentHash::from_bytes([0; 32]),
                slack_thread: None,
                customer: None,
                target: None,
                error_strings: vec![],
            },
            datetime!(2026-01-01 00:00 UTC),
        )
    }

    fn cause(org: u128, ticket: u128) -> Cause {
        Cause::rehydrate(CauseData {
            id: CauseId::from_uuid(Uuid::from_u128(50)),
            ticket_id: TicketId::from_uuid(Uuid::from_u128(ticket)),
            org_id: OrgId::from_uuid(Uuid::from_u128(org)),
            triage_run: RunId::from_uuid(Uuid::from_u128(4)),
            rank: 1,
            title: "t".into(),
            explanation: "e".into(),
            evidence: vec![],
            repro_steps: vec![],
            confirm_if: "c".into(),
            rule_out_if: "r".into(),
            confidence: Confidence::new(0.5).unwrap(),
            method: ReproMethod::Job,
            status: CauseStatus::Untested,
            superseded_at: None,
        })
    }

    fn resolve(t: &Ticket, c: Option<&Cause>, root: &str) -> Result<Resolution, DomainError> {
        Resolution::new(t, c, root.into(), None, None, UserId::from_uuid(Uuid::from_u128(8)), datetime!(2026-01-03 00:00 UTC))
    }

    #[test]
    fn resolution_links_are_checked() {
        let t = ticket(1, 10);
        assert!(resolve(&t, Some(&cause(1, 10)), "null customer").is_ok());
        assert!(resolve(&t, None, "config").is_ok());
        assert_eq!(resolve(&t, Some(&cause(2, 10)), "x").unwrap_err(), DomainError::CrossTenant);
        assert!(resolve(&t, Some(&cause(1, 11)), "x").is_err());
        assert!(resolve(&t, None, "  ").is_err());
    }
}
