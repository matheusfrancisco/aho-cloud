//! Session view, org selection and short-link resolution (server.md "Org selection",
//! `GET /api/v1/session`, `GET /api/v1/resolve/{id}`).

use std::sync::Arc;

use mage_core::domain::{ArtifactId, CauseId, MemberRole, OrgId, RunId, TicketId, UserId};
use mage_core::wire::api::{ResolvedId, ResolvedKind, SessionMembership, SessionView};

use crate::error::AppError;
use crate::ports::org::{OrgState, OrgStore};
use crate::ports::runs::{RunStore, RunStoreError};
use crate::ports::ticket::{TicketStore, TicketStoreError};

/// The caller's access to one org (what `OrgCtx` carries).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrgAccess {
    pub org_id: OrgId,
    pub role: MemberRole,
}

pub struct Directory {
    orgs: Arc<dyn OrgStore>,
    tickets: Arc<dyn TicketStore>,
    runs: Arc<dyn RunStore>,
}

impl Directory {
    pub fn new(orgs: Arc<dyn OrgStore>, tickets: Arc<dyn TicketStore>, runs: Arc<dyn RunStore>) -> Directory {
        Directory { orgs, tickets, runs }
    }

    /// Slug + session user → membership. Unknown org, non-member and an org being deleted all
    /// give `None` (same 404: no enumeration).
    pub async fn access(&self, user: UserId, slug: &str) -> Result<Option<OrgAccess>, AppError> {
        let Some(org) = self.orgs.org_by_slug(slug).await? else {
            return Ok(None);
        };
        let Some(membership) = self.orgs.membership(org.id, user).await? else {
            return Ok(None);
        };
        if self.orgs.org_state(org.id).await? != OrgState::Active {
            return Ok(None);
        }
        Ok(Some(OrgAccess { org_id: org.id, role: membership.role }))
    }

    /// `GET /api/v1/session` minus the CSRF token and flags the HTTP layer adds.
    pub async fn session_view(&self, user: UserId, csrf_token: String, features: Vec<String>) -> Result<SessionView, AppError> {
        let user_row = self.orgs.get_user(user).await?;
        let mut memberships: Vec<SessionMembership> = self
            .orgs
            .orgs_for_user(user)
            .await?
            .into_iter()
            .map(|(org, m)| SessionMembership { org: org.slug, name: org.name, role: m.role })
            .collect();
        memberships.sort_by(|a, b| a.org.cmp(&b.org));
        Ok(SessionView { user: user_row, memberships, csrf_token, features })
    }

    /// `GET /api/v1/resolve/{id}`: the org (slug) of a prefixed id among the caller's orgs.
    /// Every read is tenant-scoped, so another org's id is simply not found (404).
    pub async fn resolve(&self, user: UserId, id: &str) -> Result<ResolvedId, AppError> {
        let target = Target::parse(id).ok_or_else(|| AppError::NotFound("id".into()))?;
        for (org, _) in self.orgs.orgs_for_user(user).await? {
            let found = match target {
                Target::Ticket(t) => found(self.tickets.get_ticket(org.id, t).await.map(|_| None))?,
                Target::Cause(c) => found(self.tickets.get_cause(org.id, c).await.map(|cause| Some(cause.ticket_id)))?,
                Target::Run(r) => found_run(self.runs.get(org.id, r).await.map(|_| None))?,
                Target::Artifact(a) => found_run(self.runs.artifact(org.id, a).await.map(|_| None))?,
            };
            if let Some(ticket) = found {
                return Ok(ResolvedId { org: org.slug, kind: target.kind(), ticket });
            }
        }
        Err(AppError::NotFound("id".into()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    Ticket(TicketId),
    Cause(CauseId),
    Run(RunId),
    Artifact(ArtifactId),
}

impl Target {
    fn parse(id: &str) -> Option<Target> {
        if let Ok(t) = id.parse() {
            return Some(Target::Ticket(t));
        }
        if let Ok(c) = id.parse() {
            return Some(Target::Cause(c));
        }
        if let Ok(r) = id.parse() {
            return Some(Target::Run(r));
        }
        id.parse().ok().map(Target::Artifact)
    }

    fn kind(self) -> ResolvedKind {
        match self {
            Target::Ticket(_) => ResolvedKind::Ticket,
            Target::Cause(_) => ResolvedKind::Cause,
            Target::Run(_) => ResolvedKind::Run,
            Target::Artifact(_) => ResolvedKind::Artifact,
        }
    }
}

/// `Ok(Some(ticket))` when found in this org (the cause's ticket for causes), `Ok(None)` when not.
fn found(r: Result<Option<TicketId>, TicketStoreError>) -> Result<Option<Option<TicketId>>, AppError> {
    match r {
        Ok(ticket) => Ok(Some(ticket)),
        Err(TicketStoreError::NotFound) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn found_run(r: Result<Option<TicketId>, RunStoreError>) -> Result<Option<Option<TicketId>>, AppError> {
    match r {
        Ok(ticket) => Ok(Some(ticket)),
        Err(RunStoreError::NotFound) => Ok(None),
        Err(e) => Err(e.into()),
    }
}
