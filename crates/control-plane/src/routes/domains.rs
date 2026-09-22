//! Domain bindings: add (TXT token + routing records), verify against DNS, list.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use cloud_api::{AddDomain, DomainView};

use super::ApiJson;
use crate::auth::{Caller, project_access};
use crate::error::AppError;
use crate::state::AppState;

pub async fn list(State(st): State<AppState>, Caller(user): Caller, Path(p): Path<String>) -> Result<Json<Vec<DomainView>>, AppError> {
    let (project, _) = project_access(&st, &user, &p).await?;
    let registry = st.registry.lock().await;
    let edge = &st.cfg.edge_target;
    Ok(Json(registry.for_project(&project.slug).into_iter().map(|b| registry.view(b, edge)).collect()))
}

pub async fn add(
    State(st): State<AppState>,
    Caller(user): Caller,
    Path(p): Path<String>,
    ApiJson(body): ApiJson<AddDomain>,
) -> Result<(StatusCode, Json<DomainView>), AppError> {
    let (project, _) = project_access(&st, &user, &p).await?;
    let mut registry = st.registry.lock().await;
    let binding = registry.add(&project.slug, &body.hostname, body.mode, st.now())?;
    st.save_registry(&registry).await?;
    Ok((StatusCode::CREATED, Json(registry.view(&binding, &st.cfg.edge_target))))
}

pub async fn verify(
    State(st): State<AppState>,
    Caller(user): Caller,
    Path((p, host)): Path<(String, String)>,
) -> Result<Json<DomainView>, AppError> {
    let (project, _) = project_access(&st, &user, &p).await?;
    let host = cloud_domains::normalize_name(&host);
    let mut registry = st.registry.lock().await;
    if !registry.for_project(&project.slug).iter().any(|b| b.hostname == host) {
        return Err(AppError::not_found(format!("{host} is not bound to {}", project.slug)));
    }
    registry.check(&host, st.services.resolver.as_ref(), st.now()).await?;
    st.save_registry(&registry).await?;
    let binding = registry
        .for_project(&project.slug)
        .into_iter()
        .find(|b| b.hostname == host)
        .ok_or_else(|| AppError::internal("binding vanished during check"))?;
    Ok(Json(registry.view(binding, &st.cfg.edge_target)))
}
