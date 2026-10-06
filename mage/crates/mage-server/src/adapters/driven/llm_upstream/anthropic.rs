//! Anthropic Messages API with the tenant's key (`x-api-key`). The client body is sent byte for
//! byte; betas go through verbatim.

use reqwest::header::{CONTENT_TYPE, HeaderValue};
use url::Url;

use super::{UpstreamTimeouts, execute, http_client};
use crate::ports::llm::{ModelRoute, UpstreamError, UpstreamOp, UpstreamRequest, UpstreamResponse};

/// `anthropic-version` sent when the client did not name one.
pub const DEFAULT_ANTHROPIC_VERSION: &str = "2023-06-01";

pub struct AnthropicApi {
    client: reqwest::Client,
    messages_url: Url,
    count_tokens_url: Url,
    timeouts: UpstreamTimeouts,
}

impl AnthropicApi {
    /// `base_url` from server config (`gateway.anthropic_base_url`, e.g. `https://api.anthropic.com`).
    pub fn new(base_url: &Url) -> Result<AnthropicApi, UpstreamError> {
        AnthropicApi::with_timeouts(base_url, UpstreamTimeouts::default())
    }

    pub fn with_timeouts(base_url: &Url, timeouts: UpstreamTimeouts) -> Result<AnthropicApi, UpstreamError> {
        if !matches!(base_url.scheme(), "https" | "http") || base_url.cannot_be_a_base() {
            return Err(UpstreamError::Invalid(format!("anthropic base url must be http(s): {base_url}")));
        }
        let endpoint = |path: &str| {
            let mut url = base_url.clone();
            let joined = format!("{}/{path}", base_url.path().trim_end_matches('/'));
            url.set_path(&joined);
            url.set_query(None);
            url
        };
        Ok(AnthropicApi {
            client: http_client(&timeouts)?,
            messages_url: endpoint("v1/messages"),
            count_tokens_url: endpoint("v1/messages/count_tokens"),
            timeouts,
        })
    }

    pub fn supports(op: UpstreamOp) -> bool {
        matches!(op, UpstreamOp::Messages | UpstreamOp::CountTokens)
    }

    pub async fn send(&self, req: UpstreamRequest) -> Result<UpstreamResponse, UpstreamError> {
        let ModelRoute::AnthropicApi { api_key } = &req.route else {
            return Err(UpstreamError::Invalid("the Anthropic adapter only serves the AnthropicApi route".into()));
        };
        let mut key = HeaderValue::from_bytes(api_key.expose())
            .map_err(|_| UpstreamError::Invalid("the stored model key is not a valid header value".into()))?;
        key.set_sensitive(true);
        let url = match req.op {
            UpstreamOp::Messages => &self.messages_url,
            UpstreamOp::CountTokens => &self.count_tokens_url,
        };
        let mut request = self
            .client
            .post(url.clone())
            .header(CONTENT_TYPE, "application/json")
            .header("anthropic-version", req.anthropic_version.as_deref().unwrap_or(DEFAULT_ANTHROPIC_VERSION))
            .header("x-api-key", key);
        if !req.anthropic_beta.is_empty() {
            request = request.header("anthropic-beta", req.anthropic_beta.join(","));
        }
        execute(request.body(req.body), req.stream, &self.timeouts).await
    }
}
