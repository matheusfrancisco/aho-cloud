//! `OnPullRequestEvent` (fix.md F3 "After the PR is open"): job `fix.pr_event {org_id, repo_id,
//! number, action, merged}` forwarded by intake for fix-app `pull_request` events on `mage/*`
//! branches.
//!
//! | Event | Effect |
//! |---|---|
//! | `closed`, merged | row `Opened → Closed{merged: true}`; the merged PR is what the UI offers as the resolution's `fix_pr_url` (the user still resolves) |
//! | `closed`, unmerged | row `Opened → Closed{merged: false}` + ticket `PrOpen → FixReady`, one transaction |
//! | `synchronize` | `Notice` on the Fix run: a human pushed; Mage never touches the branch again |
//! | other / unknown PR | ignored |

use std::sync::Arc;

use mage_core::domain::fix::{FixPrEvent, FixPrState};
use mage_core::domain::{OrgId, RepoId, TicketStatus};

use crate::app::fix::FixDeps;
use crate::error::AppError;
use crate::ports::ticket::{TicketStoreError, TicketTransition};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrEventOutcome {
    Closed { merged: bool, ticket_reopened: bool },
    Recorded,
    Ignored(&'static str),
}

#[derive(Clone)]
pub struct OnPullRequestEvent {
    d: Arc<FixDeps>,
}

impl OnPullRequestEvent {
    pub fn new(d: Arc<FixDeps>) -> Self {
        Self { d }
    }

    pub async fn run(&self, org: OrgId, repo: RepoId, number: u64, action: &str, merged: bool) -> Result<PrEventOutcome, AppError> {
        let d = &self.d;
        let Some(row) = d.tickets.pull_request_by_number(org, repo, number).await? else {
            return Ok(PrEventOutcome::Ignored("not a Mage fix PR"));
        };
        match action {
            "closed" => {
                if !matches!(row.state, FixPrState::Opened { .. }) {
                    return Ok(PrEventOutcome::Ignored("already closed"));
                }
                let expect = row.state.clone();
                let mut next = row.clone();
                next.apply(FixPrEvent::Closed { merged }, d.clock.now())?;
                if merged {
                    d.tickets.update_pull_request(org, &next, &expect, None, &[]).await?;
                    return Ok(PrEventOutcome::Closed { merged: true, ticket_reopened: false });
                }
                let ticket = d.tickets.get_ticket(org, row.ticket_id).await?.value;
                let reopen = ticket.status == TicketStatus::PrOpen;
                let transition = reopen.then_some(TicketTransition { expect: TicketStatus::PrOpen, next: TicketStatus::FixReady });
                match d.tickets.update_pull_request(org, &next, &expect, transition, &[]).await {
                    Ok(_) => Ok(PrEventOutcome::Closed { merged: false, ticket_reopened: reopen }),
                    Err(TicketStoreError::Conflict) if reopen => {
                        // The ticket left `PrOpen` meanwhile: record the close only.
                        d.tickets.update_pull_request(org, &next, &expect, None, &[]).await?;
                        Ok(PrEventOutcome::Closed { merged: false, ticket_reopened: false })
                    }
                    Err(e) => Err(e.into()),
                }
            }
            "synchronize" => {
                d.notice(
                    org,
                    row.fix_run,
                    format!("PR #{number} was updated by a push to {}; Mage will not modify this branch again.", row.branch),
                )
                .await?;
                Ok(PrEventOutcome::Recorded)
            }
            _ => Ok(PrEventOutcome::Ignored("action not handled")),
        }
    }
}
