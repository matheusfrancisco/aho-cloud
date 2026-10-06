//! [`GitHubClient`]: authenticated REST calls for one GitHub App.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use mage_core::domain::ids::InstallationId;
use mage_core::ports::clock::Clock;
use mage_core::ports::source_host::{AccessToken, InstallationInfo, InstallationToken, SourceHostError, TokenScope};
use parking_lot::Mutex;
use reqwest::Method;
use reqwest::header::{ACCEPT, CONTENT_TYPE, ETAG, HeaderMap, IF_NONE_MATCH, LINK, RETRY_AFTER};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::Semaphore;
use url::Url;

use super::errors::{ErrorSignals, Failure, SecondaryBackoff, classify, error_message};
use super::etag::{Cached, EtagCache, EtagKey};
use super::link::next_link;
use super::tokens::{AppSigner, TokenCache, is_fresh, scope_key};
use super::wire::{AccessTokenRequest, AccessTokenResponse, Installation};
use super::{AppCredentials, GitHubConfig};

const GITHUB_JSON: &str = "application/vnd.github+json";
const API_VERSION_HEADER: &str = "x-github-api-version";
const API_VERSION: &str = "2022-11-28";
const USER_AGENT: &str = "ahomage";
/// Requests in flight per installation (GitHub's secondary limits punish bursts).
const PER_INSTALLATION_CONCURRENCY: usize = 4;

/// How a request authenticates.
#[derive(Debug, Clone, Copy)]
pub enum Auth<'a> {
    /// The app JWT (`/app/...` endpoints).
    App,
    /// A cached installation token of `scope`; a 401 evicts it, mints a new one and retries once.
    /// GETs are conditional (`ETag`).
    Installation { inst: InstallationId, scope: &'a TokenScope },
    /// A caller-held token (user OAuth tokens).
    Bearer(&'a AccessToken),
    Anonymous,
}

/// A decoded success response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiResponse<T> {
    pub status: u16,
    pub body: T,
    /// Absolute URL of the next page (`Link: <…>; rel="next"`); pass it back as `path_or_url`.
    pub next: Option<String>,
}

/// One GitHub App: HTTP client, app JWT, installation-token cache, ETag cache. Cheap to clone.
#[derive(Clone)]
pub struct GitHubClient {
    inner: Arc<Inner>,
}

struct Inner {
    config: GitHubConfig,
    http: reqwest::Client,
    clock: Arc<dyn Clock>,
    signer: AppSigner,
    tokens: TokenCache,
    etags: EtagCache,
    limits: Mutex<HashMap<InstallationId, Arc<Semaphore>>>,
    secondary: SecondaryBackoff,
}

/// A resolved request URL; `api` = under `api_base` (GitHub media type and API version headers).
struct Target {
    url: Url,
    api: bool,
}

/// A response before status mapping.
struct Raw {
    status: u16,
    headers: HeaderMap,
    body: Bytes,
    next: Option<String>,
}

fn invalid(message: impl Into<String>) -> SourceHostError {
    SourceHostError::Invalid(message.into())
}

/// `base` as a URL prefix: http(s), no query or fragment, path ending in `/`.
fn normalize_base(name: &str, base: &Url) -> Result<Url, SourceHostError> {
    if !matches!(base.scheme(), "http" | "https") || base.cannot_be_a_base() || base.query().is_some() || base.fragment().is_some() {
        return Err(invalid(format!("GitHub {name} {base} must be an http(s) URL without query or fragment")));
    }
    let mut base = base.clone();
    if !base.path().ends_with('/') {
        let path = format!("{}/", base.path());
        base.set_path(&path);
    }
    Ok(base)
}

fn transport_error(method: &Method, url: &Url, error: &reqwest::Error) -> SourceHostError {
    let what = if error.is_timeout() {
        "timed out"
    } else if error.is_connect() {
        "connection failed"
    } else {
        "failed"
    };
    // The URL is left out: a query string may carry OAuth parameters.
    let mut detail = error.to_string();
    if let Some(url) = error.url() {
        detail = detail.replace(url.as_str(), "");
    }
    SourceHostError::Unavailable(format!("GitHub {method} {} {what}: {detail}", url.path()))
}

