//! `OrgStore` (storage.md "`OrgStore`"): orgs, users and login identities, memberships, invites,
//! per-org settings, integrations, intake config, customers.
//!
//! Additions beyond storage.md: `integration_settings` / `set_integration_settings` (per-kind
//! non-secret settings in `integrations.settings`, intake.md). Store writes that the UI shows also
//! append an org feed entry in the same transaction (`Member`, `Integration`, `Settings`).

use std::fmt;

use async_trait::async_trait;
use mage_core::domain::fix::FixSettings;
use mage_core::domain::{Customer, CustomerId, ExternalRef, Integration, IntegrationKind, IntegrationStatus, MemberRole, Membership, Org, OrgId, User, UserId};
use mage_core::ports::common::{Page, PageRequest};
use mage_core::ports::jobs::NewJob;
use mage_core::wire::api::{GeneralSettings, TriageSettings};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::domain::intake::{IntakeConfig, IntegrationSettings};
use crate::error::AppError;
use crate::ports::auth::ExternalIdentity;
use crate::ports::llm::ModelSettings;

/// One `org_settings` row per area.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SettingsArea {
    General,
    Model,
    Triage,
    Fix,
}

impl SettingsArea {
    /// `org_settings.area` value.
    pub const fn as_str(self) -> &'static str {
        match self {
            SettingsArea::General => "general",
            SettingsArea::Model => "model",
            SettingsArea::Triage => "triage",
            SettingsArea::Fix => "fix",
        }
    }
}

/// Pending invite id (uuid v7 from `IdGen`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct InviteId(pub Uuid);

impl InviteId {
    pub const fn from_uuid(uuid: Uuid) -> InviteId {
        InviteId(uuid)
    }

    pub const fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl From<Uuid> for InviteId {
    fn from(uuid: Uuid) -> InviteId {
        InviteId(uuid)
    }
}

impl fmt::Display for InviteId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "inv_{}", self.0.simple())
    }
}

/// A pending invite keyed by the invitee's stable login identity `(provider, subject)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invite {
    pub id: InviteId,
    pub org_id: OrgId,
    /// `IdentityProviderKind::as_str()` (`"github"`, `"google"`).
    pub provider: String,
    pub subject: String,
    pub login: String,
    pub role: MemberRole,
    pub invited_by: UserId,
    pub created_at: OffsetDateTime,
    pub expires_at: OffsetDateTime,
}

/// Org lifecycle (storage.md "Retention and deletion").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrgState {
    Active,
    /// Deletion requested: every API and MCP call for the org is refused; `ops.delete_org` runs
    /// after the grace period.
    Deleting,
}

impl OrgState {
    /// `orgs.state` value.
    pub const fn as_str(self) -> &'static str {
        match self {
            OrgState::Active => "active",
            OrgState::Deleting => "deleting",
        }
    }

    pub fn parse(s: &str) -> Option<OrgState> {
        [OrgState::Active, OrgState::Deleting].into_iter().find(|v| v.as_str() == s)
    }
}

/// Per-org retention (`orgs.retention`, storage.md "Retention and deletion").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionPolicy {
    /// Default 90; `7..=730`.
    pub run_artifacts_days: u16,
    /// Default 90; `7..=730`.
    pub run_events_days: u16,
    /// Default true: scripts / patches / briefs referenced by a resolution, brief or PR are kept.
    pub keep_referenced_artifacts: bool,
}

impl RetentionPolicy {
    pub const DAYS: std::ops::RangeInclusive<u16> = 7..=730;

