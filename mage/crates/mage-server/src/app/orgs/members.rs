//! Members and invites (server.md UC4, "RBAC"): invite a GitHub login resolved to its stable id,
//! withdraw, change roles (Owner), remove members; the data-dependent rules live in
//! `crate::domain::members`.

use std::sync::Arc;

use uuid::Uuid;

use mage_core::domain::{MemberRole, Membership, OrgId, UserId};
use mage_core::ports::common::PageRequest;
use mage_core::ports::{Clock, IdGen};
use mage_core::wire::api::{InviteRequest, InviteView, MemberList, MemberView};

use crate::app::auth::{RequestMeta, audit, event, role_name};
use crate::domain::members::{MemberRuleError, check_invite, check_remove, check_role_change};
use crate::error::AppError;
use crate::ports::audit::{Actor, AuditAction, AuditLog, AuditTarget};
use crate::ports::auth::{IdentityProvider, IdentityProviderKind};
use crate::ports::org::{Invite, InviteId, OrgStore, OrgStoreError};

/// Pending invites expire after 14 days (storage.md `invites`).
pub const INVITE_TTL: time::Duration = time::Duration::days(14);

/// The caller of a members use case (from `OrgCtx`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caller {
    pub org: OrgId,
    pub user: UserId,
    pub role: MemberRole,
}

pub struct Members {
    orgs: Arc<dyn OrgStore>,
    /// GitHub: invites name a GitHub login (UC4).
    github: Arc<dyn IdentityProvider>,
    audit: Arc<dyn AuditLog>,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGen>,
}

impl Members {
    pub fn new(orgs: Arc<dyn OrgStore>, github: Arc<dyn IdentityProvider>, audit: Arc<dyn AuditLog>, clock: Arc<dyn Clock>, ids: Arc<dyn IdGen>) -> Members {
        Members { orgs, github, audit, clock, ids }
    }

    async fn all_members(&self, org: OrgId) -> Result<Vec<MemberView>, AppError> {
        let mut out = Vec::new();
        let mut page = PageRequest::first(PageRequest::MAX_LIMIT);
        loop {
            let got = self.orgs.members(org, page.clone()).await?;
            out.extend(got.items.into_iter().map(|(user, m)| MemberView { user, role: m.role }));
            match got.next {
                Some(cursor) => page = page.after(cursor),
                None => return Ok(out),
            }
        }
    }

    async fn owners(&self, org: OrgId) -> Result<usize, AppError> {
        Ok(self.all_members(org).await?.iter().filter(|m| m.role == MemberRole::Owner).count())
    }

    /// `GET …/members`: members and pending (unexpired) invites.
    pub async fn list(&self, org: OrgId) -> Result<MemberList, AppError> {
        let now = self.clock.now();
        let members = self.all_members(org).await?;
        let invites = self.orgs.invites(org).await?.into_iter().filter(|i| i.expires_at > now).map(invite_view).collect();
        Ok(MemberList { members, invites })
    }

    /// UC4: invited role ≤ caller's role; login → stable GitHub id (unknown → 422); pending
    /// invite keyed by subject (Conflict if one is pending); audit `member.invited`.
    pub async fn invite(&self, caller: Caller, req: &InviteRequest, meta: &RequestMeta) -> Result<InviteView, AppError> {
        check_invite(caller.role, req.role).map_err(rule_error)?;
        let login = req.github_login.trim();
        if login.is_empty() {
            return Err(AppError::Invalid("github_login is empty".into()));
        }
        let identity = self
            .github
            .lookup_login(login)
            .await?
            .filter(|i| i.provider == IdentityProviderKind::GitHub)
            .ok_or_else(|| AppError::Invalid(format!("unknown GitHub login {login:?}")))?;
        let now = self.clock.now();
        let invite = Invite {
            id: InviteId(self.ids.uuid_v7()),
            org_id: caller.org,
            provider: identity.provider.as_str().to_owned(),
            subject: identity.subject.clone(),
            login: identity.login.clone(),
            role: req.role,
            invited_by: caller.user,
            created_at: now,
            expires_at: now + INVITE_TTL,
        };
        self.orgs.create_invite(&invite).await.map_err(|e| match e {
            OrgStoreError::Conflict => AppError::Conflict(format!("an invite for {} is already pending", identity.login)),
            other => other.into(),
        })?;
        audit(
            self.audit.as_ref(),
            event(Some(caller.org), Actor::User(caller.user), AuditAction::MemberInvited, AuditTarget::new("invite", invite.id.to_string()), meta)
                .detail("login", identity.login.clone())
                .detail("subject", identity.subject.clone())
                .detail("role", role_name(req.role)),
        )
        .await?;
        Ok(invite_view(invite))
    }

