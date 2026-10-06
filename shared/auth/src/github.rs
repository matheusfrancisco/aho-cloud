//! GitHub OAuth web flow with PKCE (S256) and `state`; identity from `GET /user` and the primary
//! verified address from `GET /user/emails`. The user token is dropped inside [`GitHubOAuth::exchange`].

use reqwest::header::{ACCEPT, USER_AGENT};
use reqwest::{RequestBuilder, StatusCode};
use serde::Deserialize;

use crate::http::{exchange_code, read_json, status_error, transport, with_query};
use crate::pkce::{CHALLENGE_METHOD, is_valid_verifier};
use crate::{AuthError, AuthorizeParams, ExternalIdentity, Provider};

pub const AUTHORIZE_URL: &str = "https://github.com/login/oauth/authorize";
pub const TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
pub const API_BASE: &str = "https://api.github.com";
/// Ignored by GitHub App user authorization; required by OAuth Apps to read emails.
pub const DEFAULT_SCOPE: &str = "read:user user:email";
const API_VERSION: &str = "2022-11-28";
const API_ACCEPT: &str = "application/vnd.github+json";
/// GitHub logins are at most 39 characters.
const LOGIN_MAX: usize = 39;

/// GitHub OAuth client configuration. URLs are overridable (tests, GitHub Enterprise).
#[derive(Clone)]
pub struct GitHubConfig {
    pub client_id: String,
    pub client_secret: String,
    /// Space-separated scopes; empty omits the parameter.
    pub scope: String,
    pub authorize_url: String,
    pub token_url: String,
    /// REST API base, without trailing path (e.g. `https://api.github.com`).
    pub api_base: String,
    /// GitHub rejects API calls without a `User-Agent`.
    pub user_agent: String,
}

impl GitHubConfig {
    /// github.com endpoints and [`DEFAULT_SCOPE`].
    pub fn new(client_id: impl Into<String>, client_secret: impl Into<String>) -> Self {
        Self {
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            scope: DEFAULT_SCOPE.to_owned(),
            authorize_url: AUTHORIZE_URL.to_owned(),
            token_url: TOKEN_URL.to_owned(),
            api_base: API_BASE.to_owned(),
            user_agent: concat!("shared-auth/", env!("CARGO_PKG_VERSION")).to_owned(),
        }
    }
}

impl std::fmt::Debug for GitHubConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitHubConfig")
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .field("scope", &self.scope)
            .field("authorize_url", &self.authorize_url)
            .field("token_url", &self.token_url)
            .field("api_base", &self.api_base)
            .field("user_agent", &self.user_agent)
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct GitHubOAuth {
    config: GitHubConfig,
    http: reqwest::Client,
}

#[derive(Deserialize)]
struct GitHubUser {
    id: u64,
    login: String,
    name: Option<String>,
    avatar_url: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
}

#[derive(Deserialize)]
struct GitHubEmail {
    email: String,
    primary: bool,
    verified: bool,
}

impl GitHubOAuth {
    pub fn new(config: GitHubConfig, http: reqwest::Client) -> Self {
        Self { config, http }
    }

    pub fn config(&self) -> &GitHubConfig {
        &self.config
    }

    /// Authorize redirect with `client_id`, `redirect_uri`, `state`, `code_challenge`,
    /// `code_challenge_method=S256` and `scope`.
    pub fn authorize_url(&self, params: &AuthorizeParams) -> String {
        let mut pairs = vec![
            ("client_id", self.config.client_id.as_str()),
            ("redirect_uri", params.redirect_uri.as_str()),
            ("state", params.state.as_str()),
            ("code_challenge", params.pkce_challenge.as_str()),
            ("code_challenge_method", CHALLENGE_METHOD),
        ];
        if !self.config.scope.is_empty() {
            pairs.push(("scope", self.config.scope.as_str()));
        }
        with_query(&self.config.authorize_url, pairs)
    }

