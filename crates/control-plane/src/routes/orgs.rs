//! Orgs, members, the release policy, and project creation.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use cloud_api::{AddMember, CreateOrg, CreateProject, Member, Org, Project, SetPolicy};
use cloud_contract::policy::ReleasePolicy;

use super::ApiJson;
use super::auth::normalize_email;
use crate::approvals::{approver, policy_message};
use crate::auth::{Caller, org_access, require_owner, user_view};
use crate::error::AppError;
use crate::ops::ledger_state;
use crate::state::AppState;
use crate::store::{MemberRec, OrgRec, ProjectRec, UserRec};
use crate::view;

/// Lowercase letters, digits and `-`, 1–40 chars, starting with a letter (the manifest's
/// project-slug rule, used for orgs too).
pub fn valid_slug(s: &str) -> bool {
    (1..=40).contains(&s.len())
        && s.starts_with(|c: char| c.is_ascii_lowercase())
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

async fn org_view(st: &AppState, org: OrgRec) -> Result<Org, AppError> {
    let mut members = Vec::new();
    for m in st.store.members(&org.slug).await? {
        if let Some(u) = st.store.user(&m.user_id).await? {
            members.push(Member { user: user_view(&u), owner: m.owner });
        }
    }
    Ok(Org { slug: org.slug, name: org.name, policy: org.policy, members })
}

pub async fn list(State(st): State<AppState>, Caller(user): Caller) -> Result<Json<Vec<Org>>, AppError> {
    let mut out = Vec::new();
    for m in st.store.memberships(&user.id).await? {
        if let Some(org) = st.store.org(&m.org).await? {
            out.push(org_view(&st, org).await?);
        }
    }
    Ok(Json(out))
}

pub async fn create(
    State(st): State<AppState>,
    Caller(user): Caller,
    ApiJson(body): ApiJson<CreateOrg>,
) -> Result<(StatusCode, Json<Org>), AppError> {
    if !valid_slug(&body.slug) {
        return Err(AppError::invalid("org slug: 1–40 lowercase letters, digits or `-`, starting with a letter"));
    }
    let name = body.name.trim();
    if name.is_empty() || name.len() > 100 {
        return Err(AppError::invalid("org name must be 1–100 characters"));
    }
    let org = OrgRec { slug: body.slug, name: name.to_owned(), policy: ReleasePolicy::default(), policy_approver: None };
    st.store.create_org(&org, &user.id).await?;
    Ok((StatusCode::CREATED, Json(org_view(&st, org).await?)))
}

pub async fn members(State(st): State<AppState>, Caller(user): Caller, Path(org): Path<String>) -> Result<Json<Vec<Member>>, AppError> {
    let (org, _) = org_access(&st, &user, &org).await?;
    Ok(Json(org_view(&st, org).await?.members))
}

pub async fn add_member(
    State(st): State<AppState>,
    Caller(user): Caller,
    Path(org): Path<String>,
    ApiJson(body): ApiJson<AddMember>,
) -> Result<Json<Member>, AppError> {
    let (org, me) = org_access(&st, &user, &org).await?;
    require_owner(&me, "adding members")?;
    let email = normalize_email(&body.email)?;
    let added: UserRec =
        st.store.user_by_email(&email).await?.ok_or_else(|| AppError::not_found(format!("no account for {email}; they sign up first")))?;
    if added.id == user.id && !body.owner {
        let owners = st.store.members(&org.slug).await?.into_iter().filter(|m| m.owner).count();
        if owners <= 1 {
            return Err(AppError::conflict("an org keeps at least one owner"));
        }
    }
    st.store.put_member(&MemberRec { org: org.slug.clone(), user_id: added.id.clone(), owner: body.owner }).await?;
    record_member_count(&st, &org.slug).await?;
    Ok(Json(Member { user: user_view(&added), owner: body.owner }))
}

/// How many members `org` has, for the ledgers' quorum cap.
async fn member_count(st: &AppState, org: &str) -> Result<u16, AppError> {
    Ok(u16::try_from(st.store.members(org).await?.len()).unwrap_or(u16::MAX))
}

/// Records `org`'s member count in the ledger of each of its projects that has a different one
/// (or none): no approval quorum may exceed the humans there are.
async fn record_member_count(st: &AppState, org: &str) -> Result<(), AppError> {
    let count = member_count(st, org).await?;
    for p in st.store.projects().await?.into_iter().filter(|p| p.org == org && p.deleted_at.is_none()) {
        let ledger = st.ledgers.get(&p.slug)?;
        if ledger.state()?.members != Some(count) {
            ledger.members_changed(count)?;
        }
    }
    Ok(())
}

pub async fn policy(State(st): State<AppState>, Caller(user): Caller, Path(org): Path<String>) -> Result<Json<ReleasePolicy>, AppError> {
    let (org, _) = org_access(&st, &user, &org).await?;
    Ok(Json(org.policy))
}

pub async fn set_policy(
    State(st): State<AppState>,
    Caller(user): Caller,
    Path(org): Path<String>,
    ApiJson(body): ApiJson<SetPolicy>,
) -> Result<Json<Org>, AppError> {
    let (org, me) = org_access(&st, &user, &org).await?;
    require_owner(&me, "changing the release policy")?;
    if body.policy.non_widening == 0 || body.policy.widening == 0 {
        return Err(AppError::invalid("a policy needs at least one approval per release"));
    }
    let a = approver(&st, &user, true, &policy_message(&org.slug, &body.policy), &body.proof).await?;
    for p in st.store.projects().await?.into_iter().filter(|p| p.org == org.slug && p.deleted_at.is_none()) {
        st.ledgers.get(&p.slug)?.set_policy(body.policy.clone(), a.clone())?;
    }
    st.store.set_org_policy(&org.slug, &body.policy, &a).await?;
    let org = st.store.org(&org.slug).await?.ok_or_else(|| AppError::internal("org vanished"))?;
    Ok(Json(org_view(&st, org).await?))
}

pub async fn projects(State(st): State<AppState>, Caller(user): Caller, Path(org): Path<String>) -> Result<Json<Vec<Project>>, AppError> {
    let (org, _) = org_access(&st, &user, &org).await?;
    let registry = st.registry.lock().await.clone();
    let mut out = Vec::new();
    for p in st.store.projects().await?.into_iter().filter(|p| p.org == org.slug && p.deleted_at.is_none()) {
        out.push(view::project(&p.slug, &p.org, &ledger_state(&st, &p.slug)?, &registry));
    }
    Ok(Json(out))
}

pub async fn create_project(
    State(st): State<AppState>,
    Caller(user): Caller,
    Path(org): Path<String>,
    ApiJson(body): ApiJson<CreateProject>,
) -> Result<(StatusCode, Json<Project>), AppError> {
    let (org, _) = org_access(&st, &user, &org).await?;
    if !valid_slug(&body.slug) {
        return Err(AppError::invalid("project slug: 1–40 lowercase letters, digits or `-`, starting with a letter"));
    }
    st.store.create_project(&ProjectRec { slug: body.slug.clone(), org: org.slug.clone(), created_at: st.now(), deleted_at: None }).await?;
    let ledger = st.ledgers.get(&body.slug)?;
    if let Some(a) = org.policy_approver.clone() {
        ledger.set_policy(org.policy.clone(), a)?;
    }
    ledger.members_changed(member_count(&st, &org.slug).await?)?;
    let registry = st.registry.lock().await.clone();
    Ok((StatusCode::CREATED, Json(view::project(&body.slug, &org.slug, &ledger.state()?, &registry))))
}

#[cfg(test)]
mod tests {
    use super::valid_slug;

    #[test]
    fn slugs_follow_the_manifest_rule() {
        assert!(valid_slug("acme-shop2"));
        for bad in ["", "2acme", "Acme", "acme_shop", "-acme", &"a".repeat(41)] {
            assert!(!valid_slug(bad), "{bad}");
        }
    }
}