    pub async fn withdraw(&self, caller: Caller, invite: &str, meta: &RequestMeta) -> Result<(), AppError> {
        let id = parse_invite_id(invite).ok_or_else(|| AppError::NotFound("invite".into()))?;
        self.orgs.withdraw_invite(caller.org, id).await?;
        audit(
            self.audit.as_ref(),
            event(Some(caller.org), Actor::User(caller.user), AuditAction::MemberInviteWithdrawn, AuditTarget::new("invite", id.to_string()), meta),
        )
        .await
    }

    /// `PUT …/members/{user}` (Owner): demoting the last Owner is refused.
    pub async fn set_role(&self, caller: Caller, target: UserId, role: MemberRole, meta: &RequestMeta) -> Result<MemberView, AppError> {
        let current = self.orgs.membership(caller.org, target).await?.ok_or_else(|| AppError::NotFound("member".into()))?;
        let owners = self.owners(caller.org).await?;
        check_role_change(caller.role, current.role, role, owners).map_err(rule_error)?;
        if current.role != role {
            self.orgs.put_membership(&Membership { org_id: caller.org, user_id: target, role }).await.map_err(last_owner)?;
            audit(
                self.audit.as_ref(),
                event(Some(caller.org), Actor::User(caller.user), AuditAction::MemberRoleChanged, AuditTarget::new("user", target.to_string()), meta)
                    .detail("old_role", role_name(current.role))
                    .detail("new_role", role_name(role)),
            )
            .await?;
        }
        Ok(MemberView { user: self.orgs.get_user(target).await?, role })
    }

    /// `DELETE …/members/{user}`: Admins remove Members, Owners anyone; never the last Owner.
    pub async fn remove(&self, caller: Caller, target: UserId, meta: &RequestMeta) -> Result<(), AppError> {
        let current = self.orgs.membership(caller.org, target).await?.ok_or_else(|| AppError::NotFound("member".into()))?;
        let owners = self.owners(caller.org).await?;
        check_remove(caller.role, current.role, owners).map_err(rule_error)?;
        self.orgs.remove_membership(caller.org, target).await.map_err(last_owner)?;
        audit(
            self.audit.as_ref(),
            event(Some(caller.org), Actor::User(caller.user), AuditAction::MemberRemoved, AuditTarget::new("user", target.to_string()), meta)
                .detail("role", role_name(current.role)),
        )
        .await
    }
}

fn rule_error(e: MemberRuleError) -> AppError {
    match e {
        MemberRuleError::LastOwner => AppError::Conflict(e.to_string()),
        other => AppError::Forbidden(other.to_string()),
    }
}

fn last_owner(e: OrgStoreError) -> AppError {
    match e {
        OrgStoreError::Conflict => AppError::Conflict(MemberRuleError::LastOwner.to_string()),
        other => other.into(),
    }
}

fn invite_view(i: Invite) -> InviteView {
    InviteView { id: i.id.to_string(), login: i.login, role: i.role, invited_by: i.invited_by, created_at: i.created_at, expires_at: i.expires_at }
}

/// `inv_<32 hex>` (the `InviteId` display form) back to the id.
pub fn parse_invite_id(s: &str) -> Option<InviteId> {
    let hex = s.strip_prefix("inv_")?;
    if hex.len() != 32 {
        return None;
    }
    Uuid::try_parse(hex).ok().map(InviteId)
}

