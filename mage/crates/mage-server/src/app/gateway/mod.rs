//! Model gateway (server.md "Model gateway"): use case [`Forward`] behind
//! `/llm/anthropic/v1/messages` (+ `count_tokens`). Verifies the run-bound `gateway_token`,
//! enforces run state, budget and the org's model policy, picks the route (BYOK Anthropic key or
//! Mage's Vertex project), forwards the client bytes, meters usage from the response and reports
//! it to the run manager (`runs::RecordUsage`).

mod forward;
mod usage;

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde::Deserialize;
use url::Url;

use mage_core::domain::{OrgId, RunError, RunId, Usage};

use crate::app::runs::{FailRun, RecordUsage, RunManager, UsageVerdict};
use crate::error::AppError;
use crate::ports::llm::UpstreamError;

pub use forward::{Forward, GatewayBody, GatewayDeps, GatewayRequest, GatewayResponse, MeteredStream};
pub use usage::{MAX_LINE, ObservedUsage, UsageTap};

/// `SecretVault` name of the org's Anthropic key (`Org` scope; server.md "Secret inventory").
pub const BYOK_KEY_SECRET: &str = "model/anthropic_api_key";

/// `[server.gateway]` (server.md "Config").
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayConfig {
    /// `harness-login` (dev only): the run manager issues no `gateway_token`.
    #[serde(default)]
    pub mode: GatewayMode,
    #[serde(default = "default_anthropic_base_url")]
    pub anthropic_base_url: Url,
    /// Mage's own Vertex project, offered to the listed orgs only.
    #[serde(default)]
    pub vertex: Option<VertexRouteConfig>,
    /// Concurrent model calls per run on one instance; more → 429.
    #[serde(default = "default_per_run_in_flight")]
    pub per_run_in_flight: u32,
    /// Betas forwarded on the Vertex route; others are dropped silently.
    #[serde(default)]
    pub beta_allowlist_vertex: Vec<String>,
    /// Anthropic model id → price. Every allowed model needs an entry.
    #[serde(default)]
    pub pricing: BTreeMap<String, ModelPrice>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GatewayMode {
    #[default]
    Gateway,
    HarnessLogin,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VertexRouteConfig {
    pub project: String,
    pub region: String,
    /// Org slugs allowed to use the Vertex route.
    #[serde(default)]
    pub enabled_orgs: Vec<String>,
}

/// Micros (USD × 10⁶) per million tokens; operator-maintained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPrice {
    pub input: u64,
    pub output: u64,
    pub cache_write: u64,
    pub cache_read: u64,
}

impl ModelPrice {
    /// Σ field × price, rounded up to a whole micro.
    pub fn cost_micros(&self, usage: &ObservedUsage) -> u64 {
        let total = u128::from(usage.input_tokens) * u128::from(self.input)
            + u128::from(usage.cache_creation_input_tokens) * u128::from(self.cache_write)
            + u128::from(usage.cache_read_input_tokens) * u128::from(self.cache_read)
            + u128::from(usage.output_tokens) * u128::from(self.output);
        u64::try_from(total.div_ceil(1_000_000)).unwrap_or(u64::MAX)
    }
}

fn default_anthropic_base_url() -> Url {
    Url::parse("https://api.anthropic.com").unwrap_or_else(|_| unreachable!("constant url parses"))
}

fn default_per_run_in_flight() -> u32 {
    4
}

impl Default for GatewayConfig {
    fn default() -> Self {
        GatewayConfig {
            mode: GatewayMode::Gateway,
            anthropic_base_url: default_anthropic_base_url(),
            vertex: None,
            per_run_in_flight: default_per_run_in_flight(),
            beta_allowlist_vertex: Vec::new(),
            pricing: BTreeMap::new(),
        }
    }
}

impl GatewayConfig {
    /// Load-time checks; `Forward::new` calls it.
    pub fn validate(&self) -> Result<(), AppError> {
        if !matches!(self.anthropic_base_url.scheme(), "https" | "http") {
            return Err(AppError::Invalid("gateway.anthropic_base_url must be http(s)".into()));
        }
        if self.per_run_in_flight == 0 {
            return Err(AppError::Invalid("gateway.per_run_in_flight must be at least 1".into()));
        }
        if self.pricing.keys().any(|model| model.trim().is_empty()) {
            return Err(AppError::Invalid("gateway.pricing has an empty model id".into()));
        }
        if let Some(vertex) = &self.vertex {
            let ok = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
            if !ok(&vertex.project) || !ok(&vertex.region) {
                return Err(AppError::Invalid("gateway.vertex project and region must match [a-z0-9-]+".into()));
            }
        }
        Ok(())
    }

    /// The operator offers the Vertex route to this org.
    pub fn vertex_enabled_for(&self, org_slug: &str) -> bool {
        self.vertex.as_ref().is_some_and(|v| v.enabled_orgs.iter().any(|s| s == org_slug))
    }

    pub fn price(&self, model: &str) -> Option<&ModelPrice> {
        self.pricing.get(model)
    }
}

/// Gateway failures; the HTTP adapter renders them as Anthropic errors
/// (`{"type":"error","error":{"type":…,"message":"mage: …"}}`, server.md "Error formats").
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GatewayError {
    /// 401 `authentication_error`.
    #[error("{0}")]
    Unauthenticated(String),
    /// 403 `permission_error` (budget, missing or rejected key).
    #[error("{0}")]
    Forbidden(String),
    /// 400 `invalid_request_error`.
    #[error("{0}")]
    Invalid(String),
    /// 404 `not_found_error`.
    #[error("{0}")]
    NotFound(String),
    /// 409 `invalid_request_error`.
    #[error("{0}")]
    Conflict(String),
    /// 413 `request_too_large`.
    #[error("{0}")]
    TooLarge(String),
    /// 429 `rate_limit_error` with `retry-after`.
    #[error("too many concurrent model calls for this run")]
    RateLimited { retry_after_secs: u64 },
    /// 503 `api_error`.
    #[error("{0}")]
    Unavailable(String),
    /// 504 `timeout_error`.
    #[error("model upstream timed out")]
    Timeout,
    /// 502 `api_error`: the upstream broke the exchange.
    #[error("{0}")]
    BadGateway(String),
    /// 500 `api_error`.
    #[error("{0}")]
    Internal(String),
}

impl From<AppError> for GatewayError {
    fn from(e: AppError) -> Self {
        match e {
            AppError::NotFound(m) => GatewayError::NotFound(m),
            AppError::Conflict(m) => GatewayError::Conflict(m),
            AppError::Invalid(m) => GatewayError::Invalid(m),
            AppError::Unauthorized => GatewayError::Unauthenticated("unauthenticated".into()),
            AppError::Forbidden(m) => GatewayError::Forbidden(m),
            AppError::RateLimited { retry_after_secs } => GatewayError::RateLimited { retry_after_secs: retry_after_secs.unwrap_or(1) },
            AppError::Unavailable(m) => GatewayError::Unavailable(m),
            AppError::Domain(e) => GatewayError::Invalid(e.to_string()),
            AppError::Internal(m) => GatewayError::Internal(m),
        }
    }
}

impl From<UpstreamError> for GatewayError {
    fn from(e: UpstreamError) -> Self {
        match e {
            UpstreamError::Invalid(m) => GatewayError::Invalid(m),
            UpstreamError::Unavailable(m) => GatewayError::Unavailable(format!("model upstream unavailable: {m}")),
            UpstreamError::Timeout => GatewayError::Timeout,
            UpstreamError::Protocol(m) => GatewayError::BadGateway(format!("model upstream protocol error: {m}")),
        }
    }
}

/// The two run-manager operations the gateway calls (runs.md "RecordUsage", "FailRun").
#[async_trait]
pub trait RunMeter: Send + Sync {
    /// Adds a metered delta; `Deny` when the run is terminal or now over budget.
    async fn record_usage(&self, org: OrgId, run: RunId, delta: Usage) -> Result<UsageVerdict, AppError>;
    /// Ends a non-terminal run as `Errored` (no-op if terminal).
    async fn fail_run(&self, org: OrgId, run: RunId, error: RunError) -> Result<(), AppError>;
}

#[async_trait]
impl RunMeter for RunManager {
    async fn record_usage(&self, org: OrgId, run: RunId, delta: Usage) -> Result<UsageVerdict, AppError> {
        RunManager::record_usage(self, RecordUsage { org, run, delta }).await
    }

    async fn fail_run(&self, org: OrgId, run: RunId, error: RunError) -> Result<(), AppError> {
        RunManager::fail_run(self, FailRun { org, run, error }).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_section_parses_with_defaults() {
        let config: GatewayConfig = toml::from_str(
            r#"
            mode = "gateway"
            anthropic_base_url = "https://api.anthropic.com"
            vertex = { project = "p", region = "us-east5", enabled_orgs = ["hoop"] }
            per_run_in_flight = 4
            beta_allowlist_vertex = []
            [pricing."claude-model-id"]
            input = 3000000
            output = 15000000
            cache_write = 3750000
            cache_read = 300000
            "#,
        )
        .unwrap();
        config.validate().unwrap();
        assert!(config.vertex_enabled_for("hoop"));
        assert!(!config.vertex_enabled_for("other"));
        assert_eq!(config.price("claude-model-id").map(|p| p.output), Some(15_000_000));

        let minimal: GatewayConfig = toml::from_str("").unwrap();
        assert_eq!(minimal, GatewayConfig::default());
        let dev: GatewayConfig = toml::from_str("mode = \"harness-login\"").unwrap();
        assert_eq!(dev.mode, GatewayMode::HarnessLogin);
    }

    #[test]
    fn config_rejects_unsafe_values() {
        let zero = GatewayConfig { per_run_in_flight: 0, ..GatewayConfig::default() };
        assert!(zero.validate().is_err());
        let vertex = GatewayConfig {
            vertex: Some(VertexRouteConfig { project: "p/../x".into(), region: "us-east5".into(), enabled_orgs: vec![] }),
            ..GatewayConfig::default()
        };
        assert!(vertex.validate().is_err());
        assert!(toml::from_str::<GatewayConfig>("unknown_field = 1").is_err());
    }
}
