//! `AppError` → HTTP for the session groups (`/api`, `/events`, `/auth`): server.md "Error
//! formats", body `{"error": {"code", "message", "request_id"}}` (`mage_core::wire::api`).
//!
//! `IntoResponse` cannot see the request, so the body is rendered with an empty `request_id`
//! and the error is stashed in the response extensions; the shell's request-id layer
//! ([`fill_request_id`]) re-renders it with the id. Token groups render their own formats.

use axum::Json;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

use mage_core::wire::api::{ApiError, ApiErrorBody, ApiErrorCode};

use crate::error::AppError;

/// Seconds clients wait before retrying a 503.
const UNAVAILABLE_RETRY_AFTER: &str = "5";

/// Marker left on error responses rendered by [`api_error`].
#[derive(Debug, Clone)]
pub struct RenderedApiError(pub ApiError);

pub fn api_code(e: &AppError) -> ApiErrorCode {
    match e {
        AppError::NotFound(_) => ApiErrorCode::NotFound,
        AppError::Conflict(_) => ApiErrorCode::Conflict,
        AppError::Invalid(_) | AppError::Domain(_) => ApiErrorCode::Invalid,
        AppError::Unauthorized => ApiErrorCode::Unauthenticated,
        AppError::Forbidden(_) => ApiErrorCode::Forbidden,
        AppError::RateLimited { .. } => ApiErrorCode::RateLimited,
        AppError::Unavailable(_) => ApiErrorCode::Unavailable,
        AppError::Internal(_) => ApiErrorCode::Internal,
    }
}

/// Client-facing message: detail for client errors, generic text for server-side failures
/// (their detail goes to the log only).
fn message(e: &AppError) -> String {
    match e {
        AppError::NotFound(_) => "not found".into(),
        AppError::Unauthorized => "authentication required".into(),
        AppError::RateLimited { .. } => "too many requests".into(),
        AppError::Unavailable(_) => "service temporarily unavailable".into(),
        AppError::Internal(_) => "internal error".into(),
        AppError::Conflict(m) | AppError::Invalid(m) | AppError::Forbidden(m) => m.clone(),
        AppError::Domain(d) => d.to_string(),
    }
}

/// An API error response with an explicit code and message.
pub fn api_error(code: ApiErrorCode, message: impl Into<String>) -> Response {
    let error = ApiError { code, message: message.into(), request_id: String::new() };
    let status = StatusCode::from_u16(code.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut response = (status, Json(ApiErrorBody { error: error.clone() })).into_response();
    response.extensions_mut().insert(RenderedApiError(error));
    response
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        match &self {
            AppError::Internal(detail) => tracing::error!(%detail, "internal error"),
            AppError::Unavailable(detail) => tracing::warn!(%detail, "dependency unavailable"),
            _ => tracing::debug!(error = %self, "request refused"),
        }
        let mut response = api_error(api_code(&self), message(&self));
        let retry_after = match &self {
            AppError::RateLimited { retry_after_secs: Some(secs) } => HeaderValue::from_str(&secs.to_string()).ok(),
            AppError::Unavailable(_) => Some(HeaderValue::from_static(UNAVAILABLE_RETRY_AFTER)),
            _ => None,
        };
        if let Some(value) = retry_after {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        response
    }
}

/// Re-renders an error body produced by [`api_error`] with the request's id.
pub fn fill_request_id(mut response: Response, request_id: &str) -> Response {
    let Some(RenderedApiError(mut error)) = response.extensions_mut().remove::<RenderedApiError>() else {
        return response;
    };
    error.request_id = request_id.to_owned();
    let (mut parts, _) = response.into_parts();
    let body = serde_json::to_vec(&ApiErrorBody { error }).unwrap_or_default();
    parts.headers.remove(header::CONTENT_LENGTH);
    Response::from_parts(parts, axum::body::Body::from(body))
}

/// `Json<T>` whose rejections use the API error format (422, or 413 over the body limit).
pub struct JsonBody<T>(pub T);

impl<S: Send + Sync, T: serde::de::DeserializeOwned> axum::extract::FromRequest<S> for JsonBody<T> {
    type Rejection = Response;

    async fn from_request(req: axum::extract::Request, state: &S) -> Result<Self, Self::Rejection> {
        use axum::extract::rejection::JsonRejection;
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(JsonBody(value)),
            Err(JsonRejection::BytesRejection(r)) if r.status() == StatusCode::PAYLOAD_TOO_LARGE => {
                Err(api_error(ApiErrorCode::TooLarge, "request body too large"))
            }
            Err(r) => Err(api_error(ApiErrorCode::Invalid, r.body_text())),
        }
    }
}

/// `Query<T>` whose rejections use the API error format (422).
pub struct QueryParams<T>(pub T);

impl<S: Send + Sync, T: serde::de::DeserializeOwned> axum::extract::FromRequestParts<S> for QueryParams<T> {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut axum::http::request::Parts, state: &S) -> Result<Self, Self::Rejection> {
        match axum::extract::Query::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Query(value)) => Ok(QueryParams(value)),
            Err(r) => Err(api_error(ApiErrorCode::Invalid, r.body_text())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn body(response: Response) -> ApiErrorBody {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("body");
        serde_json::from_slice(&bytes).expect("json")
    }

    #[tokio::test]
    async fn errors_map_to_status_code_and_safe_messages() {
        let cases = [
            (AppError::NotFound("ticket tkt_x".into()), 404, ApiErrorCode::NotFound, "not found"),
            (AppError::Unauthorized, 401, ApiErrorCode::Unauthenticated, "authentication required"),
            (AppError::Forbidden("needs admin".into()), 403, ApiErrorCode::Forbidden, "needs admin"),
            (AppError::Conflict("taken".into()), 409, ApiErrorCode::Conflict, "taken"),
            (AppError::Invalid("bad".into()), 422, ApiErrorCode::Invalid, "bad"),
            (AppError::Internal("db password=hunter2".into()), 500, ApiErrorCode::Internal, "internal error"),
            (AppError::Unavailable("pool timeout at 10.0.0.3".into()), 503, ApiErrorCode::Unavailable, "service temporarily unavailable"),
        ];
        for (error, status, code, message) in cases {
            let response = error.into_response();
            assert_eq!(response.status().as_u16(), status);
            let response = fill_request_id(response, "req-1");
            let body = body(response).await;
            assert_eq!((body.error.code, body.error.message.as_str(), body.error.request_id.as_str()), (code, message, "req-1"));
        }
    }

    #[tokio::test]
    async fn retry_after_is_set_for_rate_limits_and_unavailability() {
        let r = AppError::RateLimited { retry_after_secs: Some(12) }.into_response();
        assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(r.headers().get(header::RETRY_AFTER).map(|v| v.as_bytes()), Some(&b"12"[..]));
        let r = AppError::Unavailable("x".into()).into_response();
        assert!(r.headers().contains_key(header::RETRY_AFTER));
    }
}
