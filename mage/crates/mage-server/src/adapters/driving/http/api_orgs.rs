//! Platform routes of the UI JSON API (`/api/v1`, session + CSRF applied by the shell):
//! session, short-link resolution, org create, members and invites, settings, model route and
//! BYOK key, integrations (list, connect, revoke, provider callbacks), audit, usage.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde::Deserialize;

use mage_core::domain::{MemberRole, UserId};
use mage_core::wire::api::{
    AuditQuery, CreateOrgRequest, InviteRequest, ModelSettingsUpdate, OrgSettings, SetModelKeyRequest, SetRoleRequest, UsageQuery,
    integration_from_slug, routes,
};

use crate::adapters::driving::http::auth::{Admin, Authed, Member, Meta, Owner, RequireRole};
use crate::adapters::driving::http::error::{JsonBody, QueryParams};
use crate::app::auth::{Login, SessionInfo, Sessions};
use crate::app::orgs::members::Caller;
use crate::app::orgs::{Activity, CreateOrg, Directory, Members, OrgOwner};
use crate::app::settings::{IntegrationCallback, Integrations, ModelSettingsService, OrgSettingsService};
use crate::error::AppError;
use crate::ports::audit::Actor;

#[derive(Clone)]
pub struct OrgsApiState {
    pub sessions: Arc<Sessions>,
    pub login: Arc<Login>,
    pub directory: Arc<Directory>,
    pub create: Arc<CreateOrg>,
    /// `POST /api/v1/orgs` is public-product only; v1 answers 404 (orgs come from the CLI).
    pub public_org_create: bool,
    pub members: Arc<Members>,
    pub settings: Arc<OrgSettingsService>,
    pub model: Arc<ModelSettingsService>,
    pub integrations: Arc<Integrations>,
    pub activity: Arc<Activity>,
    /// Extra feature flags for `GET /api/v1/session`.
    pub features: Vec<String>,
}

/// `/api/v1` routes of this module (the shell adds session, CSRF and `OrgCtx`).
pub fn router(state: OrgsApiState) -> Router {
    Router::new()
        .route(routes::SESSION, get(session))
        .route(routes::RESOLVE, get(resolve))
        .route(routes::ORGS, post(create_org))
        .route(routes::MEMBERS, get(members))
        .route(routes::MEMBER_INVITES, post(invite))
        .route(routes::MEMBER_INVITE, delete(withdraw_invite))
        .route(routes::MEMBER, put(set_role).delete(remove_member))
        .route(routes::SETTINGS, get(get_settings).put(put_settings))
        .route(routes::SETTINGS_MODEL, get(get_model).put(put_model))
        .route(routes::SETTINGS_MODEL_KEY, put(put_model_key).delete(delete_model_key))
        .route(routes::INTEGRATIONS, get(integrations))
        .route(routes::INTEGRATION_CONNECT, post(connect_integration))
        .route(routes::INTEGRATION, delete(revoke_integration))
        .route(routes::AUDIT, get(audit))
        .route(routes::USAGE, get(usage))
        .with_state(state)
}

/// `GET /integrations/{kind}/callback` (session; the org comes from the install state).
pub fn callbacks_router(state: OrgsApiState) -> Router {
    Router::new().route("/integrations/{kind}/callback", get(integration_callback)).with_state(state)
}

fn param<T: FromStr>(params: &HashMap<String, String>, name: &str) -> Result<T, AppError> {
    params.get(name).and_then(|v| v.parse().ok()).ok_or_else(|| AppError::NotFound(name.to_owned()))
}

fn caller(ctx: &crate::adapters::driving::http::auth::OrgCtx) -> Caller {
    Caller { org: ctx.org_id, user: ctx.user_id, role: ctx.role }
}

async fn session(State(s): State<OrgsApiState>, authed: Authed, Extension(info): Extension<SessionInfo>) -> Result<Response, AppError> {
    let csrf = s.sessions.csrf_token(&info).await?;
    let mut features: Vec<String> = s.login.providers().into_iter().map(|p| format!("{}_login", p.as_str())).collect();
    features.extend(s.features.iter().cloned());
    Ok(Json(s.directory.session_view(authed.user_id, csrf, features).await?).into_response())
}