    /// Both day counts within [`Self::DAYS`].
    pub fn validate(&self) -> Result<(), String> {
        for (name, days) in [("run_artifacts_days", self.run_artifacts_days), ("run_events_days", self.run_events_days)] {
            if !Self::DAYS.contains(&days) {
                return Err(format!("{name} {days} outside 7..=730"));
            }
        }
        Ok(())
    }
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        RetentionPolicy { run_artifacts_days: 90, run_events_days: 90, keep_referenced_artifacts: true }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OrgStoreError {
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
pub trait OrgStore: Send + Sync {
    // orgs
    /// Inserts the org (state `Active`) and the owner's `Owner` membership. Slug or id taken →
    /// `Conflict`; unknown owner → `Invalid`.
    async fn create_org(&self, org: &Org, owner: UserId, retention: &RetentionPolicy) -> Result<(), OrgStoreError>;
    async fn get_org(&self, org: OrgId) -> Result<Org, OrgStoreError>;
    /// Platform lookup.
    async fn org_by_slug(&self, slug: &str) -> Result<Option<Org>, OrgStoreError>;
    async fn org_state(&self, org: OrgId) -> Result<OrgState, OrgStoreError>;
    /// CAS `expect → next`; `Deleting` records `deletion_requested_at`, `Active` clears it; jobs in
    /// the same transaction.
    async fn set_org_state(&self, org: OrgId, expect: OrgState, next: OrgState, jobs: &[NewJob]) -> Result<(), OrgStoreError>;
    async fn retention(&self, org: OrgId) -> Result<RetentionPolicy, OrgStoreError>;
    /// Out-of-range days → `Invalid`.
    async fn set_retention(&self, org: OrgId, policy: &RetentionPolicy) -> Result<(), OrgStoreError>;

    // settings: one jsonb row per area; getters return `Default` when no row exists
    async fn triage_settings(&self, org: OrgId) -> Result<TriageSettings, OrgStoreError>;
    async fn set_triage_settings(&self, org: OrgId, s: &TriageSettings) -> Result<(), OrgStoreError>;
    async fn fix_settings(&self, org: OrgId) -> Result<FixSettings, OrgStoreError>;
    async fn set_fix_settings(&self, org: OrgId, s: &FixSettings) -> Result<(), OrgStoreError>;
    async fn model_settings(&self, org: OrgId) -> Result<ModelSettings, OrgStoreError>;
    async fn set_model_settings(&self, org: OrgId, s: &ModelSettings) -> Result<(), OrgStoreError>;
    async fn general_settings(&self, org: OrgId) -> Result<GeneralSettings, OrgStoreError>;
    async fn set_general_settings(&self, org: OrgId, s: &GeneralSettings) -> Result<(), OrgStoreError>;

    // users, identities, memberships, invites (users are global, memberships per org)
    /// Platform. Keyed by `(provider, subject)`: an existing identity refreshes login / avatar and
    /// the user's name (and email when the provider gave one) and returns that user; a new one
    /// creates user `new_id`. Email taken by another user → `Conflict` (never auto-linked). A
    /// GitHub identity without a readable email gets GitHub's noreply address.
    async fn upsert_user_by_identity(&self, identity: &ExternalIdentity, new_id: UserId) -> Result<User, OrgStoreError>;
    async fn get_user(&self, id: UserId) -> Result<User, OrgStoreError>;
    /// Platform. Links a second login identity to `user` (only while logged in, server.md: never
    /// by matching email). Already linked to `user` → Ok; linked to another user → `Conflict`;
    /// unknown user → `NotFound`.
    async fn link_identity(&self, identity: &ExternalIdentity, user: UserId) -> Result<(), OrgStoreError>;
    /// Insert or change the role. Demoting the last `Owner` → `Conflict`.
    async fn put_membership(&self, m: &Membership) -> Result<(), OrgStoreError>;
    /// `Conflict` if it is the last `Owner`; `NotFound` if not a member.
    async fn remove_membership(&self, org: OrgId, user: UserId) -> Result<(), OrgStoreError>;
    async fn membership(&self, org: OrgId, user: UserId) -> Result<Option<Membership>, OrgStoreError>;
    /// Ordered by user id.
    async fn members(&self, org: OrgId, page: PageRequest) -> Result<Page<(User, Membership)>, OrgStoreError>;
    /// Platform. Ordered by org slug.
    async fn orgs_for_user(&self, user: UserId) -> Result<Vec<(Org, Membership)>, OrgStoreError>;
    /// `Conflict` if an unexpired invite exists for `(org, provider, subject)`; an expired one is
    /// replaced.
    async fn create_invite(&self, invite: &Invite) -> Result<(), OrgStoreError>;
    /// Unexpired invites, oldest first.
    async fn invites(&self, org: OrgId) -> Result<Vec<Invite>, OrgStoreError>;
    async fn withdraw_invite(&self, org: OrgId, id: InviteId) -> Result<(), OrgStoreError>;
    /// Platform, one transaction: every unexpired invite for the subject becomes a membership (an
    /// existing membership keeps its role) and is deleted. Returns the memberships of those orgs.
    async fn accept_invites(&self, provider: &str, subject: &str, user: UserId) -> Result<Vec<Membership>, OrgStoreError>;

    // integrations
    /// Insert, or update the status of `(org, kind, external_id)`; settings are left untouched.
    /// Active in another org → `Conflict`.
    async fn upsert_integration(&self, integration: &Integration, jobs: &[NewJob]) -> Result<(), OrgStoreError>;
    /// Ordered by (kind, external id).
    async fn integrations(&self, org: OrgId) -> Result<Vec<Integration>, OrgStoreError>;
    async fn set_integration_status(
        &self,
        org: OrgId,
        kind: IntegrationKind,
        external_id: &str,
        status: IntegrationStatus,
        jobs: &[NewJob],
    ) -> Result<(), OrgStoreError>;
    /// Platform, cross-tenant: webhook tenant resolution only. The `Active` row if one exists, else
    /// the most recently updated `Revoked` one (intake.md tells "unbound" from "revoked" and
    /// `installation.unsuspend` reactivates it), else `None`.
    async fn integration_by_external(&self, kind: IntegrationKind, external_id: &str) -> Result<Option<Integration>, OrgStoreError>;
    /// `NotFound` if the integration does not exist.
    async fn integration_settings(&self, org: OrgId, kind: IntegrationKind, external_id: &str) -> Result<IntegrationSettings, OrgStoreError>;
    async fn set_integration_settings(
        &self,
        org: OrgId,
        kind: IntegrationKind,
        external_id: &str,
        s: &IntegrationSettings,
    ) -> Result<(), OrgStoreError>;
    /// `Default` when no row exists.
    async fn intake_config(&self, org: OrgId) -> Result<IntakeConfig, OrgStoreError>;
    async fn put_intake_config(&self, org: OrgId, config: &IntakeConfig) -> Result<(), OrgStoreError>;

    // customers
    /// Insert or update; replaces the refs in the same transaction (stored and returned as
    /// [`normalize_refs`]). A ref naming another customer → `Conflict`.
    async fn put_customer(&self, customer: &Customer) -> Result<(), OrgStoreError>;
    async fn get_customer(&self, org: OrgId, id: CustomerId) -> Result<Customer, OrgStoreError>;
    /// Customers named by any of `refs`, ordered by id.
    async fn find_customers_by_refs(&self, org: OrgId, refs: &[ExternalRef]) -> Result<Vec<Customer>, OrgStoreError>;
    /// Ordered by id.
    async fn list_customers(&self, org: OrgId, page: PageRequest) -> Result<Page<Customer>, OrgStoreError>;
}

impl From<OrgStoreError> for AppError {
    fn from(err: OrgStoreError) -> AppError {
        match err {
            OrgStoreError::NotFound => AppError::NotFound("org data".into()),
            OrgStoreError::Conflict => AppError::Conflict("org data changed or already exists".into()),
            OrgStoreError::Invalid(msg) => AppError::Invalid(msg),
            OrgStoreError::Unavailable(msg) => AppError::Unavailable(msg),
        }
    }
}

/// `integrations.kind` / `oauth_states.provider` value (the serde names).
pub const fn integration_kind_str(kind: IntegrationKind) -> &'static str {
    match kind {
        IntegrationKind::GitHub => "github",
        IntegrationKind::GitHubFix => "github_fix",
        IntegrationKind::Linear => "linear",
        IntegrationKind::Slack => "slack",
    }
}

pub fn parse_integration_kind(s: &str) -> Option<IntegrationKind> {
    [IntegrationKind::GitHub, IntegrationKind::GitHubFix, IntegrationKind::Linear, IntegrationKind::Slack]
        .into_iter()
        .find(|k| integration_kind_str(*k) == s)
}

pub const fn integration_status_str(status: IntegrationStatus) -> &'static str {
    match status {
        IntegrationStatus::Active => "active",
        IntegrationStatus::Revoked => "revoked",
    }
}

