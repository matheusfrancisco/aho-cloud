//! Model gateway routes (server.md "MCP, model gateway, capabilities", "Request handling" step 1,
//! "Error formats"), nested by the shell at `/llm/anthropic`:
//!
//! | Method | Path | |
//! |---|---|---|
//! | POST | `/v1/messages` | Anthropic Messages, streaming or not |
//! | POST | `/v1/messages/count_tokens` | 404 `not_found_error` if the route's upstream can't serve it |
//! | any | anything else | 404 `not_found_error` |
//!
//! Credential: the `gateway_token` as `x-api-key` or `Authorization: Bearer`; both present and
//! different → 401. Cookies and sessions are never consulted. Body limit 32 MiB; no handler
//! timeout (streams run until the upstream ends, or until server shutdown cuts them so graceful
//! shutdown can finish; the partial call is still metered). Errors use the Anthropic shape so
//! harnesses parse them: `{"type":"error","error":{"type":"…","message":"mage: …"}}`.

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::rejection::BytesRejection;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE, RETRY_AFTER};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::app::gateway::{Forward, GatewayBody, GatewayError, GatewayRequest, GatewayResponse};
use crate::ports::llm::UpstreamOp;

/// Cloud Run's HTTP/1 request limit (server.md "Request pipeline").
pub const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone)]
pub struct GatewayState {
    pub forward: Arc<Forward>,
    /// Server shutdown: ends open model streams.
    pub shutdown: CancellationToken,
}

impl GatewayState {
    pub fn new(forward: Arc<Forward>, shutdown: CancellationToken) -> GatewayState {
        GatewayState { forward, shutdown }
    }
}

/// Router for `/llm/anthropic` (paths relative to that prefix; the shell nests it).
pub fn router(state: GatewayState) -> Router {
    Router::new()
        .route("/v1/messages", post(messages).fallback(not_found))
        .route("/v1/messages/count_tokens", post(count_tokens).fallback(not_found))
        .fallback(not_found)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
}

async fn messages(State(state): State<GatewayState>, headers: HeaderMap, body: Result<Bytes, BytesRejection>) -> Response {
    handle(&state, UpstreamOp::Messages, &headers, body).await
}

async fn count_tokens(State(state): State<GatewayState>, headers: HeaderMap, body: Result<Bytes, BytesRejection>) -> Response {
    handle(&state, UpstreamOp::CountTokens, &headers, body).await
}

async fn not_found() -> Response {
    error_response(&GatewayError::NotFound("no such gateway route".into()))
}

async fn handle(state: &GatewayState, op: UpstreamOp, headers: &HeaderMap, body: Result<Bytes, BytesRejection>) -> Response {
    let request = match gateway_request(op, headers, body) {
        Ok(request) => request,
        Err(e) => return error_response(&e),
    };
    match state.forward.execute(request).await {
        Ok(response) => into_response(response, &state.shutdown),
        Err(e) => error_response(&e),
    }
}

fn gateway_request(op: UpstreamOp, headers: &HeaderMap, body: Result<Bytes, BytesRejection>) -> Result<GatewayRequest, GatewayError> {
    let token = gateway_token(headers)?.to_owned();
    let body = body.map_err(|rejection| {
        if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
            GatewayError::TooLarge(format!("request body exceeds {MAX_BODY_BYTES} bytes"))
        } else {
            GatewayError::Invalid(format!("request body: {}", rejection.body_text()))
        }
    })?;
    let anthropic_version = match headers.get("anthropic-version") {
        Some(v) => Some(v.to_str().map_err(|_| GatewayError::Invalid("anthropic-version is not ASCII".into()))?.to_owned()),
        None => None,
    };
    let mut anthropic_beta = Vec::new();
    for value in headers.get_all("anthropic-beta") {
        let value = value.to_str().map_err(|_| GatewayError::Invalid("anthropic-beta is not ASCII".into()))?;
        anthropic_beta.extend(value.split(',').map(str::trim).filter(|b| !b.is_empty()).map(str::to_owned));
    }
    let request_id = headers.get("x-request-id").and_then(|v| v.to_str().ok()).map(str::to_owned);
    Ok(GatewayRequest { op, token, anthropic_version, anthropic_beta, body, request_id })
}