    /// Exchange `code` + PKCE verifier for a user token, read the identity, drop the token.
    pub async fn exchange(
        &self,
        code: &str,
        pkce_verifier: &str,
        redirect_uri: &str,
    ) -> Result<ExternalIdentity, AuthError> {
        if code.is_empty() {
            return Err(AuthError::invalid("empty authorization code"));
        }
        if !is_valid_verifier(pkce_verifier) {
            return Err(AuthError::invalid("malformed PKCE verifier"));
        }
        let request = self
            .http
            .post(&self.config.token_url)
            .header(ACCEPT, "application/json")
            .header(USER_AGENT, &self.config.user_agent)
            .form(&[
                ("client_id", self.config.client_id.as_str()),
                ("client_secret", self.config.client_secret.as_str()),
                ("code", code),
                ("redirect_uri", redirect_uri),
                ("code_verifier", pkce_verifier),
            ]);
        let token = exchange_code("github token exchange", request).await?;
        let access_token = token
            .access_token
            .filter(|t| !t.is_empty())
            .ok_or_else(|| AuthError::invalid("github token exchange: no access_token"))?;

        let user = self.user(&access_token).await?;
        let email = self.primary_verified_email(&access_token).await?;
        drop(access_token);

        Ok(identity(user, email))
    }

    /// Resolve a login to its stable numeric id (invites). `token` is an installation or app
    /// token; `None` calls anonymously (low rate limit). Unknown logins and non-user accounts
    /// (organizations, bots) resolve to `None`. `email` is always `None`: public profile emails
    /// are not verified.
    pub async fn lookup_login(
        &self,
        login: &str,
        token: Option<&str>,
    ) -> Result<Option<ExternalIdentity>, AuthError> {
        if !is_valid_login(login) {
            return Err(AuthError::invalid("malformed GitHub login"));
        }
        let what = "github user lookup";
        let mut request = self.api_get(&format!("/users/{login}"));
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.map_err(|e| transport(what, e))?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(status_error(what, &response));
        }
        let user: GitHubUser = read_json(what, response).await?;
        if user.kind.as_deref().is_some_and(|kind| kind != "User") {
            return Ok(None);
        }
        Ok(Some(identity(user, None)))
    }

    async fn user(&self, access_token: &str) -> Result<GitHubUser, AuthError> {
        let what = "github user";
        let response = self
            .api_get("/user")
            .bearer_auth(access_token)
            .send()
            .await
            .map_err(|e| transport(what, e))?;
        if !response.status().is_success() {
            return Err(status_error(what, &response));
        }
        read_json(what, response).await
    }

    /// Primary verified address; `None` when there is none or the app may not read emails.
    async fn primary_verified_email(&self, access_token: &str) -> Result<Option<String>, AuthError> {
        let what = "github user emails";
        let response = self
            .api_get("/user/emails")
            .bearer_auth(access_token)
            .send()
            .await
            .map_err(|e| transport(what, e))?;
        let status = response.status();
        let rate_limited = response
            .headers()
            .get("x-ratelimit-remaining")
            .is_some_and(|v| v.as_bytes() == b"0");
        if (status == StatusCode::FORBIDDEN && !rate_limited) || status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(status_error(what, &response));
        }
        let emails: Vec<GitHubEmail> = read_json(what, response).await?;
        Ok(emails
            .into_iter()
            .find(|e| e.primary && e.verified)
            .map(|e| e.email))
    }

    fn api_get(&self, path: &str) -> RequestBuilder {
        let base = self.config.api_base.trim_end_matches('/');
        self.http
            .get(format!("{base}{path}"))
            .header(ACCEPT, API_ACCEPT)
            .header("X-GitHub-Api-Version", API_VERSION)
            .header(USER_AGENT, &self.config.user_agent)
    }
}

fn identity(user: GitHubUser, email: Option<String>) -> ExternalIdentity {
    ExternalIdentity {
        provider: Provider::GitHub,
        subject: user.id.to_string(),
        login: user.login,
        name: user.name.filter(|n| !n.is_empty()),
        email,
        avatar_url: user.avatar_url.filter(|u| !u.is_empty()),
    }
}

/// Path-safe GitHub login: 1..=39 of `[A-Za-z0-9-]` (legacy logins may also contain `_`).
fn is_valid_login(login: &str) -> bool {
    (1..=LOGIN_MAX).contains(&login.len())
        && login
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