pub fn parse_integration_status(s: &str) -> Option<IntegrationStatus> {
    [IntegrationStatus::Active, IntegrationStatus::Revoked].into_iter().find(|v| integration_status_str(*v) == s)
}

/// `memberships.role` / `invites.role` value.
pub const fn member_role_str(role: MemberRole) -> &'static str {
    match role {
        MemberRole::Owner => "owner",
        MemberRole::Admin => "admin",
        MemberRole::Member => "member",
    }
}

pub fn parse_member_role(s: &str) -> Option<MemberRole> {
    [MemberRole::Owner, MemberRole::Admin, MemberRole::Member].into_iter().find(|r| member_role_str(*r) == s)
}

/// Feed subject of an integration: `{kind}/{external_id}`.
pub fn integration_subject(kind: IntegrationKind, external_id: &str) -> String {
    format!("{}/{external_id}", integration_kind_str(kind))
}

/// `customer_refs (system, value)`; Slack channels are `"{team}/{channel}"`.
pub fn external_ref_parts(r: &ExternalRef) -> (&'static str, String) {
    match r {
        ExternalRef::LinearCustomer(id) => ("linear_customer", id.clone()),
        ExternalRef::LinearLabel(name) => ("linear_label", name.clone()),
        ExternalRef::SlackChannel { team, channel } => ("slack_channel", format!("{team}/{channel}")),
    }
}