fn decode<T: DeserializeOwned>(body: &[u8]) -> Result<T, SourceHostError> {
    let parsed = if body.iter().all(u8::is_ascii_whitespace) { serde_json::from_slice(b"null") } else { serde_json::from_slice(body) };
    parsed.map_err(|e| SourceHostError::Unavailable(format!("unexpected response from GitHub: {e}")))
}

impl GitHubClient {
    /// `Invalid` on a bad base URL or private key.
    pub fn new(config: &GitHubConfig, app: AppCredentials, clock: Arc<dyn Clock>) -> Result<Self, SourceHostError> {
        let config = GitHubConfig {
            api_base: normalize_base("api_base", &config.api_base)?,
            web_base: normalize_base("web_base", &config.web_base)?,
            connect_timeout_ms: config.connect_timeout_ms,
            request_timeout_ms: config.request_timeout_ms,
        };
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(Duration::from_millis(config.connect_timeout_ms))
            .timeout(Duration::from_millis(config.request_timeout_ms))
            .build()
            .map_err(|e| invalid(format!("GitHub HTTP client: {e}")))?;
        let signer = AppSigner::new(&app, clock.now())?;
        Ok(Self {
            inner: Arc::new(Inner {
                config,
                http,
                clock,
                signer,
                tokens: TokenCache::default(),
                etags: EtagCache::default(),
                limits: Mutex::new(HashMap::new()),
                secondary: SecondaryBackoff::default(),
            }),
        })
    }

    /// The configuration in use (base URLs normalized to end in `/`).
    pub fn config(&self) -> &GitHubConfig {
        &self.inner.config
    }

    /// RS256 app JWT (`iat = now - 60 s`, `exp = now + 9 min`, `iss = client id`), reused 8 min.
    pub async fn app_jwt(&self) -> Result<AccessToken, SourceHostError> {
        self.inner.signer.token(self.inner.clock.now())
    }

    /// A token of `scope` with at least `MIN_TOKEN_VALIDITY` left: cached per (installation,
    /// scope), minted once per key under concurrency.
    pub async fn installation_token(&self, inst: InstallationId, scope: &TokenScope) -> Result<InstallationToken, SourceHostError> {
        let key = scope_key(scope);
        let slot = self.inner.tokens.slot(inst, &key, self.inner.clock.now());
        let mut cached = slot.lock().await;
        if let Some(token) = cached.as_ref().filter(|t| is_fresh(t, self.inner.clock.now())) {
            return Ok(token.clone());
        }
        let minted = self.mint(inst, scope).await?;
        *cached = Some(minted.clone());
        Ok(minted)
    }

    /// Drops the cached tokens and ETag entries of `inst` (installation lifecycle events).
    pub fn evict_installation(&self, inst: InstallationId) {
        self.inner.tokens.evict_installation(inst);
        self.inner.etags.evict_installation(inst);
    }

    /// `GET /app/installations/{id}` with the app JWT; `suspended` = `suspended_at` is set.
    pub async fn installation(&self, inst: InstallationId) -> Result<InstallationInfo, SourceHostError> {
        let response: ApiResponse<Installation> = self.get(Auth::App, &format!("/app/installations/{}", inst.0)).await?;
        response.body.into_info()
    }

    pub async fn get<T: DeserializeOwned>(&self, auth: Auth<'_>, path_or_url: &str) -> Result<ApiResponse<T>, SourceHostError> {
        self.send::<(), T>(Method::GET, auth, path_or_url, None).await
    }