/// Step 1: `x-api-key` or `Authorization: Bearer`; both present and different → 401.
fn gateway_token(headers: &HeaderMap) -> Result<&str, GatewayError> {
    let unauthenticated = |m: &str| GatewayError::Unauthenticated(m.to_owned());
    let api_key = match headers.get("x-api-key") {
        Some(v) => Some(v.to_str().map_err(|_| unauthenticated("x-api-key is not a valid token"))?.trim()),
        None => None,
    };
    let bearer = match headers.get(AUTHORIZATION) {
        Some(v) => {
            let v = v.to_str().map_err(|_| unauthenticated("authorization is not a valid bearer token"))?;
            let (scheme, token) = v.split_once(' ').ok_or_else(|| unauthenticated("authorization must use the Bearer scheme"))?;
            if !scheme.eq_ignore_ascii_case("bearer") {
                return Err(unauthenticated("authorization must use the Bearer scheme"));
            }
            Some(token.trim())
        }
        None => None,
    };
    match (api_key, bearer) {
        (Some(key), Some(bearer)) if key != bearer => Err(unauthenticated("x-api-key and authorization carry different credentials")),
        (Some(token), _) | (None, Some(token)) if !token.is_empty() => Ok(token),
        _ => Err(unauthenticated("missing gateway token")),
    }
}

fn into_response(response: GatewayResponse, shutdown: &CancellationToken) -> Response {
    let status = StatusCode::from_u16(response.head.status).unwrap_or(StatusCode::BAD_GATEWAY);
    let body = match response.body {
        GatewayBody::Full(bytes) => Body::from(bytes),
        // Chunks pass through as received; shutdown ends the stream (dropping it settles usage).
        GatewayBody::Stream(stream) => Body::from_stream(stream.take_until(shutdown.clone().cancelled_owned())),
    };
    let mut out = Response::new(body);
    *out.status_mut() = status;
    let headers = out.headers_mut();
    for (name, value) in response.head.headers {
        if let (Ok(name), Ok(value)) = (HeaderName::try_from(name), HeaderValue::try_from(value)) {
            headers.append(name, value);
        }
    }
    out
}

/// Anthropic-shaped error (server.md "Error formats", `/llm/anthropic` column).
fn error_response(err: &GatewayError) -> Response {
    let (status, kind) = match err {
        GatewayError::Unauthenticated(_) => (StatusCode::UNAUTHORIZED, "authentication_error"),
        GatewayError::Forbidden(_) => (StatusCode::FORBIDDEN, "permission_error"),
        GatewayError::Invalid(_) => (StatusCode::BAD_REQUEST, "invalid_request_error"),
        GatewayError::NotFound(_) => (StatusCode::NOT_FOUND, "not_found_error"),
        GatewayError::Conflict(_) => (StatusCode::CONFLICT, "invalid_request_error"),
        GatewayError::TooLarge(_) => (StatusCode::PAYLOAD_TOO_LARGE, "request_too_large"),
        GatewayError::RateLimited { .. } => (StatusCode::TOO_MANY_REQUESTS, "rate_limit_error"),
        GatewayError::Unavailable(_) => (StatusCode::SERVICE_UNAVAILABLE, "api_error"),
        GatewayError::Timeout => (StatusCode::GATEWAY_TIMEOUT, "timeout_error"),
        GatewayError::BadGateway(_) => (StatusCode::BAD_GATEWAY, "api_error"),
        GatewayError::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "api_error"),
    };
    let body = serde_json::json!({ "type": "error", "error": { "type": kind, "message": format!("mage: {err}") } });
    let mut response = (status, [(CONTENT_TYPE, "application/json")], body.to_string()).into_response();
    if let GatewayError::RateLimited { retry_after_secs } = err {
        response.headers_mut().insert(RETRY_AFTER, HeaderValue::from(*retry_after_secs));
    }
    response
}

#[cfg(test)]
mod tests;
