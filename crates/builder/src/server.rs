//! The builder's HTTP face: `POST` [`routes::BUILD`] with a [`BuildJob`], answered with a
//! [`BuildResult`] once the build is done. Service-token auth runs before the body is read.

use std::sync::Arc;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use cloud_api::ApiError;
use cloud_api::internal::{BuildJob, BuildResult, routes};

use crate::Builder;

/// The builder's routes. Requests without `Authorization: Bearer <service token>` get `401`;
/// unparsable jobs get `400` (`invalid`); every parsed job gets `200` with its [`BuildResult`],
/// `Failed` included.
pub fn router(builder: Arc<Builder>) -> Router {
    Router::new()
        .route(routes::BUILD, post(build))
        .route_layer(middleware::from_fn_with_state(builder.clone(), require_service_token))
        .with_state(builder)
}

async fn require_service_token(State(builder): State<Arc<Builder>>, request: Request, next: Next) -> Response {
    let presented = request.headers().get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "));
    match presented {
        Some(token) if builder.accepts_token(token) => next.run(request).await,
        _ => {
            let mut response =
                (StatusCode::UNAUTHORIZED, Json(ApiError::new("unauthorized", "a valid service token is required"))).into_response();
            response.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
            response
        }
    }
}

async fn build(State(builder): State<Arc<Builder>>, job: Result<Json<BuildJob>, JsonRejection>) -> Response {
    match job {
        Ok(Json(job)) => Json::<BuildResult>(builder.build(&job).await).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(ApiError::new("invalid", e.body_text()))).into_response(),
    }
}