    /// `path_or_url`: `/path?query` under `api_base`, or an absolute URL under `api_base` or
    /// `web_base` (anything else is `Invalid`: credentials never leave GitHub). An empty body
    /// decodes as JSON `null`, so `T = ()` / `serde::de::IgnoredAny` fit 202/204 responses.
    pub async fn send<B: Serialize + ?Sized, T: DeserializeOwned>(
        &self,
        method: Method,
        auth: Auth<'_>,
        path_or_url: &str,
        body: Option<&B>,
    ) -> Result<ApiResponse<T>, SourceHostError> {
        let target = self.target(path_or_url)?;
        let body = match body {
            Some(body) => Some(Bytes::from(serde_json::to_vec(body).map_err(|e| invalid(format!("request body: {e}")))?)),
            None => None,
        };
        let raw = if let Auth::Installation { inst, scope } = auth {
            self.send_installation(&method, &target, body.as_ref(), inst, scope).await?
        } else {
            let bearer = match auth {
                Auth::App => Some(self.app_jwt().await?),
                Auth::Bearer(token) => Some(token.clone()),
                _ => None,
            };
            let raw = self.exchange(&method, &target, body.as_ref(), bearer.as_ref(), None).await?;
            self.check(raw)?
        };
        Ok(ApiResponse { status: raw.status, body: decode(&raw.body)?, next: raw.next })
    }

    fn target(&self, path_or_url: &str) -> Result<Target, SourceHostError> {
        let config = &self.inner.config;
        if path_or_url.starts_with('/') {
            let joined = format!("{}{path_or_url}", config.api_base.as_str().trim_end_matches('/'));
            let url = Url::parse(&joined).map_err(|e| invalid(format!("GitHub path {path_or_url:?}: {e}")))?;
            return Ok(Target { url, api: true });
        }
        let url = Url::parse(path_or_url).map_err(|e| invalid(format!("{path_or_url:?} is neither a /path nor a URL: {e}")))?;
        let under = |base: &Url| url.as_str().starts_with(base.as_str()).then_some(base.as_str().len());
        // When one base contains the other (same host), the longer prefix decides.
        match (under(&config.api_base), under(&config.web_base)) {
            (Some(api), Some(web)) => Ok(Target { url, api: api >= web }),
            (Some(_), None) => Ok(Target { url, api: true }),
            (None, Some(_)) => Ok(Target { url, api: false }),
            (None, None) => Err(invalid(format!("{} is outside the GitHub API and web bases", url.origin().ascii_serialization()))),
        }
    }

    async fn exchange(
        &self,
        method: &Method,
        target: &Target,
        body: Option<&Bytes>,
        bearer: Option<&AccessToken>,
        if_none_match: Option<&str>,
    ) -> Result<Raw, SourceHostError> {
        let mut request = self.inner.http.request(method.clone(), target.url.clone());
        request = if target.api {
            request.header(ACCEPT, GITHUB_JSON).header(API_VERSION_HEADER, API_VERSION)
        } else {
            request.header(ACCEPT, "application/json")
        };
        if let Some(token) = bearer {
            request = request.bearer_auth(token.expose());
        }
        if let Some(etag) = if_none_match {
            request = request.header(IF_NONE_MATCH, etag);
        }
        if let Some(body) = body {
            request = request.header(CONTENT_TYPE, "application/json").body(body.clone());
        }
        let response = request.send().await.map_err(|e| transport_error(method, &target.url, &e))?;
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let body = response.bytes().await.map_err(|e| transport_error(method, &target.url, &e))?;
        let next = headers.get(LINK).and_then(|v| v.to_str().ok()).and_then(next_link);
        tracing::debug!(%method, path = target.url.path(), status, "github request");
        Ok(Raw { status, headers, body, next })
    }

