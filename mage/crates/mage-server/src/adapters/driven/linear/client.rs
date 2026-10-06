//! GraphQL transport: one POST per operation, rate-limit budget tracking per workspace, and the
//! Linear error → [`TrackerError`] mapping (intake.md "Rate limits", "Error mapping").

use std::collections::HashMap;
use std::sync::Arc;

use mage_core::ports::Clock;
use parking_lot::Mutex;
use reqwest::header::{HeaderMap, AUTHORIZATION};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::Value;
use time::{Duration, OffsetDateTime};
use url::Url;

use crate::domain::intake::LinearAuthMode;
use crate::ports::tracker::TrackerError;

/// Wait used when Linear signals a rate limit without a usable reset header.
const FALLBACK_RATE_LIMIT_WAIT: Duration = Duration::minutes(1);

/// `remaining * PRE_EMPTIVE_DIVISOR < limit` ⇔ below 5 % of the budget.
const PRE_EMPTIVE_DIVISOR: u64 = 20;

/// One rate-limit budget from the `X-RateLimit-*` headers.
#[derive(Debug, Clone, Copy)]
struct Budget {
    remaining: u64,
    limit: Option<u64>,
    reset: OffsetDateTime,
}

impl Budget {
    /// Below 5 % of its limit (or exhausted when no limit is known) and not yet reset.
    fn is_low(&self, default_limit: Option<u64>, now: OffsetDateTime) -> bool {
        if now >= self.reset {
            return false;
        }
        match self.limit.or(default_limit) {
            Some(limit) => self.remaining.saturating_mul(PRE_EMPTIVE_DIVISOR) < limit,
            None => self.remaining == 0,
        }
    }
}

/// Budgets last seen for one workspace (API key: the Mage user; OAuth: the app user).
#[derive(Debug, Default)]
struct RateState {
    requests: Option<Budget>,
    complexity: Option<Budget>,
    /// `X-RateLimit-Endpoint-*`, keyed by the operation that reported it.
    endpoints: HashMap<&'static str, Budget>,
}

impl RateState {
    /// Resets of every budget below its threshold.
    fn low_resets(&self, mode: LinearAuthMode, op: &str, now: OffsetDateTime) -> Option<OffsetDateTime> {
        let (requests_limit, complexity_limit) = default_limits(mode);
        [
            self.requests.filter(|b| b.is_low(Some(requests_limit), now)),
            self.complexity.filter(|b| b.is_low(Some(complexity_limit), now)),
            self.endpoints.get(op).copied().filter(|b| b.is_low(None, now)),
        ]
        .into_iter()
        .flatten()
        .map(|b| b.reset)
        .max()
    }

    fn latest_future_reset(&self, op: &str, now: OffsetDateTime) -> Option<OffsetDateTime> {
        [self.requests, self.complexity, self.endpoints.get(op).copied()].into_iter().flatten().map(|b| b.reset).filter(|r| *r > now).max()
    }
}

/// Documented hourly budgets (requests, complexity points), used when a response omits the
/// `-Limit` header.
fn default_limits(mode: LinearAuthMode) -> (u64, u64) {
    match mode {
        LinearAuthMode::ApiKey => (2_500, 3_000_000),
        LinearAuthMode::OAuth => (5_000, 2_000_000),
    }
}

/// Who a request runs as: the rate-limit bucket and the `Authorization` header value.
pub(super) struct Caller<'a> {
    pub workspace_id: &'a str,
    pub mode: LinearAuthMode,
    pub authorization: &'a str,
}

pub(super) struct GraphqlClient {
    http: reqwest::Client,
    api_url: Url,
    clock: Arc<dyn Clock>,
    rates: Mutex<HashMap<String, RateState>>,
}

#[derive(Deserialize)]
struct RawResponse {
    #[serde(default)]
    data: Option<Value>,
    #[serde(default)]
    errors: Vec<GraphqlError>,
}