async fn resolve(State(s): State<OrgsApiState>, authed: Authed, Path(id): Path<String>) -> Result<Response, AppError> {
    Ok(Json(s.directory.resolve(authed.user_id, &id).await?).into_response())
}

async fn create_org(State(s): State<OrgsApiState>, authed: Authed, Meta(meta): Meta, JsonBody(req): JsonBody<CreateOrgRequest>) -> Result<Response, AppError> {
    if !s.public_org_create {
        return Err(AppError::NotFound("route".into()));
    }
    let org = s.create.create(&req.slug, &req.name, OrgOwner::User(authed.user_id), &Actor::User(authed.user_id), &meta).await?;
    Ok((StatusCode::CREATED, Json(org)).into_response())
}

async fn members(State(s): State<OrgsApiState>, ctx: RequireRole<Member>) -> Result<Response, AppError> {
    Ok(Json(s.members.list(ctx.org_id).await?).into_response())
}

async fn invite(State(s): State<OrgsApiState>, ctx: RequireRole<Admin>, Meta(meta): Meta, JsonBody(req): JsonBody<InviteRequest>) -> Result<Response, AppError> {
    Ok((StatusCode::CREATED, Json(s.members.invite(caller(&ctx), &req, &meta).await?)).into_response())
}

async fn withdraw_invite(
    State(s): State<OrgsApiState>,
    ctx: RequireRole<Admin>,
    Meta(meta): Meta,
    Path(p): Path<HashMap<String, String>>,
) -> Result<Response, AppError> {
    let invite = p.get("invite").ok_or_else(|| AppError::NotFound("invite".into()))?;
    s.members.withdraw(caller(&ctx), invite, &meta).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn set_role(
    State(s): State<OrgsApiState>,
    ctx: RequireRole<Owner>,
    Meta(meta): Meta,
    Path(p): Path<HashMap<String, String>>,
    JsonBody(req): JsonBody<SetRoleRequest>,
) -> Result<Response, AppError> {
    let user: UserId = param(&p, "user")?;
    Ok(Json(s.members.set_role(caller(&ctx), user, req.role, &meta).await?).into_response())
}

/// Admin minimum; removing an Admin or Owner needs Owner (checked by the use case).
async fn remove_member(State(s): State<OrgsApiState>, ctx: RequireRole<Admin>, Meta(meta): Meta, Path(p): Path<HashMap<String, String>>) -> Result<Response, AppError> {
    let user: UserId = param(&p, "user")?;
    s.members.remove(caller(&ctx), user, &meta).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn get_settings(State(s): State<OrgsApiState>, ctx: RequireRole<Member>) -> Result<Response, AppError> {
    Ok(Json(s.settings.get(ctx.org_id).await?).into_response())
}

async fn put_settings(State(s): State<OrgsApiState>, ctx: RequireRole<Admin>, Meta(meta): Meta, JsonBody(req): JsonBody<OrgSettings>) -> Result<Response, AppError> {
    Ok(Json(s.settings.put(ctx.org_id, ctx.user_id, &req, &meta).await?).into_response())
}

async fn get_model(State(s): State<OrgsApiState>, ctx: RequireRole<Admin>) -> Result<Response, AppError> {
    Ok(Json(s.model.view(ctx.org_id).await?).into_response())
}

async fn put_model(State(s): State<OrgsApiState>, ctx: RequireRole<Owner>, Meta(meta): Meta, JsonBody(req): JsonBody<ModelSettingsUpdate>) -> Result<Response, AppError> {
    Ok(Json(s.model.update(ctx.org_id, ctx.user_id, &req, &meta).await?).into_response())
}

/// Write-only: the body is never logged (`SetModelKeyRequest` redacts its `Debug`).
async fn put_model_key(State(s): State<OrgsApiState>, ctx: RequireRole<Owner>, Meta(meta): Meta, JsonBody(req): JsonBody<SetModelKeyRequest>) -> Result<Response, AppError> {
    Ok(Json(s.model.set_key(ctx.org_id, ctx.user_id, &req.api_key, &meta).await?).into_response())
}

async fn delete_model_key(State(s): State<OrgsApiState>, ctx: RequireRole<Owner>, Meta(meta): Meta) -> Result<Response, AppError> {
    s.model.delete_key(ctx.org_id, ctx.user_id, &meta).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn integrations(State(s): State<OrgsApiState>, ctx: RequireRole<Admin>) -> Result<Response, AppError> {
    Ok(Json(s.integrations.list(ctx.org_id).await?).into_response())
}

/// Admin minimum; `github-fix` needs Owner (checked by the use case).
async fn connect_integration(State(s): State<OrgsApiState>, ctx: RequireRole<Admin>, Path(p): Path<HashMap<String, String>>) -> Result<Response, AppError> {
    let kind = p.get("kind").and_then(|k| integration_from_slug(k)).ok_or_else(|| AppError::NotFound("integration kind".into()))?;
    Ok(Json(s.integrations.connect(ctx.org_id, ctx.user_id, ctx.role, kind).await?).into_response())
}

async fn revoke_integration(
    State(s): State<OrgsApiState>,
    ctx: RequireRole<Admin>,
    Meta(meta): Meta,
    Path(p): Path<HashMap<String, String>>,
) -> Result<Response, AppError> {
    let kind = p.get("kind").and_then(|k| integration_from_slug(k)).ok_or_else(|| AppError::NotFound("integration kind".into()))?;
    let external_id = p.get("external_id").ok_or_else(|| AppError::NotFound("integration".into()))?;
    s.integrations.revoke(ctx.org_id, ctx.user_id, ctx.role, kind, external_id, &meta).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Debug, Deserialize)]
struct CallbackQuery {
    state: Option<String>,
    code: Option<String>,
    installation_id: Option<String>,
    setup_action: Option<String>,
    error: Option<String>,
}

async fn integration_callback(
    State(s): State<OrgsApiState>,
    authed: Authed,
    Meta(meta): Meta,
    Path(kind): Path<String>,
    QueryParams(q): QueryParams<CallbackQuery>,
) -> Result<Response, AppError> {
    let kind = integration_from_slug(&kind).ok_or_else(|| AppError::NotFound("integration kind".into()))?;
    let cb = IntegrationCallback { state: q.state, code: q.code, installation_id: q.installation_id, setup_action: q.setup_action, error: q.error };
    let to = s.integrations.callback(authed.user_id, kind, &cb, &meta).await;
    Ok(Redirect::to(&to).into_response())
}

async fn audit(State(s): State<OrgsApiState>, ctx: RequireRole<Admin>, QueryParams(q): QueryParams<AuditQuery>) -> Result<Response, AppError> {
    Ok(Json(s.activity.audit_page(ctx.org_id, &q).await?).into_response())
}

async fn usage(State(s): State<OrgsApiState>, ctx: RequireRole<Admin>, QueryParams(q): QueryParams<UsageQuery>) -> Result<Response, AppError> {
    Ok(Json(s.activity.usage(ctx.org_id, &q).await?).into_response())
}

/// Minimum role per route of this module (the auth matrix test walks it).
pub const ROUTE_ROLES: &[(&str, &str, Option<MemberRole>)] = &[
    ("GET", routes::SESSION, None),
    ("GET", routes::RESOLVE, None),
    ("POST", routes::ORGS, None),
    ("GET", routes::MEMBERS, Some(MemberRole::Member)),
    ("POST", routes::MEMBER_INVITES, Some(MemberRole::Admin)),
    ("DELETE", routes::MEMBER_INVITE, Some(MemberRole::Admin)),
    ("PUT", routes::MEMBER, Some(MemberRole::Owner)),
    ("DELETE", routes::MEMBER, Some(MemberRole::Admin)),
    ("GET", routes::SETTINGS, Some(MemberRole::Member)),
    ("PUT", routes::SETTINGS, Some(MemberRole::Admin)),
    ("GET", routes::SETTINGS_MODEL, Some(MemberRole::Admin)),
    ("PUT", routes::SETTINGS_MODEL, Some(MemberRole::Owner)),
    ("PUT", routes::SETTINGS_MODEL_KEY, Some(MemberRole::Owner)),
    ("DELETE", routes::SETTINGS_MODEL_KEY, Some(MemberRole::Owner)),
    ("GET", routes::INTEGRATIONS, Some(MemberRole::Admin)),
    ("POST", routes::INTEGRATION_CONNECT, Some(MemberRole::Admin)),
    ("DELETE", routes::INTEGRATION, Some(MemberRole::Admin)),
    ("GET", routes::AUDIT, Some(MemberRole::Admin)),
    ("GET", routes::USAGE, Some(MemberRole::Admin)),
];