    /// 2xx passes (and ends a run of secondary limits); anything else maps to an error.
    fn check(&self, raw: Raw) -> Result<Raw, SourceHostError> {
        if (200..300).contains(&raw.status) {
            self.inner.secondary.reset();
            return Ok(raw);
        }
        let header = |name| raw.headers.get(name).and_then(|v: &reqwest::header::HeaderValue| v.to_str().ok());
        let message = error_message(&raw.body);
        let signals = ErrorSignals {
            status: raw.status,
            ratelimit_remaining: header("x-ratelimit-remaining"),
            ratelimit_reset: header("x-ratelimit-reset"),
            retry_after: header(RETRY_AFTER.as_str()),
            message: &message,
        };
        let now = self.inner.clock.now();
        let error = match classify(&signals, now) {
            Failure::Error(error) => error,
            Failure::SecondaryLimit => SourceHostError::RateLimited { retry_at: now + self.inner.secondary.next() },
        };
        if let SourceHostError::RateLimited { retry_at } = &error {
            tracing::warn!(status = raw.status, %retry_at, %message, "github rate limited");
        }
        Err(error)
    }

    async fn send_installation(
        &self,
        method: &Method,
        target: &Target,
        body: Option<&Bytes>,
        inst: InstallationId,
        scope: &TokenScope,
    ) -> Result<Raw, SourceHostError> {
        let key = scope_key(scope);
        let etag_key = (method == Method::GET).then(|| EtagKey { url: target.url.to_string(), inst, scope: key.clone() });
        let cached = etag_key.as_ref().and_then(|k| self.inner.etags.get(k));
        let if_none_match = cached.as_ref().map(|c| c.etag.as_str());

        let token = self.installation_token(inst, scope).await?;
        let limit = self.limit(inst);
        let _permit = limit.acquire().await.map_err(|e| SourceHostError::Unavailable(format!("installation request limit: {e}")))?;
        let mut raw = self.exchange(method, target, body, Some(&token.token), if_none_match).await?;
        if raw.status == 401 {
            let fresh = self.refresh_rejected(inst, scope, &key, &token.token).await?;
            raw = self.exchange(method, target, body, Some(&fresh.token), if_none_match).await?;
        }
        if raw.status == 304
            && let Some(hit) = cached
        {
            self.inner.secondary.reset();
            return Ok(Raw { status: hit.status, headers: raw.headers, body: hit.body, next: hit.next });
        }
        let raw = self.check(raw)?;
        if let Some(key) = etag_key
            && let Some(etag) = raw.headers.get(ETAG).and_then(|v| v.to_str().ok())
        {
            self.inner.etags.put(key, Cached { etag: etag.to_owned(), status: raw.status, body: raw.body.clone(), next: raw.next.clone() });
        }
        Ok(raw)
    }

    /// After a 401 with `rejected`: drop it and mint, unless a concurrent caller already did.
    async fn refresh_rejected(&self, inst: InstallationId, scope: &TokenScope, key: &str, rejected: &AccessToken) -> Result<InstallationToken, SourceHostError> {
        let slot = self.inner.tokens.slot(inst, key, self.inner.clock.now());
        let mut cached = slot.lock().await;
        if let Some(token) = cached.as_ref().filter(|t| &t.token != rejected && is_fresh(t, self.inner.clock.now())) {
            return Ok(token.clone());
        }
        *cached = None;
        let minted = self.mint(inst, scope).await?;
        *cached = Some(minted.clone());
        Ok(minted)
    }

    /// `POST /app/installations/{id}/access_tokens`. The token's repositories echo the scope; its
    /// permissions are the granted ones (the scope's when GitHub omits them).
    async fn mint(&self, inst: InstallationId, scope: &TokenScope) -> Result<InstallationToken, SourceHostError> {
        // Not through `send`: `send` reaches `mint` for installation auth (no async recursion).
        let target = self.target(&format!("/app/installations/{}/access_tokens", inst.0))?;
        let body = serde_json::to_vec(&AccessTokenRequest::new(scope)).map_err(|e| invalid(format!("token request: {e}")))?;
        let jwt = self.app_jwt().await?;
        let raw = self.check(self.exchange(&Method::POST, &target, Some(&Bytes::from(body)), Some(&jwt), None).await?)?;
        let response: AccessTokenResponse = decode(&raw.body)?;
        let granted = response.granted_permissions();
        tracing::debug!(%inst, expires_at = %response.expires_at, "minted installation token");
        Ok(InstallationToken {
            token: AccessToken::new(response.token),
            expires_at: response.expires_at,
            permissions: if granted.is_empty() { scope.permissions.clone() } else { granted },
            repositories: scope.repositories.clone(),
        })
    }

