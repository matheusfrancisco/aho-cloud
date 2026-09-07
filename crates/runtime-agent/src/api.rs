//! The internal HTTP API the control plane drives (`cloud_api::internal::routes::DEPLOYMENT*`).
//! Every request needs `Authorization: Bearer <service token>`.

use std::sync::Arc;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{post, put};
use axum::{Json, Router};
use cloud_api::ApiError;
use cloud_api::internal::{Deployment, DeploymentStatus, Maintenance, Scale, routes};
use cloud_contract::ledger::Stage;

use crate::image::digest_hex;
use crate::supervisor::{Supervisor, SupervisorError};
use crate::vmm::Vmm;

struct Api<V> {
    supervisor: Supervisor<V>,
    /// blake3 of the service token; `blake3::Hash` equality is constant-time.
    token: Option<blake3::Hash>,
}

type Shared<V> = Arc<Api<V>>;

/// A JSON [`ApiError`] with its status.
struct Error(StatusCode, ApiError);

impl Error {
    fn new(status: StatusCode, code: &str, message: impl Into<String>) -> Self {
        Error(status, ApiError::new(code, message))
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        (self.0, Json(self.1)).into_response()
    }
}

impl From<SupervisorError> for Error {
    fn from(e: SupervisorError) -> Self {
        let status = match e {
            SupervisorError::NotFound(..) | SupervisorError::UnknownProcess(_) => StatusCode::NOT_FOUND,
            SupervisorError::TooManyReplicas { .. } | SupervisorError::Invalid(_) => StatusCode::BAD_REQUEST,
        };
        let code = if status == StatusCode::NOT_FOUND { "not_found" } else { "invalid" };
        Error::new(status, code, e.to_string())
    }
}

impl From<JsonRejection> for Error {
    fn from(e: JsonRejection) -> Self {
        Error::new(StatusCode::BAD_REQUEST, "invalid", e.body_text())
    }
}

/// The router, authenticated with `service_token` (an empty token admits nobody).
pub fn router<V: Vmm>(supervisor: Supervisor<V>, service_token: &str) -> Router {
    let token = (!service_token.is_empty()).then(|| blake3::hash(service_token.as_bytes()));
    let api: Shared<V> = Arc::new(Api { supervisor, token });
    Router::new()
        .route(routes::DEPLOYMENT, put(put_deployment::<V>).get(get_deployment::<V>).delete(delete_deployment::<V>))
        .route(routes::DEPLOYMENT_RESTART, post(restart::<V>))
        .route(routes::DEPLOYMENT_SCALE, post(scale::<V>))
        .route(routes::DEPLOYMENT_MAINTENANCE, post(maintenance::<V>))
        .route(routes::DEPLOYMENT_SLEEP, post(sleep::<V>))
        .route_layer(middleware::from_fn_with_state(api.clone(), authenticate::<V>))
        .with_state(api)
}

async fn authenticate<V: Vmm>(State(api): State<Shared<V>>, request: Request, next: Next) -> Response {
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .filter(|t| !t.is_empty());
    let ok = match (presented, api.token) {
        (Some(t), Some(expected)) => blake3::hash(t.as_bytes()) == expected,
        _ => false,
    };
    if !ok {
        return Error::new(StatusCode::UNAUTHORIZED, "unauthorized", "missing or invalid service token").into_response();
    }
    next.run(request).await
}

fn stage(s: &str) -> Result<Stage, Error> {
    serde_json::from_value(serde_json::Value::String(s.to_owned()))
        .map_err(|_| Error::new(StatusCode::NOT_FOUND, "not_found", format!("no stage `{s}`")))
}

type Reply = Result<Json<DeploymentStatus>, Error>;

async fn put_deployment<V: Vmm>(
    State(api): State<Shared<V>>,
    Path((project, stage_name)): Path<(String, String)>,
    body: Result<Json<Deployment>, JsonRejection>,
) -> Reply {
    let stage = stage(&stage_name)?;
    let Json(d) = body?;
    if d.project != project || d.stage != stage {
        return Err(Error::new(
            StatusCode::BAD_REQUEST,
            "invalid",
            format!("body is for {}/{}, path is {project}/{stage_name}", d.project, d.stage.as_str()),
        ));
    }
    if let Err(refusals) = cloud_contract::validate(&d.manifest) {
        let mut error = ApiError::new("refused", "the manifest does not pass the gate");
        error.refusals = refusals;
        return Err(Error(StatusCode::BAD_REQUEST, error));
    }
    digest_hex(&d.image_digest).map_err(|e| Error::new(StatusCode::BAD_REQUEST, "invalid", e.to_string()))?;
    Ok(Json(api.supervisor.apply(d)?))
}

async fn get_deployment<V: Vmm>(State(api): State<Shared<V>>, Path((project, stage_name)): Path<(String, String)>) -> Reply {
    Ok(Json(api.supervisor.status(&project, stage(&stage_name)?)?))
}

async fn delete_deployment<V: Vmm>(
    State(api): State<Shared<V>>,
    Path((project, stage_name)): Path<(String, String)>,
) -> Result<StatusCode, Error> {
    api.supervisor.remove(&project, stage(&stage_name)?)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn restart<V: Vmm>(State(api): State<Shared<V>>, Path((project, stage_name)): Path<(String, String)>) -> Reply {
    Ok(Json(api.supervisor.restart(&project, stage(&stage_name)?)?))
}

async fn scale<V: Vmm>(
    State(api): State<Shared<V>>,
    Path((project, stage_name)): Path<(String, String)>,
    body: Result<Json<Scale>, JsonRejection>,
) -> Reply {
    let stage = stage(&stage_name)?;
    let Json(s) = body?;
    Ok(Json(api.supervisor.scale(&project, stage, &s.process, s.replicas)?))
}

async fn maintenance<V: Vmm>(
    State(api): State<Shared<V>>,
    Path((project, stage_name)): Path<(String, String)>,
    body: Result<Json<Maintenance>, JsonRejection>,
) -> Reply {
    let stage = stage(&stage_name)?;
    let Json(m) = body?;
    Ok(Json(api.supervisor.maintenance(&project, stage, m.on)?))
}

async fn sleep<V: Vmm>(State(api): State<Shared<V>>, Path((project, stage_name)): Path<(String, String)>) -> Reply {
    Ok(Json(api.supervisor.sleep(&project, stage(&stage_name)?)?))
}
