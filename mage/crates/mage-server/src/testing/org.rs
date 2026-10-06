//! `MemOrgStore`: in-memory `OrgStore`. Outbox jobs go to a shared `MemJobQueue` and feed entries to
//! a shared [`MemChangeFeed`] inside the same lock scope as the write, after validation, so a call
//! and its side effects land together or not at all. Settings, intake config and integration
//! settings are kept as JSON values (the jsonb round trip). Tenant rows need no org row (the pg
//! adapter's foreign keys are not modelled).

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use mage_core::domain::fix::FixSettings;
use mage_core::domain::{
    Customer, CustomerId, ExternalRef, Integration, IntegrationKind, IntegrationStatus, MemberRole, Membership, Org, OrgId, User, UserId,
};
use mage_core::ports::clock::Clock;
use mage_core::ports::common::{Page, PageRequest};
use mage_core::ports::jobs::NewJob;
use mage_core::testing::MemJobQueue;
use mage_core::wire::api::{GeneralSettings, TriageSettings};
use parking_lot::Mutex;
use serde::Serialize;
use serde::de::DeserializeOwned;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::domain::intake::{IntakeConfig, IntegrationSettings};
use crate::ports::auth::ExternalIdentity;
use crate::ports::feed::FeedKind;
use crate::ports::llm::ModelSettings;
use crate::ports::org::{
    Invite, InviteId, OrgState, OrgStore, OrgStoreError, RetentionPolicy, SettingsArea, external_ref_parts, identity_email, identity_name,
    integration_kind_str, integration_subject, normalize_refs,
};
use crate::testing::feed::MemChangeFeed;
use crate::testing::page::{keyset_page, uuid_after};

