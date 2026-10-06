//! `PgOrgStore` (storage.md "`OrgStore`", schema "Core: orgs, users, settings, integrations,
//! customers"). Tenant methods run on the tenant pool inside `begin_org`; the platform methods of
//! storage.md "Tenancy enforcement" (`create_org`, `org_by_slug`, `integration_by_external`,
//! `upsert_user_by_identity`, `get_user`, `link_identity`, `orgs_for_user`, `accept_invites`) on
//! the platform pool. Writes the UI shows append their `org_feed` row in the same transaction,
//! exactly where `MemOrgStore` pushes one.
//!
//! - Settings / intake config / integration settings are jsonb; a missing row or `{}` reads as
//!   `Default`. `org_settings.updated_by` is `Actor::System` (the port carries no actor).
//! - Last-Owner checks lock the org's owner membership rows `FOR UPDATE` (ordered by user id), so
//!   concurrent demotions / removals serialize and cannot leave an org without an Owner.
//! - "Active in another org" is the partial unique index `integrations_active`, which applies
//!   across tenants even though RLS hides the other org's row.
//! - Customer refs are replaced (delete + insert) in the customer's transaction; the primary key
//!   `(org_id, system, value)` makes a ref held by another customer a `Conflict`.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use mage_core::domain::fix::FixSettings;
use mage_core::domain::{
    Customer, CustomerId, ExternalRef, Integration, IntegrationKind, IntegrationStatus, MemberRole, Membership, Org, OrgId, User, UserId,
};
use mage_core::ports::clock::Clock;
use mage_core::ports::common::{Page, PageRequest};
use mage_core::ports::ids::IdGen;
use mage_core::ports::jobs::NewJob;
use mage_core::wire::api::{GeneralSettings, TriageSettings};
use mage_infra::pg::jobs::enqueue_all_in;
use mage_infra::pg::{DbError, PgPools, TxError, begin_org, begin_platform, retry_tx};
use serde::Serialize;
use serde::de::DeserializeOwned;
use sqlx::PgConnection;
use sqlx::types::Json;
use time::OffsetDateTime;
use uuid::Uuid;

use super::codec::{finish_page, page_limit, parse_enum, uuid_cursor};
use super::feed::append_in;
use crate::domain::intake::{IntakeConfig, IntegrationSettings};
use crate::ports::audit::Actor;
use crate::ports::auth::ExternalIdentity;
use crate::ports::feed::FeedKind;
use crate::ports::llm::ModelSettings;
use crate::ports::org::{
    Invite, InviteId, OrgState, OrgStore, OrgStoreError, RetentionPolicy, SettingsArea, external_ref_from_parts, external_ref_parts,
    identity_email, identity_name, integration_kind_str, integration_status_str, integration_subject, member_role_str, normalize_refs,
    parse_integration_kind, parse_integration_status, parse_member_role,
};

type TxResult<T> = Result<T, TxError<OrgStoreError>>;

impl From<DbError> for OrgStoreError {
    fn from(err: DbError) -> Self {
        match err {
            DbError::NotFound => OrgStoreError::NotFound,
            DbError::Conflict { .. } => OrgStoreError::Conflict,
            DbError::Invalid(msg) => OrgStoreError::Invalid(msg),
            DbError::Unavailable(msg) | DbError::Retry(msg) => OrgStoreError::Unavailable(msg),
            DbError::Timeout => OrgStoreError::Unavailable("statement timeout".into()),
        }
    }
}

pub struct PgOrgStore {
    pools: PgPools,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGen>,
}

impl PgOrgStore {
    pub fn new(pools: PgPools, clock: Arc<dyn Clock>, ids: Arc<dyn IdGen>) -> Self {
        Self { pools, clock, ids }
    }

    async fn get_settings<T: DeserializeOwned + Default>(&self, org: OrgId, area: SettingsArea) -> Result<T, OrgStoreError> {
        let value = retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let value: Option<Json<serde_json::Value>> = sqlx::query_scalar("SELECT value FROM org_settings WHERE org_id = $1 AND area = $2")
                .bind(org.as_uuid())
                .bind(area.as_str())
                .fetch_optional(&mut *tx)
                .await?;
            tx.commit().await?;
            Ok::<_, TxError<OrgStoreError>>(value.map(|v| v.0))
        })
        .await?;
        decode_json(value, "org_settings.value")
    }

    async fn put_settings<T: Serialize>(&self, org: OrgId, area: SettingsArea, value: &T) -> Result<(), OrgStoreError> {
        let value = to_json(value)?;
        let now = self.clock.now();
        let by = Actor::System.encode();
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            sqlx::query(
                "INSERT INTO org_settings (org_id, area, value, updated_at, updated_by) VALUES ($1, $2, $3, $4, $5) \
                 ON CONFLICT (org_id, area) DO UPDATE \
                 SET value = EXCLUDED.value, updated_at = EXCLUDED.updated_at, updated_by = EXCLUDED.updated_by",
            )
            .bind(org.as_uuid())
            .bind(area.as_str())
            .bind(Json(&value))
            .bind(now)
            .bind(&by)
            .execute(&mut *tx)
            .await?;
            append_in(&mut tx, org, FeedKind::Settings, &org.to_string(), now).await?;
            tx.commit().await?;
            Ok::<_, TxError<OrgStoreError>>(())
        })
        .await
    }
}

