//! Who is calling: bearer sessions for people, the service token for sibling services, and the
//! org-membership checks every project route runs.

use axum::extract::FromRequestParts;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use cloud_api::User;

use crate::crypto::{ct_eq, random_token, token_hash};
use crate::error::AppError;
use crate::state::AppState;
use crate::store::{MemberRec, OrgRec, ProjectRec, TokenRec, UserRec};

fn bearer(parts: &Parts) -> Option<&str> {
    let v = parts.headers.get(AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = v.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| token.trim()).filter(|t| !t.is_empty())
}

async fn session_user(state: &AppState, token: &str) -> Result<Option<UserRec>, AppError> {
    let Some(rec) = state.store.token(&token_hash(token)).await? else {
        return Ok(None);
    };
    if rec.expires_at <= state.now() {
        return Ok(None);
    }
    Ok(state.store.user(&rec.user_id).await?)
}

/// A person with a valid bearer session.
pub struct Caller(pub UserRec);

impl FromRequestParts<AppState> for Caller {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, AppError> {
        let token = bearer(parts).ok_or_else(|| AppError::unauthorized("missing bearer token"))?;
        session_user(state, token).await?.map(Caller).ok_or_else(|| AppError::unauthorized("invalid or expired session"))
    }
}

/// A person with a valid session, or nobody (routes that also serve sign-up).
pub struct MaybeCaller(pub Option<UserRec>);

impl FromRequestParts<AppState> for MaybeCaller {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, AppError> {
        match bearer(parts) {
            None => Ok(MaybeCaller(None)),
            Some(token) => session_user(state, token)
                .await?
                .map(|u| MaybeCaller(Some(u)))
                .ok_or_else(|| AppError::unauthorized("invalid or expired session")),
        }
    }
}

/// A sibling service holding the shared service token.
pub struct Service;

impl FromRequestParts<AppState> for Service {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, AppError> {
        match bearer(parts) {
            Some(token) if ct_eq(token.as_bytes(), state.cfg.service_token.as_bytes()) => Ok(Service),
            _ => Err(AppError::unauthorized("service token required")),
        }
    }
}

/// On-call: the service token (automation) or a signed-in person (checked as an org owner by
/// the route).
pub enum OnCallCaller {
    Service,
    User(UserRec),
}

impl FromRequestParts<AppState> for OnCallCaller {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, AppError> {
        let token = bearer(parts).ok_or_else(|| AppError::unauthorized("missing bearer token"))?;
        if ct_eq(token.as_bytes(), state.cfg.service_token.as_bytes()) {
            return Ok(OnCallCaller::Service);
        }
        session_user(state, token).await?.map(OnCallCaller::User).ok_or_else(|| AppError::unauthorized("invalid or expired session"))
    }
}

/// Mints a bearer session for `user_id`; the store keeps only its hash.
pub async fn issue_session(state: &AppState, user_id: &str) -> Result<String, AppError> {
    let token = random_token("ahs");
    let rec = TokenRec { hash: token_hash(&token), user_id: user_id.to_owned(), expires_at: state.now() + state.cfg.session_ttl_secs };
    state.store.put_token(&rec).await?;
    Ok(token)
}

pub fn user_view(u: &UserRec) -> User {
    User { id: u.id.clone(), email: u.email.clone(), name: u.name.clone() }
}

/// The org, if `user` belongs to it.
pub async fn org_access(state: &AppState, user: &UserRec, org: &str) -> Result<(OrgRec, MemberRec), AppError> {
    let o = state.store.org(org).await?.ok_or_else(|| AppError::not_found(format!("org {org}")))?;
    let m = state.store.member(org, &user.id).await?.ok_or_else(|| AppError::forbidden(format!("not a member of {org}")))?;
    Ok((o, m))
}

/// The (not deleted) project, if `user` belongs to its org.
pub async fn project_access(state: &AppState, user: &UserRec, project: &str) -> Result<(ProjectRec, MemberRec), AppError> {
    let p = live_project(state, project).await?;
    let m = state.store.member(&p.org, &user.id).await?.ok_or_else(|| AppError::forbidden(format!("not a member of {}", p.org)))?;
    Ok((p, m))
}

/// The project unless it does not exist or was deleted.
pub async fn live_project(state: &AppState, project: &str) -> Result<ProjectRec, AppError> {
    match state.store.project(project).await? {
        Some(p) if p.deleted_at.is_none() => Ok(p),
        _ => Err(AppError::not_found(format!("project {project}"))),
    }
}

pub fn require_owner(m: &MemberRec, action: &str) -> Result<(), AppError> {
    if m.owner { Ok(()) } else { Err(AppError::forbidden(format!("{action} needs an org owner"))) }
}
