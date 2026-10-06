//! Plain Slack Web API client over `reqwest` (intake.md "`mage_server::adapters::driven::slack`").
//! Every method answers HTTP 200 with `{ "ok": bool, "error": "<code>" }`; transport-level
//! signals are HTTP 429 (`Retry-After`) and 5xx.

use std::sync::Arc;
use std::time::Duration;

use mage_core::ports::Clock;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use url::Url;

use super::SlackConfig;
use crate::ports::chat::ChatError;

/// Wait used when a 429 carries no usable `Retry-After`.
const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(60);

pub(crate) struct SlackApi {
    http: reqwest::Client,
    base: Url,
    clock: Arc<dyn Clock>,
}

#[derive(Deserialize)]
struct Envelope {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
}

impl SlackApi {
    pub(crate) fn new(config: &SlackConfig, clock: Arc<dyn Clock>) -> Result<SlackApi, ChatError> {
        let mut base = config.api_base.clone();
        if !matches!(base.scheme(), "https" | "http") || base.cannot_be_a_base() {
            return Err(ChatError::Invalid(format!("slack api_base must be an http(s) base url: {base}")));
        }
        if !base.path().ends_with('/') {
            let path = format!("{}/", base.path());
            base.set_path(&path);
        }
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_millis(config.connect_timeout_ms))
            .timeout(Duration::from_millis(config.request_timeout_ms))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| ChatError::Unavailable(format!("slack http client: {e}")))?;
        Ok(SlackApi { http, base, clock })
    }

    fn url(&self, method: &str) -> Result<Url, ChatError> {
        self.base.join(method).map_err(|e| ChatError::Invalid(format!("slack method url {method}: {e}")))
    }

    /// `GET {base}{method}?{query}` with a bearer token.
    pub(crate) async fn get<T: DeserializeOwned>(&self, method: &str, token: &str, query: &[(&str, String)]) -> Result<T, ChatError> {
        let request = self.http.get(self.url(method)?).bearer_auth(token).query(query);
        self.finish(method, request.send().await).await
    }

    /// `POST {base}{method}` with a JSON body and a bearer token.
    pub(crate) async fn post_json<B: Serialize + ?Sized, T: DeserializeOwned>(&self, method: &str, token: &str, body: &B) -> Result<T, ChatError> {
        let request = self.http.post(self.url(method)?).bearer_auth(token).json(body);
        self.finish(method, request.send().await).await
    }

    /// `POST {base}{method}` form-encoded, without a token (`oauth.v2.access`).
    pub(crate) async fn post_form<T: DeserializeOwned>(&self, method: &str, form: &[(&str, &str)]) -> Result<T, ChatError> {
        let request = self.http.post(self.url(method)?).form(form);
        self.finish(method, request.send().await).await
    }

    async fn finish<T: DeserializeOwned>(&self, method: &str, sent: Result<reqwest::Response, reqwest::Error>) -> Result<T, ChatError> {
        let response = sent.map_err(|e| transport_error(method, &e))?;
        let status = response.status();
        if status.as_u16() == 429 {
            let wait = retry_after(response.headers()).unwrap_or(DEFAULT_RETRY_AFTER);
            return Err(ChatError::RateLimited { retry_at: self.clock.now() + wait });
        }
        if status.is_server_error() {
            return Err(ChatError::Unavailable(format!("slack {method}: HTTP {status}")));
        }
        if !status.is_success() {
            return Err(ChatError::Invalid(format!("slack {method}: HTTP {status}")));
        }
        let body = response.bytes().await.map_err(|e| transport_error(method, &e))?;
        let envelope: Envelope =
            serde_json::from_slice(&body).map_err(|e| ChatError::Unavailable(format!("slack {method}: malformed response: {e}")))?;
        if !envelope.ok {
            let code = envelope.error.unwrap_or_else(|| "unknown_error".to_owned());
            return Err(map_error_code(&code, self.clock.now() + DEFAULT_RETRY_AFTER));
        }
        serde_json::from_slice(&body).map_err(|e| ChatError::Unavailable(format!("slack {method}: unexpected response shape: {e}")))
    }
}

fn transport_error(method: &str, e: &reqwest::Error) -> ChatError {
    if e.is_timeout() {
        ChatError::Unavailable(format!("slack {method}: timeout"))
    } else {
        ChatError::Unavailable(format!("slack {method}: {e}"))
    }
}

/// `Retry-After` in seconds (Slack sends delta-seconds only).
fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let secs: u64 = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?.trim().parse().ok()?;
    Some(Duration::from_secs(secs))
}

/// Slack `error` codes → [`ChatError`] (intake.md "Error mapping"). `ratelimited_retry_at` is used
/// for an `ok: false` `ratelimited` answer that arrived without HTTP 429.
pub(crate) fn map_error_code(code: &str, ratelimited_retry_at: time::OffsetDateTime) -> ChatError {
    match code {
        "not_in_channel" | "missing_scope" => ChatError::NoAccess(code.to_owned()),
        "channel_not_found" | "thread_not_found" => ChatError::NotFound,
        "invalid_auth" | "token_revoked" | "account_inactive" | "not_authed" | "token_expired" => ChatError::Unauthorized,
        "ratelimited" => ChatError::RateLimited { retry_at: ratelimited_retry_at },
        "internal_error" | "fatal_error" | "service_unavailable" | "request_timeout" => ChatError::Unavailable(code.to_owned()),
        _ => ChatError::Invalid(code.to_owned()),
    }
}