fn invalid_jobs(jobs: &[NewJob]) -> Result<(), OrgStoreError> {
    jobs.iter().try_for_each(NewJob::validate).map_err(|e| OrgStoreError::Invalid(e.to_string()))
}

fn to_json<T: Serialize>(value: &T) -> Result<serde_json::Value, OrgStoreError> {
    serde_json::to_value(value).map_err(|e| OrgStoreError::Invalid(e.to_string()))
}

/// A missing row or `{}` is `Default`.
fn decode_json<T: DeserializeOwned + Default>(value: Option<serde_json::Value>, column: &str) -> Result<T, OrgStoreError> {
    match value {
        None => Ok(T::default()),
        Some(serde_json::Value::Object(map)) if map.is_empty() => Ok(T::default()),
        Some(value) => serde_json::from_value(value).map_err(|e| OrgStoreError::Invalid(format!("{column} does not decode: {e}"))),
    }
}

/// Owner user ids of `org`, locked until commit.
async fn lock_owners(conn: &mut PgConnection, org: OrgId) -> Result<Vec<Uuid>, DbError> {
    Ok(sqlx::query_scalar("SELECT user_id FROM memberships WHERE org_id = $1 AND role = 'owner' ORDER BY user_id FOR UPDATE")
        .bind(org.as_uuid())
        .fetch_all(conn)
        .await?)
}

#[derive(sqlx::FromRow)]
struct OrgRow {
    id: Uuid,
    slug: String,
    name: String,
    created_at: OffsetDateTime,
}

impl From<OrgRow> for Org {
    fn from(r: OrgRow) -> Org {
        Org { id: OrgId::from_uuid(r.id), slug: r.slug, name: r.name, created_at: r.created_at }
    }
}

#[derive(sqlx::FromRow)]
struct UserRow {
    id: Uuid,
    email: String,
    name: String,
}

impl From<UserRow> for User {
    fn from(r: UserRow) -> User {
        User { id: UserId::from_uuid(r.id), email: r.email, name: r.name }
    }
}

#[derive(sqlx::FromRow)]
struct MemberRow {
    id: Uuid,
    email: String,
    name: String,
    role: String,
}

#[derive(sqlx::FromRow)]
struct OrgMemberRow {
    id: Uuid,
    slug: String,
    name: String,
    created_at: OffsetDateTime,
    role: String,
}

#[derive(sqlx::FromRow)]
struct InviteRow {
    org_id: Uuid,
    id: Uuid,
    provider: String,
    subject: String,
    login: String,
    role: String,
    invited_by: Uuid,
    created_at: OffsetDateTime,
    expires_at: OffsetDateTime,
}

impl TryFrom<InviteRow> for Invite {
    type Error = DbError;

    fn try_from(r: InviteRow) -> Result<Invite, DbError> {
        Ok(Invite {
            id: InviteId::from_uuid(r.id),
            org_id: OrgId::from_uuid(r.org_id),
            provider: r.provider,
            subject: r.subject,
            login: r.login,
            role: parse_enum(&r.role, "invites.role", parse_member_role)?,
            invited_by: UserId::from_uuid(r.invited_by),
            created_at: r.created_at,
            expires_at: r.expires_at,
        })
    }
}

#[derive(sqlx::FromRow)]
struct IntegrationRow {
    org_id: Uuid,
    kind: String,
    external_id: String,
    status: String,
}

impl TryFrom<IntegrationRow> for Integration {
    type Error = DbError;

    fn try_from(r: IntegrationRow) -> Result<Integration, DbError> {
        Ok(Integration {
            org_id: OrgId::from_uuid(r.org_id),
            kind: parse_enum(&r.kind, "integrations.kind", parse_integration_kind)?,
            external_id: r.external_id,
            status: parse_enum(&r.status, "integrations.status", parse_integration_status)?,
        })
    }
}

#[derive(sqlx::FromRow)]
struct CustomerRow {
    id: Uuid,
    name: String,
    environment: Option<String>,
}

/// `rows` (ordered by id) with their refs, ordered by `(system, value)` bytewise.
async fn with_refs(conn: &mut PgConnection, org: OrgId, rows: Vec<CustomerRow>) -> Result<Vec<Customer>, DbError> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<Uuid> = rows.iter().map(|r| r.id).collect();
    let refs: Vec<(Uuid, String, String)> = sqlx::query_as(
        "SELECT customer_id, system, value FROM customer_refs WHERE org_id = $1 AND customer_id = ANY($2) \
         ORDER BY customer_id, system COLLATE \"C\", value COLLATE \"C\"",
    )
    .bind(org.as_uuid())
    .bind(&ids)
    .fetch_all(conn)
    .await?;
    let mut by_customer: BTreeMap<Uuid, Vec<ExternalRef>> = BTreeMap::new();
    for (customer, system, value) in refs {
        let r = external_ref_from_parts(&system, &value)
            .ok_or_else(|| DbError::Invalid(format!("customer_refs: undecodable ref ({system:?}, {value:?})")))?;
        by_customer.entry(customer).or_default().push(r);
    }
    Ok(rows
        .into_iter()
        .map(|r| Customer {
            id: CustomerId::from_uuid(r.id),
            org_id: org,
            name: r.name,
            external_refs: by_customer.remove(&r.id).unwrap_or_default(),
            environment: r.environment,
        })
        .collect())
}

