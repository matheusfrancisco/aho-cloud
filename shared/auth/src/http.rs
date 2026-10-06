//! Response handling shared by the providers.

use reqwest::{RequestBuilder, Response, StatusCode};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::AuthError;

/// `base` with `pairs` form-encoded and appended; infallible so authorize URLs need no `Result`.
pub(crate) fn with_query<'a>(base: &str, pairs: impl IntoIterator<Item = (&'a str, &'a str)>) -> String {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish();
    let separator = if base.contains('?') { '&' } else { '?' };
    format!("{base}{separator}{query}")
}

/// Token endpoint response (RFC 6749 §5.1/§5.2; OIDC adds `id_token`). Not `Debug`: it holds
/// the provider tokens, which callers drop as soon as the identity is read.
#[derive(Deserialize)]
pub(crate) struct TokenResponse {
    pub access_token: Option<String>,
    pub id_token: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

/// POST a code exchange and classify the outcome. GitHub answers errors with 200 + `error`;
/// Google with 400/401 + `error`; both are handled the same way.
pub(crate) async fn exchange_code(
    what: &str,
    request: RequestBuilder,
) -> Result<TokenResponse, AuthError> {
    let response = request.send().await.map_err(|e| transport(what, e))?;
    let status = response.status();
    if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS {
        return Err(AuthError::unavailable(format!("{what}: status {status}")));
    }
    let body = response.bytes().await.map_err(|e| transport(what, e))?;
    let parsed = serde_json::from_slice::<TokenResponse>(&body);
    match parsed {
        Ok(TokenResponse { error: Some(code), error_description, .. }) => {
            Err(AuthError::from_oauth_error(&code, error_description.as_deref()))
        }
        Ok(token) if status.is_success() => Ok(token),
        Ok(_) => Err(AuthError::invalid(format!("{what}: unexpected status {status}"))),
        Err(e) if status.is_success() => {
            Err(AuthError::invalid(format!("{what}: malformed response: {e}")))
        }
        Err(_) => Err(AuthError::invalid(format!("{what}: unexpected status {status}"))),
    }
}

/// Request could not be built (bad configured URL) or did not complete.
pub(crate) fn transport(what: &str, error: reqwest::Error) -> AuthError {
    if error.is_builder() {
        AuthError::invalid(format!("{what}: {error}"))
    } else {
        AuthError::unavailable(format!("{what}: {error}"))
    }
}

/// Body read as JSON; transport failures are transient, malformed bodies are not.
pub(crate) async fn read_json<T: DeserializeOwned>(
    what: &str,
    response: Response,
) -> Result<T, AuthError> {
    let body = response.bytes().await.map_err(|e| transport(what, e))?;
    serde_json::from_slice(&body)
        .map_err(|e| AuthError::invalid(format!("{what}: malformed response: {e}")))
}

/// Non-success status of an authenticated API call.
pub(crate) fn status_error(what: &str, response: &Response) -> AuthError {
    let status = response.status();
    let rate_limited = status == StatusCode::TOO_MANY_REQUESTS
        || (status == StatusCode::FORBIDDEN
            && response
                .headers()
                .get("x-ratelimit-remaining")
                .is_some_and(|v| v.as_bytes() == b"0"));
    if status == StatusCode::UNAUTHORIZED {
        AuthError::Denied
    } else if rate_limited || status.is_server_error() {
        AuthError::unavailable(format!("{what}: status {status}"))
    } else {
        AuthError::invalid(format!("{what}: unexpected status {status}"))
    }
}
