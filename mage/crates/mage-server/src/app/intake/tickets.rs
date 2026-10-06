//! Tickets API use cases (server.md endpoint catalog, `mage_core::wire::api` tickets DTOs): list
//! with filters, detail, target override (intake.md 3.6 `Override`), resolve. Retriage is
//! triage's `RequestRetriage` (`adapters::driving::http::api_triage`).

use std::sync::Arc;

use mage_core::domain::{
    Cause, JobPayload, OrgId, RepoAccess, Resolution, TargetOrigin, Ticket, TicketId, TicketSource, TicketTarget, UserId,
    VersionReason,
};
use mage_core::ports::common::{Cursor, PageRequest};
use mage_core::ports::source_host::SourceHostError;
use mage_core::ports::{Clock, NewJob, RepoStore, SourceHost};
use mage_core::wire::api::{
    Paged, ResolveTicketRequest, RunSummary, SetTargetRequest, TicketDetail, TicketListQuery, TicketSummary, TopCause,
};
use url::Url;

use crate::app::intake::github::coords;
use crate::error::AppError;
use crate::ports::org::OrgStore;
use crate::ports::runs::{RunQuery, RunStore};
use crate::ports::ticket::{TicketFilter, TicketStore, TicketStoreError};

const DEFAULT_PAGE: u16 = 50;

pub struct TicketsApi {
    tickets: Arc<dyn TicketStore>,
    repos: Arc<dyn RepoStore>,
    orgs: Arc<dyn OrgStore>,
    runs: Arc<dyn RunStore>,
    source: Arc<dyn SourceHost>,
    clock: Arc<dyn Clock>,
}

fn identifier(t: &Ticket) -> String {
    let TicketSource::Linear { identifier, .. } = &t.source;
    identifier.clone()
}

/// Live causes ranked first, superseded ones after.
fn order_causes(mut causes: Vec<Cause>) -> Vec<Cause> {
    causes.sort_by_key(|c| (c.superseded_at.is_some(), c.superseded_at, c.rank));
    causes
}

impl TicketsApi {
    pub fn new(
        tickets: Arc<dyn TicketStore>,
        repos: Arc<dyn RepoStore>,
        orgs: Arc<dyn OrgStore>,
        runs: Arc<dyn RunStore>,
        source: Arc<dyn SourceHost>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self { tickets, repos, orgs, runs, source, clock }
    }

    pub async fn list(&self, org: OrgId, q: &TicketListQuery) -> Result<Paged<TicketSummary>, AppError> {
        let filter = TicketFilter {
            status: q.status.map(|s| vec![s]),
            customer: q.customer,
            repo: q.repo,
            text: q.q.as_ref().map(|t| t.trim().to_owned()).filter(|t| !t.is_empty()),
        };
        let limit = q.limit.map_or(DEFAULT_PAGE, |l| u16::try_from(l).unwrap_or(PageRequest::MAX_LIMIT));
        let page = PageRequest { after: q.cursor.clone().map(Cursor), limit };
        let page = self.tickets.list_tickets(org, &filter, page).await?;
        let mut items = Vec::with_capacity(page.items.len());
        for t in page.items {
            let top_cause = self
                .tickets
                .causes(org, t.id, false)
                .await?
                .into_iter()
                .min_by_key(|c| c.rank)
                .map(|c| TopCause { cause_id: c.id, title: c.title.clone(), confidence: c.confidence });
            items.push(TicketSummary {
                id: t.id,
                identifier: identifier(&t),
                title: t.title.clone(),
                status: t.status,
                assumed: t.target.as_ref().is_some_and(TicketTarget::assumed),
                target: t.target.clone(),
                top_cause,
                customer: t.customer,
                updated_at: t.updated_at,
            });
        }
        Ok(Paged { items, next_cursor: page.next.map(|c| c.0) })
    }

