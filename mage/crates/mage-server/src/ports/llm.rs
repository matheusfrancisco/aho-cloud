//! `ModelUpstream` port (server.md "`ModelUpstream` (owned)", "Upstream adapters"): forwards one
//! Anthropic Messages request with the tenant credential (BYOK) or Mage's Vertex project. Also the
//! org model settings the gateway enforces (`OrgStore::model_settings`, storage.md).

use std::collections::BTreeMap;
use std::fmt;
use std::pin::Pin;

use async_trait::async_trait;
use bytes::Bytes;
use futures::Stream;
use serde::{Deserialize, Serialize};

use mage_core::wire::api::ModelRouteKind;

use crate::error::AppError;
use crate::ports::vault::SecretBytes;

/// Anthropic model id, e.g. `claude-sonnet-4-5`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModelId(pub String);

impl ModelId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ModelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Which Anthropic endpoint a request targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UpstreamOp {
    /// `/v1/messages`.
    Messages,
    /// `/v1/messages/count_tokens`.
    CountTokens,
}

/// Where a request goes and with which credential.
#[derive(Clone)]
pub enum ModelRoute {
    /// BYOK or the operator's own key, loaded from `SecretVault` by the gateway use case.
    AnthropicApi { api_key: SecretBytes },
    /// Mage's GCP project; the credential is the adapter's ADC (service account).
    Vertex {
        project: String,
        region: String,
        /// Vertex model id from the org's `model_map` (e.g. `claude-sonnet-4-5@20250929`).
        model: String,
    },
}

impl ModelRoute {
    pub fn kind(&self) -> ModelRouteKind {
        match self {
            ModelRoute::AnthropicApi { .. } => ModelRouteKind::Anthropic,
            ModelRoute::Vertex { .. } => ModelRouteKind::Vertex,
        }
    }
}

/// Never prints the key.
impl fmt::Debug for ModelRoute {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ModelRoute::AnthropicApi { .. } => f.debug_struct("AnthropicApi").field("api_key", &"<redacted>").finish(),
            ModelRoute::Vertex { project, region, model } => {
                f.debug_struct("Vertex").field("project", project).field("region", region).field("model", model).finish()
            }
        }
    }
}

/// One request to forward. `body` is the client JSON as received; the adapter only rewrites it
/// where the route requires (Vertex).
#[derive(Debug, Clone)]
pub struct UpstreamRequest {
    pub op: UpstreamOp,
    pub route: ModelRoute,
    /// The client asked for SSE (`"stream": true`); a 2xx answer then comes back as
    /// [`UpstreamBody::Stream`].
    pub stream: bool,
    /// `anthropic-version` header; the adapter applies the API default when `None`.
    pub anthropic_version: Option<String>,
    /// `anthropic-beta` values, already filtered for the route by the gateway.
    pub anthropic_beta: Vec<String>,
    pub body: Bytes,
}

/// Upstream status and the allowlisted response headers (lowercase names).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamHead {
    pub status: u16,
    pub headers: Vec<(String, String)>,
}

impl UpstreamHead {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, UpstreamError>> + Send>>;

pub enum UpstreamBody {
    Full(Bytes),
    /// Chunks exactly as the upstream sent them.
    Stream(ByteStream),
}

impl fmt::Debug for UpstreamBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UpstreamBody::Full(b) => f.debug_tuple("Full").field(&b.len()).finish(),
            UpstreamBody::Stream(_) => f.write_str("Stream"),
        }
    }
}

#[derive(Debug)]
pub struct UpstreamResponse {
    pub head: UpstreamHead,
    pub body: UpstreamBody,
}

#[async_trait]
pub trait ModelUpstream: Send + Sync {
    /// Non-2xx upstream responses are `Ok(UpstreamResponse)` and passed through;
    /// `Err` only for transport/protocol failures.
    async fn send(&self, req: UpstreamRequest) -> Result<UpstreamResponse, UpstreamError>;
    fn supports(&self, route: &ModelRoute, op: UpstreamOp) -> bool;
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UpstreamError {
    /// The request cannot be sent as given (e.g. Vertex rewrite of a non-object body).
    #[error("invalid upstream request: {0}")]
    Invalid(String),
    /// Connect failure, route not configured, credential source down.
    #[error("upstream unavailable: {0}")]
    Unavailable(String),
    /// Non-stream total timeout or stream idle timeout.
    #[error("upstream timed out")]
    Timeout,
    /// The upstream broke the HTTP exchange (reset mid-body, undecodable response).
    #[error("upstream protocol error: {0}")]
    Protocol(String),
}

impl From<UpstreamError> for AppError {
    fn from(e: UpstreamError) -> Self {
        match e {
            UpstreamError::Invalid(m) => AppError::Invalid(m),
            UpstreamError::Unavailable(m) => AppError::Unavailable(m),
            UpstreamError::Timeout => AppError::Unavailable("model upstream timed out".into()),
            UpstreamError::Protocol(m) => AppError::Unavailable(format!("model upstream protocol error: {m}")),
        }
    }
}

/// Org model settings (`org_settings` area `model`, jsonb; server.md "Additions": `OrgStore` org
/// model settings `{route, allowed_models, model_map, max_tokens_cap}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSettings {
    pub route: ModelRouteKind,
    /// Anthropic model ids the gateway accepts for this org (empty = none).
    pub allowed_models: Vec<String>,
    /// Anthropic id → Vertex id (`name@date`); used only on the Vertex route. No string munging.
    #[serde(default)]
    pub model_map: BTreeMap<String, String>,
    /// Upper bound for a request's `max_tokens` (`None` = no org cap).
    #[serde(default)]
    pub max_tokens_cap: Option<u32>,
}

/// A fresh org: Anthropic route (BYOK), no models allowed until an Owner configures them.
impl Default for ModelSettings {
    fn default() -> Self {
        ModelSettings { route: ModelRouteKind::Anthropic, allowed_models: Vec::new(), model_map: BTreeMap::new(), max_tokens_cap: None }
    }
}