pub struct MemOrgStore {
    clock: Arc<dyn Clock>,
    jobs: Arc<MemJobQueue>,
    feed: Arc<MemChangeFeed>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    orgs: BTreeMap<OrgId, OrgRow>,
    users: BTreeMap<UserId, User>,
    /// `(provider, subject)` → identity.
    identities: BTreeMap<(String, String), IdentityRow>,
    memberships: BTreeMap<(OrgId, UserId), MemberRole>,
    invites: BTreeMap<(OrgId, InviteId), Invite>,
    settings: BTreeMap<(OrgId, &'static str), serde_json::Value>,
    /// `(org, kind, external_id)` → (status, settings).
    integrations: BTreeMap<(OrgId, &'static str, String), IntegrationRow>,
    intake: BTreeMap<OrgId, serde_json::Value>,
    customers: BTreeMap<(OrgId, CustomerId), Customer>,
}

struct OrgRow {
    org: Org,
    state: OrgState,
    retention: RetentionPolicy,
    deletion_requested_at: Option<OffsetDateTime>,
}

struct IdentityRow {
    user: UserId,
    login: String,
    avatar_url: Option<String>,
}

struct IntegrationRow {
    integration: Integration,
    settings: serde_json::Value,
    /// Clock time of the insert or the last status / settings write (pg `integrations.updated_at`).
    updated_at: OffsetDateTime,
}

fn invalid_jobs(jobs: &[NewJob]) -> Result<(), OrgStoreError> {
    MemJobQueue::validate_all(jobs).map_err(|e| OrgStoreError::Invalid(e.to_string()))
}

fn to_json<T: Serialize>(value: &T) -> Result<serde_json::Value, OrgStoreError> {
    serde_json::to_value(value).map_err(|e| OrgStoreError::Invalid(e.to_string()))
}

fn from_json<T: DeserializeOwned + Default>(value: Option<&serde_json::Value>) -> Result<T, OrgStoreError> {
    match value {
        None => Ok(T::default()),
        Some(serde_json::Value::Object(map)) if map.is_empty() => Ok(T::default()),
        Some(value) => serde_json::from_value(value.clone()).map_err(|e| OrgStoreError::Invalid(format!("stored settings do not decode: {e}"))),
    }
}

fn bad_cursor(msg: String) -> OrgStoreError {
    OrgStoreError::Invalid(msg)
}

impl State {
    fn owners(&self, org: OrgId) -> usize {
        self.memberships.iter().filter(|((o, _), role)| *o == org && **role == MemberRole::Owner).count()
    }

    fn email_taken(&self, email: &str, except: Option<UserId>) -> bool {
        self.users.values().any(|u| Some(u.id) != except && u.email.eq_ignore_ascii_case(email))
    }

    /// An active integration of `(kind, external_id)` owned by another org.
    fn active_elsewhere(&self, org: OrgId, kind: &str, external_id: &str) -> bool {
        self.integrations.iter().any(|((o, k, e), row)| {
            *o != org && *k == kind && e == external_id && row.integration.status == IntegrationStatus::Active
        })
    }
}

impl MemOrgStore {
    pub fn new(clock: Arc<dyn Clock>, jobs: Arc<MemJobQueue>, feed: Arc<MemChangeFeed>) -> Self {
        Self { clock, jobs, feed, state: Mutex::new(State::default()) }
    }

    fn get_settings<T: DeserializeOwned + Default>(&self, org: OrgId, area: SettingsArea) -> Result<T, OrgStoreError> {
        from_json(self.state.lock().settings.get(&(org, area.as_str())))
    }

    fn put_settings<T: Serialize>(&self, org: OrgId, area: SettingsArea, value: &T) -> Result<(), OrgStoreError> {
        let value = to_json(value)?;
        let mut state = self.state.lock();
        state.settings.insert((org, area.as_str()), value);
        self.feed.push(org, FeedKind::Settings, org.to_string());
        Ok(())
    }

    fn set_status(&self, state: &mut State, org: OrgId, kind: IntegrationKind, external_id: &str, status: IntegrationStatus) -> Result<(), OrgStoreError> {
        let kind_str = integration_kind_str(kind);
        let key = (org, kind_str, external_id.to_owned());
        if !state.integrations.contains_key(&key) {
            return Err(OrgStoreError::NotFound);
        }
        if status == IntegrationStatus::Active && state.active_elsewhere(org, kind_str, external_id) {
            return Err(OrgStoreError::Conflict);
        }
        let row = state.integrations.get_mut(&key).ok_or(OrgStoreError::NotFound)?;
        row.integration.status = status;
        row.updated_at = self.clock.now();
        Ok(())
    }
}

#[async_trait]
impl OrgStore for MemOrgStore {
    async fn create_org(&self, org: &Org, owner: UserId, retention: &RetentionPolicy) -> Result<(), OrgStoreError> {
        retention.validate().map_err(OrgStoreError::Invalid)?;
        let mut state = self.state.lock();
        if state.orgs.contains_key(&org.id) || state.orgs.values().any(|r| r.org.slug == org.slug) {
            return Err(OrgStoreError::Conflict);
        }
        state.orgs.insert(org.id, OrgRow { org: org.clone(), state: OrgState::Active, retention: *retention, deletion_requested_at: None });
        state.memberships.insert((org.id, owner), MemberRole::Owner);
        Ok(())
    }

    async fn get_org(&self, org: OrgId) -> Result<Org, OrgStoreError> {
        self.state.lock().orgs.get(&org).map(|r| r.org.clone()).ok_or(OrgStoreError::NotFound)
    }

    async fn org_by_slug(&self, slug: &str) -> Result<Option<Org>, OrgStoreError> {
        Ok(self.state.lock().orgs.values().find(|r| r.org.slug == slug).map(|r| r.org.clone()))
    }

    async fn org_state(&self, org: OrgId) -> Result<OrgState, OrgStoreError> {
        self.state.lock().orgs.get(&org).map(|r| r.state).ok_or(OrgStoreError::NotFound)
    }

    async fn set_org_state(&self, org: OrgId, expect: OrgState, next: OrgState, jobs: &[NewJob]) -> Result<(), OrgStoreError> {
        invalid_jobs(jobs)?;
        let now = self.clock.now();
        let mut state = self.state.lock();
        let row = state.orgs.get_mut(&org).ok_or(OrgStoreError::NotFound)?;
        if row.state != expect {
            return Err(OrgStoreError::Conflict);
        }
        row.state = next;
        row.deletion_requested_at = match next {
            OrgState::Deleting => Some(now),
            OrgState::Active => None,
        };
        self.jobs.insert_validated(jobs);
        Ok(())
    }

    async fn retention(&self, org: OrgId) -> Result<RetentionPolicy, OrgStoreError> {
        self.state.lock().orgs.get(&org).map(|r| r.retention).ok_or(OrgStoreError::NotFound)
    }

    async fn set_retention(&self, org: OrgId, policy: &RetentionPolicy) -> Result<(), OrgStoreError> {
        policy.validate().map_err(OrgStoreError::Invalid)?;
        let mut state = self.state.lock();
        state.orgs.get_mut(&org).ok_or(OrgStoreError::NotFound)?.retention = *policy;
        self.feed.push(org, FeedKind::Settings, org.to_string());
        Ok(())
    }

    async fn triage_settings(&self, org: OrgId) -> Result<TriageSettings, OrgStoreError> {
        self.get_settings(org, SettingsArea::Triage)
    }

    async fn set_triage_settings(&self, org: OrgId, s: &TriageSettings) -> Result<(), OrgStoreError> {
        self.put_settings(org, SettingsArea::Triage, s)
    }

    async fn fix_settings(&self, org: OrgId) -> Result<FixSettings, OrgStoreError> {
        self.get_settings(org, SettingsArea::Fix)
    }

    async fn set_fix_settings(&self, org: OrgId, s: &FixSettings) -> Result<(), OrgStoreError> {
        self.put_settings(org, SettingsArea::Fix, s)
    }

    async fn model_settings(&self, org: OrgId) -> Result<ModelSettings, OrgStoreError> {
        self.get_settings(org, SettingsArea::Model)
    }

    async fn set_model_settings(&self, org: OrgId, s: &ModelSettings) -> Result<(), OrgStoreError> {
        self.put_settings(org, SettingsArea::Model, s)
    }

    async fn general_settings(&self, org: OrgId) -> Result<GeneralSettings, OrgStoreError> {
        self.get_settings(org, SettingsArea::General)
    }

    async fn set_general_settings(&self, org: OrgId, s: &GeneralSettings) -> Result<(), OrgStoreError> {
        self.put_settings(org, SettingsArea::General, s)
    }

    async fn upsert_user_by_identity(&self, identity: &ExternalIdentity, new_id: UserId) -> Result<User, OrgStoreError> {
        let key = (identity.provider.as_str().to_owned(), identity.subject.clone());
        let name = identity_name(identity);
        let mut state = self.state.lock();
        if let Some(existing) = state.identities.get(&key).map(|row| row.user) {
            let email = identity.email.as_deref().map(str::trim).filter(|e| !e.is_empty()).map(str::to_owned);
            if let Some(email) = &email {
                if state.email_taken(email, Some(existing)) {
                    return Err(OrgStoreError::Conflict);
                }
            }
            let row = state.identities.get_mut(&key).ok_or(OrgStoreError::NotFound)?;
            row.login = identity.login.clone();
            row.avatar_url = identity.avatar_url.clone();
            let user = state.users.get_mut(&existing).ok_or(OrgStoreError::NotFound)?;
            user.name = name;
            if let Some(email) = email {
                user.email = email;
            }
            return Ok(user.clone());
        }
        let email = identity_email(identity)?;
        if state.users.contains_key(&new_id) || state.email_taken(&email, None) {
            return Err(OrgStoreError::Conflict);
        }
        let user = User { id: new_id, email, name };
        state.users.insert(new_id, user.clone());
        state.identities.insert(key, IdentityRow { user: new_id, login: identity.login.clone(), avatar_url: identity.avatar_url.clone() });
        Ok(user)
    }

    async fn get_user(&self, id: UserId) -> Result<User, OrgStoreError> {
        self.state.lock().users.get(&id).cloned().ok_or(OrgStoreError::NotFound)
    }

    async fn link_identity(&self, identity: &ExternalIdentity, user: UserId) -> Result<(), OrgStoreError> {
        let key = (identity.provider.as_str().to_owned(), identity.subject.clone());
        let mut state = self.state.lock();
        if !state.users.contains_key(&user) {
            return Err(OrgStoreError::NotFound);
        }
        match state.identities.get_mut(&key) {
            Some(row) if row.user != user => Err(OrgStoreError::Conflict),
            Some(row) => {
                row.login = identity.login.clone();
                row.avatar_url = identity.avatar_url.clone();
                Ok(())
            }
            None => {
                state.identities.insert(key, IdentityRow { user, login: identity.login.clone(), avatar_url: identity.avatar_url.clone() });
                Ok(())
            }
        }
    }

    async fn put_membership(&self, m: &Membership) -> Result<(), OrgStoreError> {
        let mut state = self.state.lock();
        let current = state.memberships.get(&(m.org_id, m.user_id)).copied();
        if current == Some(MemberRole::Owner) && m.role != MemberRole::Owner && state.owners(m.org_id) == 1 {
            return Err(OrgStoreError::Conflict);
        }
        state.memberships.insert((m.org_id, m.user_id), m.role);
        self.feed.push(m.org_id, FeedKind::Member, m.user_id.to_string());
        Ok(())
    }

    async fn remove_membership(&self, org: OrgId, user: UserId) -> Result<(), OrgStoreError> {
        let mut state = self.state.lock();
        let role = state.memberships.get(&(org, user)).copied().ok_or(OrgStoreError::NotFound)?;
        if role == MemberRole::Owner && state.owners(org) == 1 {
            return Err(OrgStoreError::Conflict);
        }
        state.memberships.remove(&(org, user));
        self.feed.push(org, FeedKind::Member, user.to_string());
        Ok(())
    }

    async fn membership(&self, org: OrgId, user: UserId) -> Result<Option<Membership>, OrgStoreError> {
        Ok(self.state.lock().memberships.get(&(org, user)).map(|role| Membership { org_id: org, user_id: user, role: *role }))
    }

    async fn members(&self, org: OrgId, page: PageRequest) -> Result<Page<(User, Membership)>, OrgStoreError> {
        let after = uuid_after(&page).map_err(bad_cursor)?;
        let state = self.state.lock();
        let items = state
            .memberships
            .iter()
            .filter(|((o, _), _)| *o == org)
            .filter_map(|((_, user), role)| {
                let user = state.users.get(user)?;
                Some((user.id.as_uuid(), (user.clone(), Membership { org_id: org, user_id: user.id, role: *role })))
            })
            .collect();
        Ok(keyset_page(items, after.as_ref(), &page, |k: &Uuid| k.to_string()))
    }

    async fn orgs_for_user(&self, user: UserId) -> Result<Vec<(Org, Membership)>, OrgStoreError> {
        let state = self.state.lock();
        let mut out: Vec<(Org, Membership)> = state
            .memberships
            .iter()
            .filter(|((_, u), _)| *u == user)
            .filter_map(|((org, _), role)| {
                let row = state.orgs.get(org)?;
                Some((row.org.clone(), Membership { org_id: *org, user_id: user, role: *role }))
            })
            .collect();
        out.sort_by(|a, b| a.0.slug.cmp(&b.0.slug));
        Ok(out)
    }

    async fn create_invite(&self, invite: &Invite) -> Result<(), OrgStoreError> {
        let now = self.clock.now();
        let mut state = self.state.lock();
        let same_subject = |i: &Invite| i.org_id == invite.org_id && i.provider == invite.provider && i.subject == invite.subject;
        if state.invites.values().any(|i| same_subject(i) && i.expires_at > now) || state.invites.contains_key(&(invite.org_id, invite.id)) {
            return Err(OrgStoreError::Conflict);
        }
        state.invites.retain(|_, i| !same_subject(i));
        state.invites.insert((invite.org_id, invite.id), invite.clone());
        self.feed.push(invite.org_id, FeedKind::Member, invite.id.to_string());
        Ok(())
    }

    async fn invites(&self, org: OrgId) -> Result<Vec<Invite>, OrgStoreError> {
        let now = self.clock.now();
        let state = self.state.lock();
        let mut out: Vec<Invite> = state.invites.values().filter(|i| i.org_id == org && i.expires_at > now).cloned().collect();
        out.sort_by_key(|i| (i.created_at, i.id));
        Ok(out)
    }

    async fn withdraw_invite(&self, org: OrgId, id: InviteId) -> Result<(), OrgStoreError> {
        let mut state = self.state.lock();
        state.invites.remove(&(org, id)).ok_or(OrgStoreError::NotFound)?;
        self.feed.push(org, FeedKind::Member, id.to_string());
        Ok(())
    }

    async fn accept_invites(&self, provider: &str, subject: &str, user: UserId) -> Result<Vec<Membership>, OrgStoreError> {
        let now = self.clock.now();
        let mut state = self.state.lock();
        let accepted: Vec<Invite> =
            state.invites.values().filter(|i| i.provider == provider && i.subject == subject && i.expires_at > now).cloned().collect();
        let mut out = Vec::with_capacity(accepted.len());
        for invite in accepted {
            state.invites.remove(&(invite.org_id, invite.id));
            let role = *state.memberships.entry((invite.org_id, user)).or_insert(invite.role);
            out.push(Membership { org_id: invite.org_id, user_id: user, role });
            self.feed.push(invite.org_id, FeedKind::Member, user.to_string());
        }
        out.sort_by_key(|m| m.org_id);
        Ok(out)
    }

    async fn upsert_integration(&self, integration: &Integration, jobs: &[NewJob]) -> Result<(), OrgStoreError> {
        invalid_jobs(jobs)?;
        let now = self.clock.now();
        let kind = integration_kind_str(integration.kind);
        let mut state = self.state.lock();
        if integration.status == IntegrationStatus::Active && state.active_elsewhere(integration.org_id, kind, &integration.external_id) {
            return Err(OrgStoreError::Conflict);
        }
        let key = (integration.org_id, kind, integration.external_id.clone());
        match state.integrations.get_mut(&key) {
            Some(row) => {
                row.integration.status = integration.status;
                row.updated_at = now;
            }
            None => {
                let row = IntegrationRow { integration: integration.clone(), settings: serde_json::json!({}), updated_at: now };
                state.integrations.insert(key, row);
            }
        }
        self.jobs.insert_validated(jobs);
        self.feed.push(integration.org_id, FeedKind::Integration, integration_subject(integration.kind, &integration.external_id));
        Ok(())
    }

    async fn integrations(&self, org: OrgId) -> Result<Vec<Integration>, OrgStoreError> {
        let state = self.state.lock();
        Ok(state.integrations.iter().filter(|((o, _, _), _)| *o == org).map(|(_, row)| row.integration.clone()).collect())
    }

    async fn set_integration_status(
        &self,
        org: OrgId,
        kind: IntegrationKind,
        external_id: &str,
        status: IntegrationStatus,
        jobs: &[NewJob],
    ) -> Result<(), OrgStoreError> {
        invalid_jobs(jobs)?;
        let mut state = self.state.lock();
        self.set_status(&mut state, org, kind, external_id, status)?;
        self.jobs.insert_validated(jobs);
        self.feed.push(org, FeedKind::Integration, integration_subject(kind, external_id));
        Ok(())
    }

    async fn integration_by_external(&self, kind: IntegrationKind, external_id: &str) -> Result<Option<Integration>, OrgStoreError> {
        let kind = integration_kind_str(kind);
        let state = self.state.lock();
        // Active first, then the latest update; ties by org id (the pg ORDER BY).
        Ok(state
            .integrations
            .iter()
            .filter(|((_, k, e), _)| *k == kind && e == external_id)
            .min_by_key(|((org, _, _), row)| {
                (row.integration.status != IntegrationStatus::Active, std::cmp::Reverse(row.updated_at), *org)
            })
            .map(|(_, row)| row.integration.clone()))
    }

    async fn integration_settings(&self, org: OrgId, kind: IntegrationKind, external_id: &str) -> Result<IntegrationSettings, OrgStoreError> {
        let state = self.state.lock();
        let row = state.integrations.get(&(org, integration_kind_str(kind), external_id.to_owned())).ok_or(OrgStoreError::NotFound)?;
        from_json(Some(&row.settings))
    }

    async fn set_integration_settings(
        &self,
        org: OrgId,
        kind: IntegrationKind,
        external_id: &str,
        s: &IntegrationSettings,
    ) -> Result<(), OrgStoreError> {
        let value = to_json(s)?;
        let mut state = self.state.lock();
        let row = state.integrations.get_mut(&(org, integration_kind_str(kind), external_id.to_owned())).ok_or(OrgStoreError::NotFound)?;
        row.settings = value;
        row.updated_at = self.clock.now();
        self.feed.push(org, FeedKind::Integration, integration_subject(kind, external_id));
        Ok(())
    }

    async fn intake_config(&self, org: OrgId) -> Result<IntakeConfig, OrgStoreError> {
        from_json(self.state.lock().intake.get(&org))
    }

    async fn put_intake_config(&self, org: OrgId, config: &IntakeConfig) -> Result<(), OrgStoreError> {
        let value = to_json(config)?;
        let mut state = self.state.lock();
        state.intake.insert(org, value);
        self.feed.push(org, FeedKind::Settings, org.to_string());
        Ok(())
    }

    async fn put_customer(&self, customer: &Customer) -> Result<(), OrgStoreError> {
        let refs = normalize_refs(&customer.external_refs);
        let mut state = self.state.lock();
        let taken = state.customers.values().any(|c| {
            c.org_id == customer.org_id
                && c.id != customer.id
                && c.external_refs.iter().any(|r| refs.iter().any(|n| external_ref_parts(n) == external_ref_parts(r)))
        });
        if taken {
            return Err(OrgStoreError::Conflict);
        }
        state.customers.insert((customer.org_id, customer.id), Customer { external_refs: refs, ..customer.clone() });
        Ok(())
    }

    async fn get_customer(&self, org: OrgId, id: CustomerId) -> Result<Customer, OrgStoreError> {
        self.state.lock().customers.get(&(org, id)).cloned().ok_or(OrgStoreError::NotFound)
    }

    async fn find_customers_by_refs(&self, org: OrgId, refs: &[ExternalRef]) -> Result<Vec<Customer>, OrgStoreError> {
        let wanted: Vec<(&'static str, String)> = refs.iter().map(external_ref_parts).collect();
        let state = self.state.lock();
        Ok(state
            .customers
            .values()
            .filter(|c| c.org_id == org && c.external_refs.iter().any(|r| wanted.contains(&external_ref_parts(r))))
            .cloned()
            .collect())
    }

    async fn list_customers(&self, org: OrgId, page: PageRequest) -> Result<Page<Customer>, OrgStoreError> {
        let after = uuid_after(&page).map_err(bad_cursor)?;
        let state = self.state.lock();
        let items = state.customers.values().filter(|c| c.org_id == org).map(|c| (c.id.as_uuid(), c.clone())).collect();
        Ok(keyset_page(items, after.as_ref(), &page, |k: &Uuid| k.to_string()))
    }
}