#[derive(Debug, Deserialize)]
pub(super) struct GraphqlError {
    #[serde(default)]
    message: String,
    #[serde(default)]
    extensions: Extensions,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Extensions {
    code: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    status_code: Option<u16>,
    user_error: Option<bool>,
    user_presentable_message: Option<String>,
}

/// What one GraphQL error means for the caller.
#[derive(Debug, PartialEq, Eq)]
enum ErrorClass {
    RateLimited,
    Unauthorized,
    NotFound,
    Conflict,
    Invalid(String),
    Unavailable(String),
}

impl GraphqlError {
    fn class(&self, http_status: u16) -> ErrorClass {
        let ext = &self.extensions;
        let code = ext.code.as_deref().unwrap_or("").to_ascii_uppercase();
        let kind = ext.kind.as_deref().unwrap_or("").to_ascii_lowercase();
        let text = format!("{} {}", self.message, ext.user_presentable_message.as_deref().unwrap_or("")).to_ascii_lowercase();
        let status = ext.status_code.unwrap_or(0);
        if code == "RATELIMITED" || kind == "ratelimited" || status == 429 {
            ErrorClass::RateLimited
        } else if matches!(code.as_str(), "AUTHENTICATION_ERROR" | "UNAUTHENTICATED" | "FORBIDDEN")
            || kind.contains("authentication")
            || kind == "forbidden"
            || matches!(status, 401 | 403)
        {
            ErrorClass::Unauthorized
        } else if code == "ENTITY_NOT_FOUND" || kind.contains("not found") || text.contains("entity not found") || status == 404 {
            ErrorClass::NotFound
        } else if code == "CONFLICT" || text.contains("already exists") {
            ErrorClass::Conflict
        } else if matches!(code.as_str(), "INPUT_ERROR" | "INVALID_INPUT" | "BAD_USER_INPUT" | "GRAPHQL_VALIDATION_FAILED" | "GRAPHQL_PARSE_FAILED")
            || kind.contains("invalid input")
            || ext.user_error == Some(true)
        {
            ErrorClass::Invalid(self.message.clone())
        } else if status >= 500 || code == "INTERNAL_SERVER_ERROR" || http_status >= 500 {
            ErrorClass::Unavailable(self.message.clone())
        } else if http_status == 400 {
            ErrorClass::Invalid(self.message.clone())
        } else {
            ErrorClass::Unavailable(self.message.clone())
        }
    }
}

/// The most significant class among `errors`: rate limit, then auth, then the first error.
fn classify(errors: &[GraphqlError], http_status: u16) -> ErrorClass {
    let classes: Vec<ErrorClass> = errors.iter().map(|e| e.class(http_status)).collect();
    if classes.contains(&ErrorClass::RateLimited) {
        ErrorClass::RateLimited
    } else if classes.contains(&ErrorClass::Unauthorized) {
        ErrorClass::Unauthorized
    } else {
        classes.into_iter().next().unwrap_or(ErrorClass::Unavailable("empty GraphQL error list".into()))
    }
}

pub(super) fn transport_error(err: &reqwest::Error) -> TrackerError {
    if err.is_timeout() {
        TrackerError::Unavailable("Linear request timed out".into())
    } else if err.is_connect() {
        TrackerError::Unavailable(format!("cannot connect to Linear: {err}"))
    } else {
        TrackerError::Unavailable(format!("Linear request failed: {err}"))
    }
}

fn header_u64(headers: &HeaderMap, name: &str) -> Option<u64> {
    headers.get(name)?.to_str().ok()?.trim().parse().ok()
}

fn epoch_ms(ms: u64) -> Option<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000).ok()
}

fn read_budget(headers: &HeaderMap, prefix: &str) -> Option<Budget> {
    let remaining = header_u64(headers, &format!("{prefix}-remaining"))?;
    let reset = epoch_ms(header_u64(headers, &format!("{prefix}-reset"))?)?;
    Some(Budget { remaining, limit: header_u64(headers, &format!("{prefix}-limit")), reset })
}

impl GraphqlClient {
    pub(super) fn new(http: reqwest::Client, api_url: Url, clock: Arc<dyn Clock>) -> Self {
        GraphqlClient { http, api_url, clock, rates: Mutex::new(HashMap::new()) }
    }

    /// `RateLimited` when a tracked budget of the workspace is below 5 % (no request is sent).
    fn precheck(&self, caller: &Caller<'_>, op: &'static str) -> Result<(), TrackerError> {
        let now = self.clock.now();
        let rates = self.rates.lock();
        match rates.get(caller.workspace_id).and_then(|s| s.low_resets(caller.mode, op, now)) {
            Some(retry_at) => Err(TrackerError::RateLimited { retry_at }),
            None => Ok(()),
        }
    }

    fn record(&self, workspace_id: &str, op: &'static str, headers: &HeaderMap) {
        let requests = read_budget(headers, "x-ratelimit-requests");
        let complexity = read_budget(headers, "x-ratelimit-complexity");
        let endpoint = read_budget(headers, "x-ratelimit-endpoint-requests");
        if requests.is_none() && complexity.is_none() && endpoint.is_none() {
            return;
        }
        let mut rates = self.rates.lock();
        let state = rates.entry(workspace_id.to_owned()).or_default();
        if requests.is_some() {
            state.requests = requests;
        }
        if complexity.is_some() {
            state.complexity = complexity;
        }
        if let Some(endpoint) = endpoint {
            state.endpoints.insert(op, endpoint);
        }
    }

    /// `retry_at` for a `RATELIMITED` answer: the reset of the exhausted budget, else the latest
    /// known future reset, else one minute.
    fn retry_at(&self, caller: &Caller<'_>, op: &'static str) -> OffsetDateTime {
        let now = self.clock.now();
        let rates = self.rates.lock();
        rates
            .get(caller.workspace_id)
            .and_then(|s| s.low_resets(caller.mode, op, now).or_else(|| s.latest_future_reset(op, now)))
            .unwrap_or(now + FALLBACK_RATE_LIMIT_WAIT)
    }

