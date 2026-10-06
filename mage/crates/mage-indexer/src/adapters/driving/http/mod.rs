//! Internal HTTP API of the indexer, `/internal/v1` (knowledge.md "Text search, blame, and diff
//! on Cloud Run"; routes and DTOs in `mage_core::wire::indexer`), served on
//! `indexer.internal_listen` inside the VPC.
//!
//! Every `POST` carries `Authorization: Bearer mgi_…` ([`auth::ServiceAuth`]); the body's
//! `org_id`/`repo_id` must equal the token's claims (403 otherwise). `GET /health` is
//! unauthenticated: it reveals counts only and is what probes call. Errors use the
//! [`IndexerError`] body with the status of its code.

pub mod auth;

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use mage_core::domain::{OrgId, RepoId};
use mage_core::wire::indexer::{
    BlameRequest, BlameResponse, DiffRequest, DiffResponse, HealthResponse, IndexerError, IndexerErrorCode, LogRequest, LogResponse, SearchRequest,
    TextHits, routes,
};
use serde::de::DeserializeOwned;

use self::auth::{AuthError, IndexerCaller, ServiceAuth};
use crate::app::live::LiveService;
use crate::error::AppError;

/// Request bodies are small JSON documents.
pub const MAX_BODY_BYTES: usize = 64 * 1024;

struct ApiState {
    live: Arc<LiveService>,
    auth: Arc<ServiceAuth>,
}

/// The `/internal/v1` router (state applied).
pub fn router(live: Arc<LiveService>, auth: Arc<ServiceAuth>) -> Router {
    Router::new()
        .route(routes::SEARCH, post(search))
        .route(routes::BLAME, post(blame))
        .route(routes::DIFF, post(diff))
        .route(routes::LOG, post(log))
        .route(routes::HEALTH, get(health))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(Arc::new(ApiState { live, auth }))
}

/// An error answer: `IndexerError` JSON with the code's status.
#[derive(Debug)]
pub struct ApiError(IndexerError);

impl ApiError {
    fn new(code: IndexerErrorCode, message: impl Into<String>) -> Self {
        ApiError(IndexerError { code, message: message.into() })
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.0.code.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        if status.is_server_error() {
            tracing::warn!(code = ?self.0.code, message = %self.0.message, "internal api error");
        }
        (status, Json(self.0)).into_response()
    }
}

impl From<AppError> for ApiError {
    fn from(err: AppError) -> Self {
        let code = match &err {
            AppError::NotFound(_) => IndexerErrorCode::NotFound,
            AppError::Invalid(_) | AppError::Domain(_) => IndexerErrorCode::Invalid,
            AppError::Conflict(_) | AppError::Unavailable(_) | AppError::RateLimited { .. } => IndexerErrorCode::Unavailable,
            AppError::Internal(_) => IndexerErrorCode::Internal,
        };
        ApiError::new(code, err.to_string())
    }
}

impl From<AuthError> for ApiError {
    fn from(err: AuthError) -> Self {
        let code = match err {
            AuthError::KeysUnavailable(_) => IndexerErrorCode::Unavailable,
            _ => IndexerErrorCode::Unauthorized,
        };
        ApiError::new(code, err.to_string())
    }
}

/// Bearer token → verified caller.
async fn authenticate(state: &ApiState, headers: &HeaderMap) -> Result<IndexerCaller, ApiError> {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or(AuthError::Missing)?;
    Ok(state.auth.verify(token).await?)
}

fn parse<T: DeserializeOwned>(body: &Bytes) -> Result<T, ApiError> {
    serde_json::from_slice(body).map_err(|e| ApiError::new(IndexerErrorCode::Invalid, format!("body: {e}")))
}

/// The body must address the token's org and repo.
fn in_scope(caller: IndexerCaller, org: OrgId, repo: RepoId) -> Result<(), ApiError> {
    if caller.org != org || caller.repo != repo {
        return Err(ApiError::new(IndexerErrorCode::Forbidden, "org_id/repo_id differ from the token claims"));
    }
    Ok(())
}

async fn search(State(state): State<Arc<ApiState>>, headers: HeaderMap, body: Bytes) -> Result<Json<TextHits>, ApiError> {
    let caller = authenticate(&state, &headers).await?;
    let req: SearchRequest = parse(&body)?;
    in_scope(caller, req.org_id, req.repo_id)?;
    Ok(Json(state.live.search(&req).await?))
}

async fn blame(State(state): State<Arc<ApiState>>, headers: HeaderMap, body: Bytes) -> Result<Json<BlameResponse>, ApiError> {
    let caller = authenticate(&state, &headers).await?;
    let req: BlameRequest = parse(&body)?;
    in_scope(caller, req.org_id, req.repo_id)?;
    Ok(Json(state.live.blame(&req).await?))
}

async fn diff(State(state): State<Arc<ApiState>>, headers: HeaderMap, body: Bytes) -> Result<Json<DiffResponse>, ApiError> {
    let caller = authenticate(&state, &headers).await?;
    let req: DiffRequest = parse(&body)?;
    in_scope(caller, req.org_id, req.repo_id)?;
    Ok(Json(state.live.diff(&req).await?))
}

async fn log(State(state): State<Arc<ApiState>>, headers: HeaderMap, body: Bytes) -> Result<Json<LogResponse>, ApiError> {
    let caller = authenticate(&state, &headers).await?;
    let req: LogRequest = parse(&body)?;
    in_scope(caller, req.org_id, req.repo_id)?;
    Ok(Json(state.live.log(&req).await?))
}

async fn health(State(state): State<Arc<ApiState>>) -> Json<HealthResponse> {
    Json(state.live.health().await)
}

#[cfg(test)]
mod tests;