pub fn external_ref_from_parts(system: &str, value: &str) -> Option<ExternalRef> {
    match system {
        "linear_customer" => Some(ExternalRef::LinearCustomer(value.to_owned())),
        "linear_label" => Some(ExternalRef::LinearLabel(value.to_owned())),
        "slack_channel" => {
            let (team, channel) = value.split_once('/')?;
            Some(ExternalRef::SlackChannel { team: team.to_owned(), channel: channel.to_owned() })
        }
        _ => None,
    }
}

/// A customer's refs as stored and returned: sorted by `(system, value)`, duplicates dropped.
pub fn normalize_refs(refs: &[ExternalRef]) -> Vec<ExternalRef> {
    let mut keyed: Vec<((&'static str, String), ExternalRef)> = refs.iter().map(|r| (external_ref_parts(r), r.clone())).collect();
    keyed.sort_by(|a, b| a.0.cmp(&b.0));
    keyed.dedup_by(|a, b| a.0 == b.0);
    keyed.into_iter().map(|(_, r)| r).collect()
}

/// `users.email` for an identity: the provider's email, else GitHub's noreply address
/// (`{id}+{login}@users.noreply.github.com`). Other providers without an email → `Invalid`.
pub fn identity_email(identity: &ExternalIdentity) -> Result<String, OrgStoreError> {
    use crate::ports::auth::IdentityProviderKind;
    match (&identity.email, identity.provider) {
        (Some(email), _) if !email.trim().is_empty() => Ok(email.trim().to_owned()),
        (_, IdentityProviderKind::GitHub) => Ok(format!("{}+{}@users.noreply.github.com", identity.subject, identity.login)),
        _ => Err(OrgStoreError::Invalid(format!("{} identity has no email", identity.provider.as_str()))),
    }
}

/// `users.name` for an identity: its display name, else the login.
pub fn identity_name(identity: &ExternalIdentity) -> String {
    match &identity.name {
        Some(name) if !name.trim().is_empty() => name.trim().to_owned(),
        _ => identity.login.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retention_bounds() {
        assert_eq!(RetentionPolicy::default().validate(), Ok(()));
        assert!(RetentionPolicy { run_artifacts_days: 6, ..RetentionPolicy::default() }.validate().is_err());
        assert!(RetentionPolicy { run_events_days: 731, ..RetentionPolicy::default() }.validate().is_err());
        assert_eq!(RetentionPolicy { run_artifacts_days: 7, run_events_days: 730, keep_referenced_artifacts: false }.validate(), Ok(()));
    }
}