    /// Runs one operation as `caller` and decodes `data` into `T`.
    pub(super) async fn execute<T: DeserializeOwned>(&self, caller: &Caller<'_>, op: &'static str, query: &str, variables: &Value) -> Result<T, TrackerError> {
        self.precheck(caller, op)?;
        let body = serde_json::json!({ "query": query, "variables": variables, "operationName": op });
        let response = self
            .http
            .post(self.api_url.clone())
            .header(AUTHORIZATION, caller.authorization)
            .json(&body)
            .send()
            .await
            .map_err(|e| transport_error(&e))?;
        let status = response.status().as_u16();
        self.record(caller.workspace_id, op, response.headers());
        let bytes = response.bytes().await.map_err(|e| transport_error(&e))?;
        let raw: RawResponse = match serde_json::from_slice(&bytes) {
            Ok(raw) => raw,
            Err(_) => return Err(self.status_error(caller, op, status, "non-JSON response")),
        };
        if !raw.errors.is_empty() {
            return Err(match classify(&raw.errors, status) {
                ErrorClass::RateLimited => TrackerError::RateLimited { retry_at: self.retry_at(caller, op) },
                ErrorClass::Unauthorized => TrackerError::Unauthorized,
                ErrorClass::NotFound => TrackerError::NotFound,
                ErrorClass::Conflict => TrackerError::Conflict,
                ErrorClass::Invalid(msg) => TrackerError::Invalid(format!("Linear {op}: {msg}")),
                ErrorClass::Unavailable(msg) => TrackerError::Unavailable(format!("Linear {op}: {msg}")),
            });
        }
        if !(200..300).contains(&status) {
            return Err(self.status_error(caller, op, status, "no GraphQL errors"));
        }
        let data = raw.data.ok_or_else(|| TrackerError::Unavailable(format!("Linear {op}: response without data")))?;
        serde_json::from_value(data).map_err(|e| TrackerError::Unavailable(format!("Linear {op}: unexpected response shape: {e}")))
    }

    fn status_error(&self, caller: &Caller<'_>, op: &'static str, status: u16, what: &str) -> TrackerError {
        match status {
            401 | 403 => TrackerError::Unauthorized,
            429 => TrackerError::RateLimited { retry_at: self.retry_at(caller, op) },
            _ => TrackerError::Unavailable(format!("Linear {op}: HTTP {status}, {what}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn error(json: Value) -> GraphqlError {
        serde_json::from_value(json).expect("error shape")
    }

    #[test]
    fn linear_error_shapes_map_to_tracker_classes() {
        let cases = [
            (serde_json::json!({"message": "Rate limit exceeded", "extensions": {"code": "RATELIMITED"}}), 400, ErrorClass::RateLimited),
            (
                serde_json::json!({"message": "Authentication required, not authenticated", "extensions": {"type": "authentication error", "code": "AUTHENTICATION_ERROR", "statusCode": 401}}),
                400,
                ErrorClass::Unauthorized,
            ),
            (
                serde_json::json!({"message": "Entity not found: Issue", "extensions": {"type": "invalid input", "code": "INPUT_ERROR", "userError": true}}),
                200,
                ErrorClass::NotFound,
            ),
            (
                serde_json::json!({"message": "Argument Validation Error", "extensions": {"type": "invalid input", "code": "INPUT_ERROR", "userError": true}}),
                200,
                ErrorClass::Invalid("Argument Validation Error".into()),
            ),
            (serde_json::json!({"message": "Comment with this id already exists"}), 200, ErrorClass::Conflict),
            (serde_json::json!({"message": "boom", "extensions": {"code": "INTERNAL_SERVER_ERROR"}}), 200, ErrorClass::Unavailable("boom".into())),
            (serde_json::json!({"message": "Cannot query field \"nope\""}), 400, ErrorClass::Invalid("Cannot query field \"nope\"".into())),
        ];
        for (json, status, want) in cases {
            let label = json.to_string();
            assert_eq!(error(json).class(status), want, "{label}");
        }
    }

    #[test]
    fn rate_limit_wins_over_other_errors_in_one_response() {
        let errors = [error(serde_json::json!({"message": "Entity not found"})), error(serde_json::json!({"message": "x", "extensions": {"code": "RATELIMITED"}}))];
        assert_eq!(classify(&errors, 400), ErrorClass::RateLimited);
    }

    #[test]
    fn budgets_are_low_below_five_percent_until_reset() {
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let reset = now + Duration::minutes(10);
        let budget = |remaining, limit| Budget { remaining, limit, reset };
        assert!(budget(124, Some(2_500)).is_low(None, now));
        assert!(!budget(125, Some(2_500)).is_low(None, now));
        assert!(budget(124, None).is_low(Some(2_500), now), "documented default limit applies");
        assert!(!budget(1, None).is_low(None, now) && budget(0, None).is_low(None, now));
        assert!(!budget(0, Some(2_500)).is_low(None, reset), "a passed reset frees the budget");
    }
}