/// `(systems, values)` arrays for `UNNEST`.
fn ref_arrays(refs: &[ExternalRef]) -> (Vec<String>, Vec<String>) {
    refs.iter().map(|r| {
        let (system, value) = external_ref_parts(r);
        (system.to_owned(), value)
    })
    .unzip()
}

#[async_trait]
impl OrgStore for PgOrgStore {
    async fn create_org(&self, org: &Org, owner: UserId, retention: &RetentionPolicy) -> Result<(), OrgStoreError> {
        retention.validate().map_err(OrgStoreError::Invalid)?;
        let now = self.clock.now();
        retry_tx(|| async {
            let mut tx = begin_platform(&self.pools.platform).await?;
            sqlx::query("INSERT INTO orgs (id, slug, name, state, retention, created_at) VALUES ($1, $2, $3, $4, $5, $6)")
                .bind(org.id.as_uuid())
                .bind(&org.slug)
                .bind(&org.name)
                .bind(OrgState::Active.as_str())
                .bind(Json(retention))
                .bind(org.created_at)
                .execute(&mut *tx)
                .await?;
            sqlx::query("INSERT INTO memberships (org_id, user_id, role, created_at) VALUES ($1, $2, $3, $4)")
                .bind(org.id.as_uuid())
                .bind(owner.as_uuid())
                .bind(member_role_str(MemberRole::Owner))
                .bind(now)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            Ok::<_, TxError<OrgStoreError>>(())
        })
        .await
    }