    pub async fn detail(&self, org: OrgId, id: TicketId) -> Result<TicketDetail, AppError> {
        let ticket = self.tickets.get_ticket(org, id).await?.value;
        let repo = match &ticket.target {
            Some(t) => Some(self.repos.repo(org, t.repo_id).await.map_err(|e| AppError::Internal(format!("target repo: {e}")))?),
            None => None,
        };
        let customer = match ticket.customer {
            Some(c) => Some(self.orgs.get_customer(org, c).await?),
            None => None,
        };
        let causes = order_causes(self.tickets.causes(org, id, true).await?);
        let q = RunQuery { ticket: Some(id), newest_first: true, ..RunQuery::default() };
        let runs = self.runs.list(org, &q).await?.iter().map(|r| RunSummary::of(&r.run)).collect();
        let pull_requests = self.tickets.pull_requests(org, id).await?;
        let resolution = self.tickets.resolution(org, id).await?;
        Ok(TicketDetail {
            assumed: ticket.target.as_ref().is_some_and(TicketTarget::assumed),
            ticket,
            repo,
            customer,
            causes,
            runs,
            pull_requests,
            resolution,
        })
    }

    /// User override of repo/sha (`TargetOrigin::Override`, sticky). The sha must exist in the
    /// repo; the version is pinned and indexed, and the issue is re-ingested so the content hash
    /// and the triage decision follow the new target.
    pub async fn set_target(&self, org: OrgId, id: TicketId, req: &SetTargetRequest) -> Result<TicketTarget, AppError> {
        let current = self.tickets.get_ticket(org, id).await?;
        if current.value.status.is_terminal() {
            return Err(AppError::Conflict(format!("ticket is {}", current.value.status.name())));
        }
        let repo = self.repos.repo(org, req.repo_id).await.map_err(|_| AppError::NotFound("repo".into()))?;
        if repo.access != RepoAccess::Granted {
            return Err(AppError::Invalid("Mage lost access to this repo".into()));
        }
        let sha = match self.source.resolve_ref(repo.installation_id, &coords(&repo), &req.sha.to_hex()).await {
            Ok(sha) => sha,
            Err(SourceHostError::NotFound | SourceHostError::Invalid(_)) => {
                return Err(AppError::Invalid(format!("commit {} not found in {}/{}", req.sha, repo.owner, repo.name)));
            }
            Err(e) => return Err(e.into()),
        };
        let target = TicketTarget { repo_id: repo.id, sha, origin: TargetOrigin::Override };
        self.repos
            .add_version_reason(org, repo.id, sha, VersionReason::OnDemand(id))
            .await
            .map_err(|e| AppError::Unavailable(format!("repo store: {e}")))?;
        let TicketSource::Linear { issue_id, .. } = &current.value.source;
        let jobs = [
            NewJob::new(JobPayload::IndexVersion { org_id: org, repo_id: repo.id, sha, reason: VersionReason::OnDemand(id) }),
            NewJob::new(JobPayload::IntakeLinearIssue { org_id: org, issue_id: issue_id.clone() }),
        ];
        match self.tickets.set_target(org, id, Some(target.clone()), current.rev, &jobs).await {
            Ok(_) => Ok(target),
            Err(TicketStoreError::Conflict) => Err(AppError::Conflict("the ticket changed; reload and try again".into())),
            Err(e) => Err(e.into()),
        }
    }

    /// Record a `Resolution` and move the ticket to `Resolved`.
    pub async fn resolve(&self, org: OrgId, id: TicketId, req: &ResolveTicketRequest, by: UserId) -> Result<Resolution, AppError> {
        let ticket = self.tickets.get_ticket(org, id).await?.value;
        if ticket.status.is_terminal() {
            return Err(AppError::Conflict(format!("ticket is {}", ticket.status.name())));
        }
        let cause = match req.confirmed_cause {
            Some(c) => Some(self.tickets.get_cause(org, c).await.map_err(|e| match e {
                TicketStoreError::NotFound => AppError::Invalid("confirmed cause not found".into()),
                other => other.into(),
            })?),
            None => None,
        };
        let fix_pr_url = match &req.fix_pr_url {
            Some(u) => Some(Url::parse(u).map_err(|e| AppError::Invalid(format!("fix_pr_url: {e}")))?),
            None => None,
        };
        let resolution = Resolution::new(&ticket, cause.as_ref(), req.root_cause.trim().to_owned(), fix_pr_url, req.repro_script, by, self.clock.now())?;
        match self.tickets.resolve(org, resolution.clone(), ticket.status, &[]).await {
            Ok(_) => Ok(resolution),
            Err(TicketStoreError::Conflict) => Err(AppError::Conflict("the ticket changed; reload and try again".into())),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests;