    fn limit(&self, inst: InstallationId) -> Arc<Semaphore> {
        self.inner.limits.lock().entry(inst).or_insert_with(|| Arc::new(Semaphore::new(PER_INSTALLATION_CONCURRENCY))).clone()
    }
}

#[cfg(test)]
mod tests {
    use mage_core::ports::clock::SystemClock;

    use super::*;

    fn client(api: &str, web: &str) -> Result<GitHubClient, SourceHostError> {
        let config = GitHubConfig {
            api_base: Url::parse(api).map_err(|e| invalid(e.to_string()))?,
            web_base: Url::parse(web).map_err(|e| invalid(e.to_string()))?,
            ..GitHubConfig::default()
        };
        let app = AppCredentials { client_id: "Iv1".into(), private_key_pem: include_str!("../../tests/fixtures/github_app_key.pem").into() };
        GitHubClient::new(&config, app, Arc::new(SystemClock))
    }

    fn resolved(client: &GitHubClient, path_or_url: &str) -> Result<(String, bool), SourceHostError> {
        client.target(path_or_url).map(|t| (t.url.to_string(), t.api))
    }

    #[test]
    fn paths_join_the_api_base_with_its_prefix() {
        let Ok(c) = client("https://ghe.example/api/v3", "https://ghe.example/") else { panic!("client") };
        assert_eq!(c.config().api_base.as_str(), "https://ghe.example/api/v3/", "bases are normalized");
        assert_eq!(resolved(&c, "/repos/o/r?x=1&y=a%2Fb"), Ok(("https://ghe.example/api/v3/repos/o/r?x=1&y=a%2Fb".into(), true)));
        assert_eq!(resolved(&c, "https://ghe.example/api/v3/x?page=2"), Ok(("https://ghe.example/api/v3/x?page=2".into(), true)));
        assert_eq!(resolved(&c, "https://ghe.example/login/oauth/access_token"), Ok(("https://ghe.example/login/oauth/access_token".into(), false)));
        let Ok(nested) = client("http://127.0.0.1:8080/", "http://127.0.0.1:8080/web/") else { panic!("client") };
        assert_eq!(resolved(&nested, "http://127.0.0.1:8080/web/x"), Ok(("http://127.0.0.1:8080/web/x".into(), false)), "the longer base wins");
        assert_eq!(resolved(&nested, "http://127.0.0.1:8080/x"), Ok(("http://127.0.0.1:8080/x".into(), true)));
    }

    #[test]
    fn foreign_urls_are_refused() {
        let Ok(c) = client("https://api.github.com/", "https://github.com/") else { panic!("client") };
        for url in ["https://evil.example/repos", "https://api.github.com.evil.example/x", "http://api.github.com/x", "repos/o/r", ""] {
            assert!(matches!(resolved(&c, url), Err(SourceHostError::Invalid(_))), "{url}");
        }
    }

    #[test]
    fn bad_bases_are_invalid() {
        assert!(matches!(client("ftp://x/", "https://github.com/").err(), Some(SourceHostError::Invalid(_))));
        assert!(matches!(client("https://api.github.com/?a=1", "https://github.com/").err(), Some(SourceHostError::Invalid(_))));
    }

    #[test]
    fn empty_bodies_decode_as_null() {
        assert_eq!(decode::<()>(b""), Ok(()));
        assert_eq!(decode::<()>(b" \n"), Ok(()));
        assert!(decode::<serde::de::IgnoredAny>(b"").is_ok());
        assert!(decode::<serde::de::IgnoredAny>(br#"{"a":1}"#).is_ok());
        assert_eq!(decode::<Option<u32>>(b"7"), Ok(Some(7)));
        assert!(matches!(decode::<u32>(b"<html>"), Err(SourceHostError::Unavailable(_))));
    }
}