    async fn get_org(&self, org: OrgId) -> Result<Org, OrgStoreError> {
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let row: Option<OrgRow> = sqlx::query_as("SELECT id, slug, name, created_at FROM orgs WHERE id = $1")
                .bind(org.as_uuid())
                .fetch_optional(&mut *tx)
                .await?;
            tx.commit().await?;
            row.map(Org::from).ok_or(TxError::Port(OrgStoreError::NotFound))
        })
        .await
    }

    async fn org_by_slug(&self, slug: &str) -> Result<Option<Org>, OrgStoreError> {
        retry_tx(|| async {
            let mut tx = begin_platform(&self.pools.platform).await?;
            let row: Option<OrgRow> =
                sqlx::query_as("SELECT id, slug, name, created_at FROM orgs WHERE slug = $1").bind(slug).fetch_optional(&mut *tx).await?;
            tx.commit().await?;
            Ok::<_, TxError<OrgStoreError>>(row.map(Org::from))
        })
        .await
    }

    async fn org_state(&self, org: OrgId) -> Result<OrgState, OrgStoreError> {
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let state: Option<String> =
                sqlx::query_scalar("SELECT state FROM orgs WHERE id = $1").bind(org.as_uuid()).fetch_optional(&mut *tx).await?;
            tx.commit().await?;
            let state = state.ok_or(TxError::Port(OrgStoreError::NotFound))?;
            Ok(parse_enum(&state, "orgs.state", OrgState::parse)?)
        })
        .await
    }

    async fn set_org_state(&self, org: OrgId, expect: OrgState, next: OrgState, jobs: &[NewJob]) -> Result<(), OrgStoreError> {
        invalid_jobs(jobs)?;
        let now = self.clock.now();
        let requested_at = match next {
            OrgState::Deleting => Some(now),
            OrgState::Active => None,
        };
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let updated = sqlx::query("UPDATE orgs SET state = $3, deletion_requested_at = $4 WHERE id = $1 AND state = $2")
                .bind(org.as_uuid())
                .bind(expect.as_str())
                .bind(next.as_str())
                .bind(requested_at)
                .execute(&mut *tx)
                .await?
                .rows_affected();
            if updated == 0 {
                let exists: bool =
                    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM orgs WHERE id = $1)").bind(org.as_uuid()).fetch_one(&mut *tx).await?;
                return Err(TxError::Port(if exists { OrgStoreError::Conflict } else { OrgStoreError::NotFound }));
            }
            enqueue_all_in(&mut tx, jobs, now, &*self.ids).await?;
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    async fn retention(&self, org: OrgId) -> Result<RetentionPolicy, OrgStoreError> {
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let policy: Option<Json<RetentionPolicy>> =
                sqlx::query_scalar("SELECT retention FROM orgs WHERE id = $1").bind(org.as_uuid()).fetch_optional(&mut *tx).await?;
            tx.commit().await?;
            policy.map(|p| p.0).ok_or(TxError::Port(OrgStoreError::NotFound))
        })
        .await
    }

    async fn set_retention(&self, org: OrgId, policy: &RetentionPolicy) -> Result<(), OrgStoreError> {
        policy.validate().map_err(OrgStoreError::Invalid)?;
        let now = self.clock.now();
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let updated = sqlx::query("UPDATE orgs SET retention = $2 WHERE id = $1")
                .bind(org.as_uuid())
                .bind(Json(policy))
                .execute(&mut *tx)
                .await?
                .rows_affected();
            if updated == 0 {
                return Err(TxError::Port(OrgStoreError::NotFound));
            }
            append_in(&mut tx, org, FeedKind::Settings, &org.to_string(), now).await?;
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    async fn triage_settings(&self, org: OrgId) -> Result<TriageSettings, OrgStoreError> {
        self.get_settings(org, SettingsArea::Triage).await
    }

    async fn set_triage_settings(&self, org: OrgId, s: &TriageSettings) -> Result<(), OrgStoreError> {
        self.put_settings(org, SettingsArea::Triage, s).await
    }

    async fn fix_settings(&self, org: OrgId) -> Result<FixSettings, OrgStoreError> {
        self.get_settings(org, SettingsArea::Fix).await
    }

    async fn set_fix_settings(&self, org: OrgId, s: &FixSettings) -> Result<(), OrgStoreError> {
        self.put_settings(org, SettingsArea::Fix, s).await
    }

    async fn model_settings(&self, org: OrgId) -> Result<ModelSettings, OrgStoreError> {
        self.get_settings(org, SettingsArea::Model).await
    }

    async fn set_model_settings(&self, org: OrgId, s: &ModelSettings) -> Result<(), OrgStoreError> {
        self.put_settings(org, SettingsArea::Model, s).await
    }

    async fn general_settings(&self, org: OrgId) -> Result<GeneralSettings, OrgStoreError> {
        self.get_settings(org, SettingsArea::General).await
    }

    async fn set_general_settings(&self, org: OrgId, s: &GeneralSettings) -> Result<(), OrgStoreError> {
        self.put_settings(org, SettingsArea::General, s).await
    }

    async fn upsert_user_by_identity(&self, identity: &ExternalIdentity, new_id: UserId) -> Result<User, OrgStoreError> {
        let provider = identity.provider.as_str();
        let name = identity_name(identity);
        let refreshed_email = identity.email.as_deref().map(str::trim).filter(|e| !e.is_empty());
        // Only a new identity needs an address (other providers without one → Invalid).
        let new_email = identity_email(identity);
        let now = self.clock.now();
        retry_tx(|| async {
            let mut tx = begin_platform(&self.pools.platform).await?;
            let existing: Option<Uuid> = sqlx::query_scalar("SELECT user_id FROM user_identities WHERE provider = $1 AND subject = $2 FOR UPDATE")
                .bind(provider)
                .bind(&identity.subject)
                .fetch_optional(&mut *tx)
                .await?;
            let user: UserRow = match existing {
                Some(user_id) => {
                    let user = sqlx::query_as("UPDATE users SET name = $2, email = COALESCE($3, email) WHERE id = $1 RETURNING id, email, name")
                        .bind(user_id)
                        .bind(&name)
                        .bind(refreshed_email)
                        .fetch_one(&mut *tx)
                        .await?;
                    sqlx::query("UPDATE user_identities SET login = $3, avatar_url = $4, updated_at = $5 WHERE provider = $1 AND subject = $2")
                        .bind(provider)
                        .bind(&identity.subject)
                        .bind(&identity.login)
                        .bind(&identity.avatar_url)
                        .bind(now)
                        .execute(&mut *tx)
                        .await?;
                    user
                }
                None => {
                    let email = new_email.clone().map_err(TxError::Port)?;
                    let user = sqlx::query_as("INSERT INTO users (id, email, name, created_at) VALUES ($1, $2, $3, $4) RETURNING id, email, name")
                        .bind(new_id.as_uuid())
                        .bind(&email)
                        .bind(&name)
                        .bind(now)
                        .fetch_one(&mut *tx)
                        .await?;
                    sqlx::query(
                        "INSERT INTO user_identities (provider, subject, user_id, login, avatar_url, created_at, updated_at) \
                         VALUES ($1, $2, $3, $4, $5, $6, $6)",
                    )
                    .bind(provider)
                    .bind(&identity.subject)
                    .bind(new_id.as_uuid())
                    .bind(&identity.login)
                    .bind(&identity.avatar_url)
                    .bind(now)
                    .execute(&mut *tx)
                    .await?;
                    user
                }
            };
            tx.commit().await?;
            Ok::<_, TxError<OrgStoreError>>(User::from(user))
        })
        .await
    }

    async fn get_user(&self, id: UserId) -> Result<User, OrgStoreError> {
        retry_tx(|| async {
            let mut tx = begin_platform(&self.pools.platform).await?;
            let row: Option<UserRow> =
                sqlx::query_as("SELECT id, email, name FROM users WHERE id = $1").bind(id.as_uuid()).fetch_optional(&mut *tx).await?;
            tx.commit().await?;
            row.map(User::from).ok_or(TxError::Port(OrgStoreError::NotFound))
        })
        .await
    }

    async fn link_identity(&self, identity: &ExternalIdentity, user: UserId) -> Result<(), OrgStoreError> {
        let now = self.clock.now();
        retry_tx(|| async {
            let mut tx = begin_platform(&self.pools.platform).await?;
            let known: bool =
                sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM users WHERE id = $1)").bind(user.as_uuid()).fetch_one(&mut *tx).await?;
            if !known {
                return Err(TxError::Port(OrgStoreError::NotFound));
            }
            // Refreshes the row only when it already belongs to `user`; no row back → another user's.
            let linked: Option<Uuid> = sqlx::query_scalar(
                "INSERT INTO user_identities (provider, subject, user_id, login, avatar_url, created_at, updated_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $6) \
                 ON CONFLICT (provider, subject) DO UPDATE \
                 SET login = EXCLUDED.login, avatar_url = EXCLUDED.avatar_url, updated_at = EXCLUDED.updated_at \
                 WHERE user_identities.user_id = EXCLUDED.user_id \
                 RETURNING user_id",
            )
            .bind(identity.provider.as_str())
            .bind(&identity.subject)
            .bind(user.as_uuid())
            .bind(&identity.login)
            .bind(&identity.avatar_url)
            .bind(now)
            .fetch_optional(&mut *tx)
            .await?;
            if linked.is_none() {
                return Err(TxError::Port(OrgStoreError::Conflict));
            }
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    async fn put_membership(&self, m: &Membership) -> Result<(), OrgStoreError> {
        let now = self.clock.now();
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, m.org_id).await?;
            let owners = lock_owners(&mut tx, m.org_id).await?;
            if m.role != MemberRole::Owner && owners == [m.user_id.as_uuid()] {
                return Err(TxError::Port(OrgStoreError::Conflict));
            }
            sqlx::query(
                "INSERT INTO memberships (org_id, user_id, role, created_at) VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (org_id, user_id) DO UPDATE SET role = EXCLUDED.role",
            )
            .bind(m.org_id.as_uuid())
            .bind(m.user_id.as_uuid())
            .bind(member_role_str(m.role))
            .bind(now)
            .execute(&mut *tx)
            .await?;
            append_in(&mut tx, m.org_id, FeedKind::Member, &m.user_id.to_string(), now).await?;
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    async fn remove_membership(&self, org: OrgId, user: UserId) -> Result<(), OrgStoreError> {
        let now = self.clock.now();
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let owners = lock_owners(&mut tx, org).await?;
            let role: Option<String> = sqlx::query_scalar("SELECT role FROM memberships WHERE org_id = $1 AND user_id = $2 FOR UPDATE")
                .bind(org.as_uuid())
                .bind(user.as_uuid())
                .fetch_optional(&mut *tx)
                .await?;
            if role.is_none() {
                return Err(TxError::Port(OrgStoreError::NotFound));
            }
            if owners == [user.as_uuid()] {
                return Err(TxError::Port(OrgStoreError::Conflict));
            }
            sqlx::query("DELETE FROM memberships WHERE org_id = $1 AND user_id = $2")
                .bind(org.as_uuid())
                .bind(user.as_uuid())
                .execute(&mut *tx)
                .await?;
            append_in(&mut tx, org, FeedKind::Member, &user.to_string(), now).await?;
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    async fn membership(&self, org: OrgId, user: UserId) -> Result<Option<Membership>, OrgStoreError> {
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let role: Option<String> = sqlx::query_scalar("SELECT role FROM memberships WHERE org_id = $1 AND user_id = $2")
                .bind(org.as_uuid())
                .bind(user.as_uuid())
                .fetch_optional(&mut *tx)
                .await?;
            tx.commit().await?;
            let role = role.map(|r| parse_enum(&r, "memberships.role", parse_member_role)).transpose()?;
            Ok::<_, TxError<OrgStoreError>>(role.map(|role| Membership { org_id: org, user_id: user, role }))
        })
        .await
    }

    async fn members(&self, org: OrgId, page: PageRequest) -> Result<Page<(User, Membership)>, OrgStoreError> {
        let after = uuid_cursor(page.after.as_ref())?;
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let rows: Vec<MemberRow> = sqlx::query_as(
                "SELECT u.id, u.email, u.name, m.role FROM memberships m JOIN users u ON u.id = m.user_id \
                 WHERE m.org_id = $1 AND ($2::uuid IS NULL OR m.user_id > $2) ORDER BY m.user_id LIMIT $3",
            )
            .bind(org.as_uuid())
            .bind(after)
            .bind(page_limit(&page))
            .fetch_all(&mut *tx)
            .await?;
            tx.commit().await?;
            let items = rows
                .into_iter()
                .map(|r| {
                    let role = parse_enum(&r.role, "memberships.role", parse_member_role)?;
                    let user = UserId::from_uuid(r.id);
                    Ok((User { id: user, email: r.email, name: r.name }, Membership { org_id: org, user_id: user, role }))
                })
                .collect::<Result<Vec<_>, DbError>>()?;
            Ok::<_, TxError<OrgStoreError>>(finish_page(items, &page, |(u, _)| u.id.as_uuid().to_string()))
        })
        .await
    }

    async fn orgs_for_user(&self, user: UserId) -> Result<Vec<(Org, Membership)>, OrgStoreError> {
        retry_tx(|| async {
            let mut tx = begin_platform(&self.pools.platform).await?;
            let rows: Vec<OrgMemberRow> = sqlx::query_as(
                "SELECT o.id, o.slug, o.name, o.created_at, m.role FROM memberships m JOIN orgs o ON o.id = m.org_id \
                 WHERE m.user_id = $1 ORDER BY o.slug COLLATE \"C\"",
            )
            .bind(user.as_uuid())
            .fetch_all(&mut *tx)
            .await?;
            tx.commit().await?;
            rows.into_iter()
                .map(|r| {
                    let role = parse_enum(&r.role, "memberships.role", parse_member_role)?;
                    let org = Org { id: OrgId::from_uuid(r.id), slug: r.slug, name: r.name, created_at: r.created_at };
                    let m = Membership { org_id: org.id, user_id: user, role };
                    Ok((org, m))
                })
                .collect::<TxResult<Vec<_>>>()
        })
        .await
    }

    async fn create_invite(&self, invite: &Invite) -> Result<(), OrgStoreError> {
        let now = self.clock.now();
        let org = invite.org_id;
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let id_taken: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM invites WHERE org_id = $1 AND id = $2)")
                .bind(org.as_uuid())
                .bind(invite.id.as_uuid())
                .fetch_one(&mut *tx)
                .await?;
            let pending: Option<OffsetDateTime> =
                sqlx::query_scalar("SELECT expires_at FROM invites WHERE org_id = $1 AND provider = $2 AND subject = $3 FOR UPDATE")
                    .bind(org.as_uuid())
                    .bind(&invite.provider)
                    .bind(&invite.subject)
                    .fetch_optional(&mut *tx)
                    .await?;
            if id_taken || pending.is_some_and(|expires_at| expires_at > now) {
                return Err(TxError::Port(OrgStoreError::Conflict));
            }
            if pending.is_some() {
                sqlx::query("DELETE FROM invites WHERE org_id = $1 AND provider = $2 AND subject = $3")
                    .bind(org.as_uuid())
                    .bind(&invite.provider)
                    .bind(&invite.subject)
                    .execute(&mut *tx)
                    .await?;
            }
            sqlx::query(
                "INSERT INTO invites (org_id, id, provider, subject, login, role, invited_by, created_at, expires_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            )
            .bind(org.as_uuid())
            .bind(invite.id.as_uuid())
            .bind(&invite.provider)
            .bind(&invite.subject)
            .bind(&invite.login)
            .bind(member_role_str(invite.role))
            .bind(invite.invited_by.as_uuid())
            .bind(invite.created_at)
            .bind(invite.expires_at)
            .execute(&mut *tx)
            .await?;
            append_in(&mut tx, org, FeedKind::Member, &invite.id.to_string(), now).await?;
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    async fn invites(&self, org: OrgId) -> Result<Vec<Invite>, OrgStoreError> {
        let now = self.clock.now();
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let rows: Vec<InviteRow> = sqlx::query_as(
                "SELECT org_id, id, provider, subject, login, role, invited_by, created_at, expires_at FROM invites \
                 WHERE org_id = $1 AND expires_at > $2 ORDER BY created_at, id",
            )
            .bind(org.as_uuid())
            .bind(now)
            .fetch_all(&mut *tx)
            .await?;
            tx.commit().await?;
            rows.into_iter().map(|r| Ok(Invite::try_from(r)?)).collect::<TxResult<Vec<_>>>()
        })
        .await
    }

    async fn withdraw_invite(&self, org: OrgId, id: InviteId) -> Result<(), OrgStoreError> {
        let now = self.clock.now();
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let deleted = sqlx::query("DELETE FROM invites WHERE org_id = $1 AND id = $2")
                .bind(org.as_uuid())
                .bind(id.as_uuid())
                .execute(&mut *tx)
                .await?
                .rows_affected();
            if deleted == 0 {
                return Err(TxError::Port(OrgStoreError::NotFound));
            }
            append_in(&mut tx, org, FeedKind::Member, &id.to_string(), now).await?;
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    async fn accept_invites(&self, provider: &str, subject: &str, user: UserId) -> Result<Vec<Membership>, OrgStoreError> {
        let now = self.clock.now();
        retry_tx(|| async {
            let mut tx = begin_platform(&self.pools.platform).await?;
            let accepted: Vec<(Uuid, Uuid, String)> = sqlx::query_as(
                "SELECT org_id, id, role FROM invites WHERE provider = $1 AND subject = $2 AND expires_at > $3 ORDER BY org_id FOR UPDATE",
            )
            .bind(provider)
            .bind(subject)
            .bind(now)
            .fetch_all(&mut *tx)
            .await?;
            let mut out = Vec::with_capacity(accepted.len());
            for (org, id, role) in accepted {
                sqlx::query("DELETE FROM invites WHERE org_id = $1 AND id = $2").bind(org).bind(id).execute(&mut *tx).await?;
                sqlx::query(
                    "INSERT INTO memberships (org_id, user_id, role, created_at) VALUES ($1, $2, $3, $4) \
                     ON CONFLICT (org_id, user_id) DO NOTHING",
                )
                .bind(org)
                .bind(user.as_uuid())
                .bind(&role)
                .bind(now)
                .execute(&mut *tx)
                .await?;
                let role: String = sqlx::query_scalar("SELECT role FROM memberships WHERE org_id = $1 AND user_id = $2")
                    .bind(org)
                    .bind(user.as_uuid())
                    .fetch_one(&mut *tx)
                    .await?;
                let org = OrgId::from_uuid(org);
                out.push(Membership { org_id: org, user_id: user, role: parse_enum(&role, "memberships.role", parse_member_role)? });
                append_in(&mut tx, org, FeedKind::Member, &user.to_string(), now).await?;
            }
            tx.commit().await?;
            Ok::<_, TxError<OrgStoreError>>(out)
        })
        .await
    }

    async fn upsert_integration(&self, integration: &Integration, jobs: &[NewJob]) -> Result<(), OrgStoreError> {
        invalid_jobs(jobs)?;
        let now = self.clock.now();
        let org = integration.org_id;
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            sqlx::query(
                "INSERT INTO integrations (org_id, kind, external_id, status, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, $5) \
                 ON CONFLICT (org_id, kind, external_id) DO UPDATE SET status = EXCLUDED.status, updated_at = EXCLUDED.updated_at",
            )
            .bind(org.as_uuid())
            .bind(integration_kind_str(integration.kind))
            .bind(&integration.external_id)
            .bind(integration_status_str(integration.status))
            .bind(now)
            .execute(&mut *tx)
            .await?;
            enqueue_all_in(&mut tx, jobs, now, &*self.ids).await?;
            append_in(&mut tx, org, FeedKind::Integration, &integration_subject(integration.kind, &integration.external_id), now).await?;
            tx.commit().await?;
            Ok::<_, TxError<OrgStoreError>>(())
        })
        .await
    }

    async fn integrations(&self, org: OrgId) -> Result<Vec<Integration>, OrgStoreError> {
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let rows: Vec<IntegrationRow> = sqlx::query_as(
                "SELECT org_id, kind, external_id, status FROM integrations WHERE org_id = $1 \
                 ORDER BY kind COLLATE \"C\", external_id COLLATE \"C\"",
            )
            .bind(org.as_uuid())
            .fetch_all(&mut *tx)
            .await?;
            tx.commit().await?;
            rows.into_iter().map(|r| Ok(Integration::try_from(r)?)).collect::<TxResult<Vec<_>>>()
        })
        .await
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
        let now = self.clock.now();
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let updated = sqlx::query("UPDATE integrations SET status = $4, updated_at = $5 WHERE org_id = $1 AND kind = $2 AND external_id = $3")
                .bind(org.as_uuid())
                .bind(integration_kind_str(kind))
                .bind(external_id)
                .bind(integration_status_str(status))
                .bind(now)
                .execute(&mut *tx)
                .await?
                .rows_affected();
            if updated == 0 {
                return Err(TxError::Port(OrgStoreError::NotFound));
            }
            enqueue_all_in(&mut tx, jobs, now, &*self.ids).await?;
            append_in(&mut tx, org, FeedKind::Integration, &integration_subject(kind, external_id), now).await?;
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    async fn integration_by_external(&self, kind: IntegrationKind, external_id: &str) -> Result<Option<Integration>, OrgStoreError> {
        retry_tx(|| async {
            let mut tx = begin_platform(&self.pools.platform).await?;
            let row: Option<IntegrationRow> = sqlx::query_as(
                "SELECT org_id, kind, external_id, status FROM integrations WHERE kind = $1 AND external_id = $2 \
                 ORDER BY (status = 'active') DESC, updated_at DESC, org_id LIMIT 1",
            )
            .bind(integration_kind_str(kind))
            .bind(external_id)
            .fetch_optional(&mut *tx)
            .await?;
            tx.commit().await?;
            Ok::<_, TxError<OrgStoreError>>(row.map(Integration::try_from).transpose()?)
        })
        .await
    }

    async fn integration_settings(&self, org: OrgId, kind: IntegrationKind, external_id: &str) -> Result<IntegrationSettings, OrgStoreError> {
        let value = retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let value: Option<Json<serde_json::Value>> =
                sqlx::query_scalar("SELECT settings FROM integrations WHERE org_id = $1 AND kind = $2 AND external_id = $3")
                    .bind(org.as_uuid())
                    .bind(integration_kind_str(kind))
                    .bind(external_id)
                    .fetch_optional(&mut *tx)
                    .await?;
            tx.commit().await?;
            value.map(|v| v.0).ok_or(TxError::Port(OrgStoreError::NotFound))
        })
        .await?;
        decode_json(Some(value), "integrations.settings")
    }

    async fn set_integration_settings(
        &self,
        org: OrgId,
        kind: IntegrationKind,
        external_id: &str,
        s: &IntegrationSettings,
    ) -> Result<(), OrgStoreError> {
        let value = to_json(s)?;
        let now = self.clock.now();
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let updated =
                sqlx::query("UPDATE integrations SET settings = $4, updated_at = $5 WHERE org_id = $1 AND kind = $2 AND external_id = $3")
                    .bind(org.as_uuid())
                    .bind(integration_kind_str(kind))
                    .bind(external_id)
                    .bind(Json(&value))
                    .bind(now)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected();
            if updated == 0 {
                return Err(TxError::Port(OrgStoreError::NotFound));
            }
            append_in(&mut tx, org, FeedKind::Integration, &integration_subject(kind, external_id), now).await?;
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    async fn intake_config(&self, org: OrgId) -> Result<IntakeConfig, OrgStoreError> {
        let value = retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let value: Option<Json<serde_json::Value>> =
                sqlx::query_scalar("SELECT config FROM org_intake_config WHERE org_id = $1").bind(org.as_uuid()).fetch_optional(&mut *tx).await?;
            tx.commit().await?;
            Ok::<_, TxError<OrgStoreError>>(value.map(|v| v.0))
        })
        .await?;
        decode_json(value, "org_intake_config.config")
    }

    async fn put_intake_config(&self, org: OrgId, config: &IntakeConfig) -> Result<(), OrgStoreError> {
        let value = to_json(config)?;
        let now = self.clock.now();
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            sqlx::query(
                "INSERT INTO org_intake_config (org_id, config, updated_at) VALUES ($1, $2, $3) \
                 ON CONFLICT (org_id) DO UPDATE SET config = EXCLUDED.config, updated_at = EXCLUDED.updated_at",
            )
            .bind(org.as_uuid())
            .bind(Json(&value))
            .bind(now)
            .execute(&mut *tx)
            .await?;
            append_in(&mut tx, org, FeedKind::Settings, &org.to_string(), now).await?;
            tx.commit().await?;
            Ok::<_, TxError<OrgStoreError>>(())
        })
        .await
    }

    async fn put_customer(&self, customer: &Customer) -> Result<(), OrgStoreError> {
        let (systems, values) = ref_arrays(&normalize_refs(&customer.external_refs));
        let now = self.clock.now();
        let org = customer.org_id;
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            sqlx::query(
                "INSERT INTO customers (org_id, id, name, environment, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, $5) \
                 ON CONFLICT (org_id, id) DO UPDATE \
                 SET name = EXCLUDED.name, environment = EXCLUDED.environment, updated_at = EXCLUDED.updated_at",
            )
            .bind(org.as_uuid())
            .bind(customer.id.as_uuid())
            .bind(&customer.name)
            .bind(&customer.environment)
            .bind(now)
            .execute(&mut *tx)
            .await?;
            sqlx::query("DELETE FROM customer_refs WHERE org_id = $1 AND customer_id = $2")
                .bind(org.as_uuid())
                .bind(customer.id.as_uuid())
                .execute(&mut *tx)
                .await?;
            sqlx::query(
                "INSERT INTO customer_refs (org_id, customer_id, system, value) \
                 SELECT $1, $2, r.system, r.value FROM UNNEST($3::text[], $4::text[]) AS r(system, value)",
            )
            .bind(org.as_uuid())
            .bind(customer.id.as_uuid())
            .bind(&systems)
            .bind(&values)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            Ok::<_, TxError<OrgStoreError>>(())
        })
        .await
    }

    async fn get_customer(&self, org: OrgId, id: CustomerId) -> Result<Customer, OrgStoreError> {
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let row: Option<CustomerRow> = sqlx::query_as("SELECT id, name, environment FROM customers WHERE org_id = $1 AND id = $2")
                .bind(org.as_uuid())
                .bind(id.as_uuid())
                .fetch_optional(&mut *tx)
                .await?;
            let row = row.ok_or(TxError::Port(OrgStoreError::NotFound))?;
            let customer = with_refs(&mut tx, org, vec![row]).await?.pop().ok_or(TxError::Port(OrgStoreError::NotFound))?;
            tx.commit().await?;
            Ok(customer)
        })
        .await
    }

    async fn find_customers_by_refs(&self, org: OrgId, refs: &[ExternalRef]) -> Result<Vec<Customer>, OrgStoreError> {
        if refs.is_empty() {
            return Ok(Vec::new());
        }
        let (systems, values) = ref_arrays(refs);
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let rows: Vec<CustomerRow> = sqlx::query_as(
                "SELECT id, name, environment FROM customers WHERE org_id = $1 AND id IN ( \
                   SELECT customer_id FROM customer_refs WHERE org_id = $1 \
                   AND (system, value) IN (SELECT r.system, r.value FROM UNNEST($2::text[], $3::text[]) AS r(system, value))) \
                 ORDER BY id",
            )
            .bind(org.as_uuid())
            .bind(&systems)
            .bind(&values)
            .fetch_all(&mut *tx)
            .await?;
            let customers = with_refs(&mut tx, org, rows).await?;
            tx.commit().await?;
            Ok::<_, TxError<OrgStoreError>>(customers)
        })
        .await
    }

    async fn list_customers(&self, org: OrgId, page: PageRequest) -> Result<Page<Customer>, OrgStoreError> {
        let after = uuid_cursor(page.after.as_ref())?;
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let rows: Vec<CustomerRow> = sqlx::query_as(
                "SELECT id, name, environment FROM customers WHERE org_id = $1 AND ($2::uuid IS NULL OR id > $2) ORDER BY id LIMIT $3",
            )
            .bind(org.as_uuid())
            .bind(after)
            .bind(page_limit(&page))
            .fetch_all(&mut *tx)
            .await?;
            let customers = with_refs(&mut tx, org, rows).await?;
            tx.commit().await?;
            Ok::<_, TxError<OrgStoreError>>(finish_page(customers, &page, |c| c.id.as_uuid().to_string()))
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use mage_core::ports::clock::Clock;
    use mage_core::ports::ids::IdGen;
    use mage_core::testing::SeqIdGen;
    use mage_infra::pg::PgJobQueue;

    use super::PgOrgStore;
    use crate::adapters::driven::pg::feed::PgChangeFeed;
    use crate::testing::contract::{OrgFixture, contract_org_store};
    use crate::testing::pg::with_pg;

    #[tokio::test]
    async fn pg_org_store_passes_the_contract() {
        with_pg("pg_org_contract", |env| async move {
            contract_org_store(|clock| {
                let env = env.clone();
                async move {
                    let db = env.fresh().await;
                    let clock: Arc<dyn Clock> = clock;
                    let ids: Arc<dyn IdGen> = Arc::new(SeqIdGen::new());
                    OrgFixture {
                        orgs: Arc::new(PgOrgStore::new(db.pools.clone(), clock.clone(), ids.clone())),
                        jobs: Arc::new(PgJobQueue::new(db.pools.clone(), clock.clone(), ids)),
                        feed: Arc::new(PgChangeFeed::new(db.pools.clone(), clock.clone())),
                        seed: db.seed(clock),
                    }
                }
            })
            .await;
        })
        .await;
    }
}
