//! Service-token routes: the edge's site directory, surface observations, archive downloads for
//! the builder.

use std::collections::HashMap;

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::Response;
use cloud_api::internal::{Observation, Sites};

use super::ApiJson;
use crate::auth::{Service, live_project};
use crate::error::AppError;
use crate::state::AppState;

pub async fn sites(State(st): State<AppState>, _: Service, Query(q): Query<HashMap<String, String>>) -> Result<Json<Sites>, AppError> {
    let since = q.get("since").map(|s| s.parse::<u64>().map_err(|_| AppError::invalid("`since` is a version number"))).transpose()?;
    Ok(Json(crate::sites::sites(&st, since).await?))
}

pub async fn observe(State(st): State<AppState>, _: Service, ApiJson(body): ApiJson<Observation>) -> Result<StatusCode, AppError> {
    let project = live_project(&st, &body.project).await?;
    st.ledgers.get(&project.slug)?.observe(&body.release, body.listeners, body.egress)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn archive(State(st): State<AppState>, _: Service, Path(hash): Path<String>) -> Result<Response, AppError> {
    crate::blobs::hash_hex(&hash)?;
    if !st.store.archive_exists(&hash).await? {
        return Err(AppError::not_found(format!("archive {hash}")));
    }
    let bytes = st.blobs.get(&hash).await?.ok_or_else(|| AppError::not_found(format!("archive {hash}")))?;
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from(bytes))
        .map_err(|e| AppError::internal(e.to_string()))
}
