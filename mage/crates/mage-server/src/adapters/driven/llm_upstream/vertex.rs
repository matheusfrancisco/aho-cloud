//! Vertex AI (Anthropic publisher models) with Mage's ADC token.
//!
//! - URL: `https://{region}-aiplatform.googleapis.com/v1/projects/{project}/locations/{region}/
//!   publishers/anthropic/models/{model}:streamRawPredict` (stream) or `:rawPredict`; region
//!   `global` uses host `aiplatform.googleapis.com` [unverified].
//! - Body: top-level rewrite only: drop `model`, set `"anthropic_version": "vertex-2023-10-16"`;
//!   nested values stay raw bytes (`BTreeMap<String, Box<RawValue>>`).
//! - Betas: forwarded as `anthropic-beta`; the gateway has already filtered them to the
//!   operator's Vertex allowlist.
//! - `count_tokens`: not served (`count-tokens:rawPredict` is [unverified]), so `supports` is false
//!   and the gateway answers 404 `not_found_error` (Claude Code falls back).

use std::collections::BTreeMap;

use bytes::Bytes;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue};
use serde_json::value::RawValue;
use url::Url;

use super::{AdcTokenSource, UpstreamTimeouts, execute, http_client};
use crate::ports::llm::{ModelRoute, UpstreamError, UpstreamOp, UpstreamRequest, UpstreamResponse};

/// Body `anthropic_version` Vertex requires.
pub const VERTEX_ANTHROPIC_VERSION: &str = "vertex-2023-10-16";

pub struct VertexUpstream {
    client: reqwest::Client,
    tokens: AdcTokenSource,
    /// Replaces `https://{region}-aiplatform.googleapis.com` (tests, private endpoints).
    endpoint: Option<Url>,
    timeouts: UpstreamTimeouts,
}

impl VertexUpstream {
    pub fn new(tokens: AdcTokenSource) -> Result<VertexUpstream, UpstreamError> {
        let timeouts = UpstreamTimeouts::default();
        Ok(VertexUpstream { client: http_client(&timeouts)?, tokens, endpoint: None, timeouts })
    }

    /// Sends to `endpoint` (scheme + host + optional path prefix) instead of the regional host.
    pub fn with_endpoint(tokens: AdcTokenSource, endpoint: Url, timeouts: UpstreamTimeouts) -> Result<VertexUpstream, UpstreamError> {
        if endpoint.cannot_be_a_base() {
            return Err(UpstreamError::Invalid(format!("vertex endpoint must be a base url: {endpoint}")));
        }
        Ok(VertexUpstream { client: http_client(&timeouts)?, tokens, endpoint: Some(endpoint), timeouts })
    }

    pub fn supports(op: UpstreamOp) -> bool {
        matches!(op, UpstreamOp::Messages)
    }

    pub async fn send(&self, req: UpstreamRequest) -> Result<UpstreamResponse, UpstreamError> {
        let ModelRoute::Vertex { project, region, model } = &req.route else {
            return Err(UpstreamError::Invalid("the Vertex adapter only serves the Vertex route".into()));
        };
        if req.op != UpstreamOp::Messages {
            return Err(UpstreamError::Invalid("Vertex serves only the Messages operation".into()));
        }
        let url = self.url(project, region, model, req.stream)?;
        let body = rewrite_body(&req.body)?;
        let token = self.tokens.access_token().await.map_err(|e| UpstreamError::Unavailable(e.to_string()))?;
        let mut bearer = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| UpstreamError::Unavailable("ADC returned an access token that is not a header value".into()))?;
        bearer.set_sensitive(true);
        let mut request = self.client.post(url).header(CONTENT_TYPE, "application/json").header(AUTHORIZATION, bearer);
        if !req.anthropic_beta.is_empty() {
            request = request.header("anthropic-beta", req.anthropic_beta.join(","));
        }
        execute(request.body(body), req.stream, &self.timeouts).await
    }

    fn url(&self, project: &str, region: &str, model: &str, stream: bool) -> Result<Url, UpstreamError> {
        check_segment("project", project, |b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')?;
        check_segment("region", region, |b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')?;
        // Model ids come from org settings (`model_map`): keep them to a path-safe charset.
        check_segment("model", model, |b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'@'))?;
        let method = if stream { "streamRawPredict" } else { "rawPredict" };
        let path = format!("v1/projects/{project}/locations/{region}/publishers/anthropic/models/{model}:{method}");
        let base = match &self.endpoint {
            Some(endpoint) => {
                let mut base = endpoint.clone();
                if !base.path().ends_with('/') {
                    base.set_path(&format!("{}/", base.path()));
                }
                base
            }
            None if region == "global" => {
                Url::parse("https://aiplatform.googleapis.com/").map_err(|e| UpstreamError::Invalid(e.to_string()))?
            }
            None => {
                Url::parse(&format!("https://{region}-aiplatform.googleapis.com/")).map_err(|e| UpstreamError::Invalid(e.to_string()))?
            }
        };
        base.join(&path).map_err(|e| UpstreamError::Invalid(format!("vertex url: {e}")))
    }
}

fn check_segment(what: &str, value: &str, allowed: impl Fn(u8) -> bool) -> Result<(), UpstreamError> {
    if value.is_empty() || value.len() > 128 || !value.bytes().all(allowed) {
        return Err(UpstreamError::Invalid(format!("vertex {what} {value:?} has characters outside the allowed set")));
    }
    Ok(())
}

/// Top-level rewrite for Vertex: remove `model`, set `anthropic_version`. Nested values are kept
/// as the client's raw bytes.
pub fn rewrite_body(body: &[u8]) -> Result<Bytes, UpstreamError> {
    let mut top: BTreeMap<String, Box<RawValue>> =
        serde_json::from_slice(body).map_err(|e| UpstreamError::Invalid(format!("request body must be a JSON object: {e}")))?;
    top.remove("model");
    let version = RawValue::from_string(format!("\"{VERTEX_ANTHROPIC_VERSION}\"")).map_err(|e| UpstreamError::Invalid(e.to_string()))?;
    top.insert("anthropic_version".to_owned(), version);
    serde_json::to_vec(&top).map(Bytes::from).map_err(|e| UpstreamError::Invalid(e.to_string()))
}
