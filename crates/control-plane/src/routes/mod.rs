//! The HTTP surface: every route in `cloud_api::routes`, plus the internal routes the edge,
//! runtime and builder call.

mod auth;
mod domains;
mod internal;
mod orgs;
mod projects;
mod releases;
mod sandboxes;
mod secrets;
mod workspace;

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, FromRequest, Request};
use axum::routing::{delete, get, post, put};
use cloud_api::internal::routes as ir;
use cloud_api::routes as r;
use serde::de::DeserializeOwned;

use crate::error::AppError;
use crate::state::AppState;

/// `axum::Json` whose rejections are [`cloud_api::ApiError`]s.
pub struct ApiJson<T>(pub T);

impl<T: DeserializeOwned, S: Send + Sync> FromRequest<S> for ApiJson<T> {
    type Rejection = AppError;

    async fn from_request(req: Request, state: &S) -> Result<Self, AppError> {
        axum::Json::<T>::from_request(req, state)
            .await
            .map(|axum::Json(v)| ApiJson(v))
            .map_err(|e: JsonRejection| AppError::invalid(e.body_text()))
    }
}

async fn not_found() -> AppError {
    AppError::not_found("no such route")
}

async fn method_not_allowed() -> AppError {
    AppError::invalid("method not allowed on this route")
}

/// The whole server.
pub fn router(state: AppState) -> Router {
    let uploads = Router::new()
        .route(r::ARCHIVES, post(projects::upload_archive))
        .route(r::SESSIONS, post(projects::upload_session))
        .layer(DefaultBodyLimit::max(state.cfg.max_upload_bytes));
    Router::new()
        .route(r::DEVICE_START, post(auth::device_start))
        .route(r::DEVICE_APPROVE, post(auth::device_approve))
        .route(r::DEVICE_POLL, post(auth::device_poll))
        .route(r::PASSKEY_REGISTER_START, post(auth::passkey_register_start))
        .route(r::PASSKEY_REGISTER_FINISH, post(auth::passkey_register_finish))
        .route(r::PASSKEY_LOGIN_START, post(auth::passkey_login_start))
        .route(r::PASSKEY_LOGIN_FINISH, post(auth::passkey_login_finish))
        .route(r::ME, get(auth::me))
        .route(r::ORGS, get(orgs::list).post(orgs::create))
        .route(r::ORG_MEMBERS, get(orgs::members).post(orgs::add_member))
        .route(r::ORG_POLICY, get(orgs::policy).put(orgs::set_policy))
        .route(r::ORG_PROJECTS, get(orgs::projects).post(orgs::create_project))
        .route(r::PROJECT, get(projects::get).delete(projects::delete))
        .route(r::MANIFEST_CHECK, post(releases::check))
        .route(r::RELEASES, get(releases::list).post(releases::propose))
        .route(r::RELEASE, get(releases::get))
        .route(r::APPROVE, post(releases::approve))
        .route(r::REJECT, post(releases::reject))
        .route(r::PROMOTE, post(releases::promote))
        .route(r::ROLLBACK, post(releases::rollback))
        .route(r::STANDING_ORDERS, post(releases::grant_standing_orders))
        .route(r::ONCALL, post(releases::oncall))
        .route(r::DOMAINS, get(domains::list).post(domains::add))
        .route(r::DOMAIN_VERIFY, post(domains::verify))
        .route(r::WORKSPACES, post(projects::workspace))
        .route(r::WORKSPACE_FILES, get(workspace::files))
        .route(r::WORKSPACE_FILE, get(workspace::file))
        .route(r::WORKSPACE_DEPLOY, post(workspace::deploy))
        .route(r::ORG_SANDBOXES, get(sandboxes::list).post(sandboxes::create))
        .route(r::SANDBOX, get(sandboxes::get).delete(sandboxes::delete))
        .route(r::SANDBOX_START, post(sandboxes::start))
        .route(r::SANDBOX_STOP, post(sandboxes::stop))
        .route(r::SANDBOX_SETTINGS, put(sandboxes::settings))
        .route(r::SANDBOX_TERMINAL, post(sandboxes::terminal))
        .route(r::SANDBOX_CHAT, post(sandboxes::chat))
        .route(r::SANDBOX_FILES, get(sandboxes::files))
        .route(r::SANDBOX_FILE, get(sandboxes::file))
        .route(r::SANDBOX_ENV, get(sandboxes::env).put(sandboxes::set_env))
        .route(r::SANDBOX_ENV_VAR, delete(sandboxes::delete_env))
        .route(r::LEDGER, get(projects::ledger))
        .route(r::SECRETS, get(secrets::list).put(secrets::set).post(secrets::set))
        .route(ir::SITES, get(internal::sites))
        .route(ir::OBSERVATIONS, post(internal::observe))
        .route(crate::ops::ARCHIVE_BLOB, get(internal::archive))
        .merge(uploads)
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .with_state(state)
}
