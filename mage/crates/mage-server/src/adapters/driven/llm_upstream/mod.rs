//! `ModelUpstream` adapters (server.md "Upstream adapters", "Model upstreams"): the Anthropic API
//! with the tenant key, and Vertex AI with Mage's ADC. [`RoutedUpstream`] dispatches on
//! [`ModelRoute`].
//!
//! HTTP client per adapter: rustls, pooled, connect timeout 10 s, non-stream total 10 min, stream
//! idle 5 min (also bounds the wait for the response head of a streaming request). Upstream
//! requests carry only `content-type`, the API version, betas and the adapter's credential; the
//! response keeps only allowlisted headers. The adapter never retries.

pub mod adc;
pub mod anthropic;
pub mod vertex;

use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::header::HeaderMap;

use crate::ports::llm::{
    ModelRoute, ModelUpstream, UpstreamBody, UpstreamError, UpstreamHead, UpstreamOp, UpstreamRequest, UpstreamResponse,
};

pub use adc::{AdcError, AdcTokenSource};
pub use anthropic::AnthropicApi;
pub use vertex::VertexUpstream;

/// Dispatches on [`ModelRoute`]; Vertex is optional (operator-enabled).
pub struct RoutedUpstream {
    anthropic: AnthropicApi,
    vertex: Option<VertexUpstream>,
}

impl RoutedUpstream {
    pub fn new(anthropic: AnthropicApi, vertex: Option<VertexUpstream>) -> RoutedUpstream {
        RoutedUpstream { anthropic, vertex }
    }
}

#[async_trait]
impl ModelUpstream for RoutedUpstream {
    async fn send(&self, req: UpstreamRequest) -> Result<UpstreamResponse, UpstreamError> {
        match &req.route {
            ModelRoute::AnthropicApi { .. } => self.anthropic.send(req).await,
            ModelRoute::Vertex { .. } => match &self.vertex {
                Some(vertex) => vertex.send(req).await,
                None => Err(UpstreamError::Unavailable("the Vertex route is not configured on this server".into())),
            },
        }
    }

    fn supports(&self, route: &ModelRoute, op: UpstreamOp) -> bool {
        match route {
            ModelRoute::AnthropicApi { .. } => AnthropicApi::supports(op),
            ModelRoute::Vertex { .. } => self.vertex.is_some() && VertexUpstream::supports(op),
        }
    }
}

/// Timeouts of one upstream adapter (server.md "Upstream adapters").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpstreamTimeouts {
    pub connect: Duration,
    /// Whole exchange of a non-streaming request, body included.
    pub total: Duration,
    /// Longest silence while streaming, and the longest wait for a streaming response's head.
    pub stream_idle: Duration,
}

impl Default for UpstreamTimeouts {
    fn default() -> Self {
        UpstreamTimeouts { connect: Duration::from_secs(10), total: Duration::from_secs(600), stream_idle: Duration::from_secs(300) }
    }
}

/// Response headers passed back to the harness (server.md "Request handling" step 8).
pub(crate) fn allowlisted_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter(|(name, _)| {
            let name = name.as_str();
            matches!(name, "content-type" | "request-id" | "retry-after") || name.starts_with("anthropic-ratelimit-")
        })
        .filter_map(|(name, value)| value.to_str().ok().map(|v| (name.as_str().to_owned(), v.to_owned())))
        .collect()
}

pub(crate) fn http_client(timeouts: &UpstreamTimeouts) -> Result<reqwest::Client, UpstreamError> {
    reqwest::Client::builder()
        .connect_timeout(timeouts.connect)
        .pool_idle_timeout(Duration::from_secs(90))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| UpstreamError::Unavailable(format!("http client: {e}")))
}

pub(crate) fn map_reqwest_error(e: reqwest::Error) -> UpstreamError {
    if e.is_timeout() {
        UpstreamError::Timeout
    } else if e.is_connect() {
        UpstreamError::Unavailable(format!("connect: {e}"))
    } else if e.is_builder() {
        UpstreamError::Invalid(e.to_string())
    } else if e.is_body() || e.is_decode() {
        UpstreamError::Protocol(e.to_string())
    } else {
        UpstreamError::Unavailable(e.to_string())
    }
}

/// Sends a prepared request. A 2xx answer to a streaming request comes back as a stream of the
/// upstream's chunks, unbuffered; everything else is read in full.
pub(crate) async fn execute(
    request: reqwest::RequestBuilder,
    stream: bool,
    timeouts: &UpstreamTimeouts,
) -> Result<UpstreamResponse, UpstreamError> {
    let response = if stream {
        tokio::time::timeout(timeouts.stream_idle, request.send()).await.map_err(|_| UpstreamError::Timeout)?
    } else {
        request.timeout(timeouts.total).send().await
    }
    .map_err(map_reqwest_error)?;
    let head = UpstreamHead { status: response.status().as_u16(), headers: allowlisted_headers(response.headers()) };
    if stream && response.status().is_success() {
        let idle = timeouts.stream_idle;
        let chunks = Box::pin(response.bytes_stream());
        let body = futures::stream::unfold(Some(chunks), move |state| async move {
            let mut chunks = state?;
            match tokio::time::timeout(idle, chunks.next()).await {
                Ok(Some(Ok(bytes))) => Some((Ok(bytes), Some(chunks))),
                Ok(Some(Err(e))) => Some((Err(map_reqwest_error(e)), None)),
                Ok(None) => None,
                Err(_) => Some((Err(UpstreamError::Timeout), None)),
            }
        });
        return Ok(UpstreamResponse { head, body: UpstreamBody::Stream(Box::pin(body)) });
    }
    let body = if stream {
        // Error answer to a streaming request: no request-level timeout was set, bound it here.
        tokio::time::timeout(timeouts.total, response.bytes()).await.map_err(|_| UpstreamError::Timeout)?
    } else {
        response.bytes().await
    }
    .map_err(map_reqwest_error)?;
    Ok(UpstreamResponse { head, body: UpstreamBody::Full(body) })
}

#[cfg(test)]
mod tests;
