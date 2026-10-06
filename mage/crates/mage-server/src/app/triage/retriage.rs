//! UC5 `RequestRetriage` (UI "Retriage"; triage.md "Retriage policy").

use std::sync::Arc;

use mage_core::domain::{JobPayload, MemberRole, OrgId, TicketId, TicketStatus, TriageTrigger, UserId};
use mage_core::ports::jobs::{Enqueued, JobQueue, NewJob};
use mage_core::ports::Clock;

use crate::app::triage::config::TriageConfig;
use crate::app::triage::job_error;
use crate::app::triage::start::live_triage_runs;
use crate::error::AppError;
use crate::ports::org::OrgStore;
use crate::ports::runs::RunStore;
use crate::ports::ticket::TicketStore;

/// Statuses a user may retriage from. `New` is included so orgs with `auto_triage = false` can
/// start the first triage by hand.
pub const RETRIAGE_FROM: [TicketStatus; 4] = [TicketStatus::New, TicketStatus::Triaged, TicketStatus::TriageFailed, TicketStatus::Triaging];

pub struct RequestRetriage {
    tickets: Arc<dyn TicketStore>,
    runs: Arc<dyn RunStore>,
    orgs: Arc<dyn OrgStore>,
    jobs: Arc<dyn JobQueue>,
    clock: Arc<dyn Clock>,
    cfg: TriageConfig,
}

impl RequestRetriage {
    pub fn new(
        tickets: Arc<dyn TicketStore>,
        runs: Arc<dyn RunStore>,
        orgs: Arc<dyn OrgStore>,
        jobs: Arc<dyn JobQueue>,
        clock: Arc<dyn Clock>,
        cfg: TriageConfig,
    ) -> Self {
        Self { tickets, runs, orgs, jobs, clock, cfg }
    }

    /// Enqueues `triage.ticket { trigger: Manual { by } }` for the ticket's current content.
    /// Over the ticket cost cap only an Admin or Owner may retriage.
    pub async fn run(&self, org: OrgId, ticket: TicketId, by: UserId) -> Result<Enqueued, AppError> {
        let t = self.tickets.get_ticket(org, ticket).await?.value;
        if !RETRIAGE_FROM.contains(&t.status) {
            return Err(AppError::Conflict(format!(
                "ticket is {}; retriage is possible only while new, triaged, triage failed or triaging",
                t.status.name()
            )));
        }
        if t.target.is_none() {
            return Err(AppError::Invalid("the ticket has no target repo/sha; set one first".into()));
        }
        let settings = self.cfg.clamp(self.orgs.triage_settings(org).await?);
        let now = self.clock.now();
        let spent: u64 = live_triage_runs(self.runs.as_ref(), org, ticket).await?.iter().map(|r| r.effective_usage(now).cost_micros).sum();
        if spent >= settings.ticket_cost_cap_micros {
            let role = self.orgs.membership(org, by).await?.map(|m| m.role);
            if !role.is_some_and(|r| r.at_least(MemberRole::Admin)) {
                return Err(AppError::Forbidden("the ticket reached its triage cost cap; an Admin or Owner can retriage".into()));
            }
        }
        let job = NewJob::new(JobPayload::TriageTicket {
            org_id: org,
            ticket_id: ticket,
            content_hash: t.content_hash,
            trigger: TriageTrigger::Manual { by },
        });
        let enqueued = self.jobs.enqueue(job).await.map_err(job_error)?;
        tracing::info!(%ticket, user = %by, job = %enqueued.job_id(), "manual retriage requested");
        Ok(enqueued)
    }
}

#[cfg(test)]
#[path = "retriage_tests.rs"]
mod tests;
