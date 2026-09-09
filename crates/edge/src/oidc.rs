//! Edge login through an identity provider: OIDC authorization code flow with PKCE, `state`
//! and `nonce` (Google, Microsoft, any custom issuer via discovery), and GitHub OAuth (which
//! has no id_token) via its user and emails APIs. id_tokens are verified against the
//! provider's JWKS (RS256/ES256), refetched once when a token names an unknown `kid`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use cloud_api::internal::Site;
use cloud_contract::manifest::{Auth, OidcProvider};
use cloud_contract::principal::LEEWAY_SECS;
use http::header;
use parking_lot::Mutex;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::jwt;
use crate::keys::Jwks;
use crate::session::Identity;
use crate::verify::VerifyError;

const CACHE_TTL: Duration = Duration::from_secs(3600);
const USER_AGENT: &str = "aho-cloud-edge";

/// An OAuth client registration the edge owns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientCredentials {
    pub client_id: String,
    /// Used when the site carries no `oidc_client_secret` of its own.
    pub client_secret: Option<String>,
}

/// GitHub OAuth endpoints (overridable for GitHub Enterprise and tests).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GithubEndpoints {
    pub authorize: String,
    pub token: String,
    pub api: String,
}

impl Default for GithubEndpoints {
    fn default() -> Self {
        Self {
            authorize: "https://github.com/login/oauth/authorize".into(),
            token: "https://github.com/login/oauth/access_token".into(),
            api: "https://api.github.com".into(),
        }
    }
}

/// The edge's own registrations for the built-in providers. Custom issuers bring their
/// client id in the manifest and their secret in the site record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OidcSettings {
    pub google: Option<ClientCredentials>,
    pub microsoft: Option<ClientCredentials>,
    pub github: Option<ClientCredentials>,
    pub google_issuer: String,
    /// Multi-tenant endpoint; the id_token issuer is checked with its `{tenantid}` filled in.
    pub microsoft_issuer: String,
    pub github_endpoints: GithubEndpoints,
}

impl Default for OidcSettings {
    fn default() -> Self {
        Self {
            google: None,
            microsoft: None,
            github: None,
            google_issuer: "https://accounts.google.com".into(),
            microsoft_issuer: "https://login.microsoftonline.com/common/v2.0".into(),
            github_endpoints: GithubEndpoints::default(),
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum OidcError {
    #[error("{0} login is not configured on this edge")]
    NotConfigured(&'static str),
    #[error("provider request failed: {0}")]
    Http(String),
    #[error("provider discovery: {0}")]
    Discovery(String),
    #[error("token endpoint: {0}")]
    Token(String),
    #[error("id_token: {0}")]
    IdToken(String),
    #[error("id_token nonce does not match this login")]
    Nonce,
}

impl From<reqwest::Error> for OidcError {
    fn from(e: reqwest::Error) -> Self {
        OidcError::Http(e.to_string())
    }
}

impl From<VerifyError> for OidcError {
    fn from(e: VerifyError) -> Self {
        OidcError::IdToken(e.to_string())
    }
}

/// A resolved provider for one site.
#[derive(Clone, Debug)]
pub(crate) enum Provider {
    Oidc { idp: &'static str, issuer: String, client_id: String, client_secret: Option<String> },
    Github { client_id: String, client_secret: Option<String>, endpoints: GithubEndpoints },
}

impl Provider {
    /// The provider `site.auth` names, with credentials; `None` when the site has no OIDC.
    pub fn for_site(site: &Site, settings: &OidcSettings) -> Option<Result<Self, OidcError>> {
        let Auth::Oidc { provider } = &site.auth else { return None };
        Some(Self::resolve(provider, site, settings))
    }

    fn resolve(provider: &OidcProvider, site: &Site, settings: &OidcSettings) -> Result<Self, OidcError> {
        let own = |creds: &Option<ClientCredentials>, name: &'static str| -> Result<(String, Option<String>), OidcError> {
            let c = creds.as_ref().ok_or(OidcError::NotConfigured(name))?;
            Ok((c.client_id.clone(), site.oidc_client_secret.clone().or_else(|| c.client_secret.clone())))
        };
        match provider {
            OidcProvider::Google => {
                let (client_id, client_secret) = own(&settings.google, "Google")?;
                Ok(Provider::Oidc { idp: "google", issuer: settings.google_issuer.clone(), client_id, client_secret })
            }
            OidcProvider::Microsoft => {
                let (client_id, client_secret) = own(&settings.microsoft, "Microsoft")?;
                Ok(Provider::Oidc { idp: "microsoft", issuer: settings.microsoft_issuer.clone(), client_id, client_secret })
            }
            OidcProvider::Github => {
                let (client_id, client_secret) = own(&settings.github, "GitHub")?;
                Ok(Provider::Github { client_id, client_secret, endpoints: settings.github_endpoints.clone() })
            }
            OidcProvider::Custom { issuer, client_id } => Ok(Provider::Oidc {
                idp: "oidc",
                issuer: issuer.clone(),
                client_id: client_id.clone(),
                client_secret: site.oidc_client_secret.clone(),
            }),
        }
    }
}

/// PKCE S256 challenge for `verifier`.
pub(crate) fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

#[derive(Debug, Deserialize)]
struct Discovery {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Audience {
    One(String),
    Many(Vec<String>),
}

#[derive(Debug, Deserialize)]
struct IdClaims {
    iss: String,
    sub: String,
    aud: Audience,
    #[serde(default)]
    azp: Option<String>,
    exp: u64,
    #[serde(default)]
    iat: Option<u64>,
    #[serde(default)]
    nonce: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    email_verified: Option<serde_json::Value>,
    #[serde(default)]
    groups: Option<serde_json::Value>,
    #[serde(default)]
    tid: Option<String>,
}

/// HTTP client plus discovery and JWKS caches.
pub(crate) struct OidcClient {
    http: reqwest::Client,
    discovery: Mutex<HashMap<String, (Instant, Arc<Discovery>)>>,
    jwks: Mutex<HashMap<String, (Instant, Arc<Jwks>)>>,
}

fn cached<V>(map: &Mutex<HashMap<String, (Instant, Arc<V>)>>, key: &str) -> Option<Arc<V>> {
    let map = map.lock();
    map.get(key).filter(|(at, _)| at.elapsed() < CACHE_TTL).map(|(_, v)| v.clone())
}

fn store<V>(map: &Mutex<HashMap<String, (Instant, Arc<V>)>>, key: &str, v: Arc<V>) {
    map.lock().insert(key.to_owned(), (Instant::now(), v));
}

fn form(pairs: &[(&str, &str)]) -> String {
    let mut s = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in pairs {
        s.append_pair(k, v);
    }
    s.finish()
}

impl OidcClient {
    pub fn new() -> Result<Self, reqwest::Error> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(USER_AGENT)
            .build()?;
        Ok(Self { http, discovery: Mutex::new(HashMap::new()), jwks: Mutex::new(HashMap::new()) })
    }

    async fn discover(&self, issuer: &str) -> Result<Arc<Discovery>, OidcError> {
        let issuer = issuer.trim_end_matches('/');
        if let Some(d) = cached(&self.discovery, issuer) {
            return Ok(d);
        }
        let url = format!("{issuer}/.well-known/openid-configuration");
        let resp = self.http.get(&url).send().await?;
        if !resp.status().is_success() {
            return Err(OidcError::Discovery(format!("{url} answered {}", resp.status())));
        }
        let d: Discovery = resp.json().await.map_err(|e| OidcError::Discovery(e.to_string()))?;
        if !d.issuer.contains("{tenantid}") && d.issuer.trim_end_matches('/') != issuer {
            return Err(OidcError::Discovery(format!("document issuer `{}` is not `{issuer}`", d.issuer)));
        }
        let d = Arc::new(d);
        store(&self.discovery, issuer, d.clone());
        Ok(d)
    }

    async fn keys(&self, uri: &str, refresh: bool) -> Result<Arc<Jwks>, OidcError> {
        if !refresh && let Some(k) = cached(&self.jwks, uri) {
            return Ok(k);
        }
        let resp = self.http.get(uri).send().await?;
        if !resp.status().is_success() {
            return Err(OidcError::Discovery(format!("{uri} answered {}", resp.status())));
        }
        let k = Arc::new(resp.json::<Jwks>().await.map_err(|e| OidcError::Discovery(e.to_string()))?);
        store(&self.jwks, uri, k.clone());
        Ok(k)
    }

    /// Where to send the browser to start the login.
    pub async fn authorize_url(
        &self,
        provider: &Provider,
        redirect_uri: &str,
        state: &str,
        nonce: &str,
        verifier: &str,
    ) -> Result<String, OidcError> {
        let challenge = pkce_challenge(verifier);
        let (endpoint, params): (String, Vec<(&str, &str)>) = match provider {
            Provider::Oidc { issuer, client_id, .. } => {
                let d = self.discover(issuer).await?;
                (
                    d.authorization_endpoint.clone(),
                    vec![
                        ("response_type", "code"),
                        ("client_id", client_id.as_str()),
                        ("redirect_uri", redirect_uri),
                        ("scope", "openid email profile"),
                        ("state", state),
                        ("nonce", nonce),
                        ("code_challenge", challenge.as_str()),
                        ("code_challenge_method", "S256"),
                    ],
                )
            }
            Provider::Github { client_id, endpoints, .. } => (
                endpoints.authorize.clone(),
                vec![
                    ("client_id", client_id.as_str()),
                    ("redirect_uri", redirect_uri),
                    ("scope", "read:user user:email"),
                    ("state", state),
                    ("code_challenge", challenge.as_str()),
                    ("code_challenge_method", "S256"),
                    ("allow_signup", "false"),
                ],
            ),
        };
        let sep = if endpoint.contains('?') { '&' } else { '?' };
        Ok(format!("{endpoint}{sep}{}", form(&params)))
    }

    /// Exchanges `code` and returns who logged in.
    pub async fn complete(
        &self,
        provider: &Provider,
        redirect_uri: &str,
        code: &str,
        verifier: &str,
        nonce: &str,
        now: u64,
    ) -> Result<Identity, OidcError> {
        match provider {
            Provider::Oidc { idp, issuer, client_id, client_secret } => {
                let d = self.discover(issuer).await?;
                let tokens = self.exchange(&d.token_endpoint, client_id, client_secret, redirect_uri, code, verifier).await?;
                let id_token = tokens.id_token.ok_or_else(|| OidcError::Token("no id_token in response".into()))?;
                let claims = self.check(&id_token, &d, client_id, nonce, now).await?;
                Ok(Identity {
                    sub: format!("{idp}:{}", claims.sub),
                    email: claims.email.map(|e| e.to_ascii_lowercase()),
                    email_verified: matches!(claims.email_verified, Some(serde_json::Value::Bool(true)))
                        || matches!(&claims.email_verified, Some(serde_json::Value::String(s)) if s == "true"),
                    groups: match claims.groups {
                        Some(serde_json::Value::Array(items)) => items.into_iter().filter_map(|v| v.as_str().map(str::to_owned)).collect(),
                        _ => Vec::new(),
                    },
                    amr: vec!["oidc".into()],
                })
            }
            Provider::Github { client_id, client_secret, endpoints } => {
                let tokens = self.exchange(&endpoints.token, client_id, client_secret, redirect_uri, code, verifier).await?;
                let access = tokens.access_token.ok_or_else(|| OidcError::Token("no access_token".into()))?;
                self.github_identity(&endpoints.api, &access).await
            }
        }
    }

    async fn exchange(
        &self,
        endpoint: &str,
        client_id: &str,
        client_secret: &Option<String>,
        redirect_uri: &str,
        code: &str,
        verifier: &str,
    ) -> Result<TokenResponse, OidcError> {
        let mut pairs = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", client_id),
            ("code_verifier", verifier),
        ];
        if let Some(secret) = client_secret {
            pairs.push(("client_secret", secret));
        }
        let resp = self
            .http
            .post(endpoint)
            .header(header::ACCEPT, "application/json")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(form(&pairs))
            .send()
            .await?;
        let status = resp.status();
        let body: TokenResponse = resp.json().await.map_err(|e| OidcError::Token(format!("{status}: {e}")))?;
        if let Some(err) = &body.error {
            let desc = body.error_description.as_deref().unwrap_or_default();
            return Err(OidcError::Token(format!("{err} {desc}").trim().to_owned()));
        }
        if !status.is_success() {
            return Err(OidcError::Token(status.to_string()));
        }
        Ok(body)
    }

    async fn check(&self, id_token: &str, d: &Discovery, client_id: &str, nonce: &str, now: u64) -> Result<IdClaims, OidcError> {
        let parts = jwt::parse(id_token)?;
        let alg = parts.header.alg.as_str();
        if alg != "RS256" && alg != "ES256" {
            return Err(VerifyError::Algorithm(alg.to_owned()).into());
        }
        // A kid we have not seen means the provider rotated: refetch the JWKS once.
        let mut refresh = false;
        loop {
            let jwks = self.keys(&d.jwks_uri, refresh).await?;
            match jwt::select(&jwks, parts.header.kid.as_deref(), alg) {
                Ok(key) => {
                    jwt::verify(alg, key, parts.signing_input.as_bytes(), &parts.signature)?;
                    break;
                }
                Err(VerifyError::UnknownKey) if !refresh => refresh = true,
                Err(e) => return Err(e.into()),
            }
        }
        let claims: IdClaims = serde_json::from_slice(&parts.payload).map_err(|e| OidcError::IdToken(e.to_string()))?;
        check_claims(&claims, &d.issuer, client_id, nonce, now)?;
        Ok(claims)
    }

    async fn github_identity(&self, api: &str, token: &str) -> Result<Identity, OidcError> {
        #[derive(Deserialize)]
        struct User {
            id: u64,
        }
        #[derive(Deserialize)]
        struct Email {
            email: String,
            #[serde(default)]
            primary: bool,
            #[serde(default)]
            verified: bool,
        }
        let api = api.trim_end_matches('/');
        let get =
            |path: &str| self.http.get(format!("{api}{path}")).bearer_auth(token).header(header::ACCEPT, "application/vnd.github+json");
        let resp = get("/user").send().await?;
        if !resp.status().is_success() {
            return Err(OidcError::Token(format!("GitHub /user answered {}", resp.status())));
        }
        let user: User = resp.json().await?;
        // Without the user:email grant the list is unavailable; the login still works, the
        // principal just carries no email.
        let emails: Vec<Email> = match get("/user/emails").send().await {
            Ok(r) if r.status().is_success() => r.json().await.unwrap_or_default(),
            _ => Vec::new(),
        };
        let email = emails
            .iter()
            .find(|e| e.primary && e.verified)
            .or_else(|| emails.iter().find(|e| e.verified))
            .map(|e| e.email.to_ascii_lowercase());
        Ok(Identity {
            sub: format!("github:{}", user.id),
            email_verified: email.is_some(),
            email,
            groups: Vec::new(),
            amr: vec!["oidc".into()],
        })
    }
}

fn check_claims(c: &IdClaims, discovered_issuer: &str, client_id: &str, nonce: &str, now: u64) -> Result<(), OidcError> {
    let expected = if discovered_issuer.contains("{tenantid}") {
        let tid = c.tid.as_deref().ok_or_else(|| OidcError::IdToken("no tid for a multi-tenant issuer".into()))?;
        discovered_issuer.replace("{tenantid}", tid)
    } else {
        discovered_issuer.to_owned()
    };
    if c.iss != expected {
        return Err(OidcError::IdToken(format!("issuer `{}` is not `{expected}`", c.iss)));
    }
    let auds: Vec<&str> = match &c.aud {
        Audience::One(a) => vec![a.as_str()],
        Audience::Many(v) => v.iter().map(String::as_str).collect(),
    };
    if !auds.contains(&client_id) {
        return Err(OidcError::IdToken("audience does not include this client".into()));
    }
    if (auds.len() > 1 || c.azp.is_some()) && c.azp.as_deref() != Some(client_id) {
        return Err(OidcError::IdToken("authorized party is not this client".into()));
    }
    if now >= c.exp + LEEWAY_SECS {
        return Err(OidcError::IdToken("expired".into()));
    }
    if c.iat.is_some_and(|iat| iat > now + LEEWAY_SECS) {
        return Err(OidcError::IdToken("issued in the future".into()));
    }
    if c.sub.is_empty() {
        return Err(OidcError::IdToken("empty subject".into()));
    }
    match &c.nonce {
        Some(n) if bool::from(n.as_bytes().ct_eq(nonce.as_bytes())) => Ok(()),
        _ => Err(OidcError::Nonce),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims(iss: &str, aud: Audience, azp: Option<&str>, nonce: Option<&str>) -> IdClaims {
        IdClaims {
            iss: iss.into(),
            sub: "u1".into(),
            aud,
            azp: azp.map(str::to_owned),
            exp: 2000,
            iat: Some(1000),
            nonce: nonce.map(str::to_owned),
            email: None,
            email_verified: None,
            groups: None,
            tid: Some("t-123".into()),
        }
    }

    #[test]
    fn claims_checks() {
        let one = |a: &str| Audience::One(a.into());
        let ok = claims("https://idp", one("cid"), None, Some("n"));
        assert_eq!(check_claims(&ok, "https://idp", "cid", "n", 1500), Ok(()));
        assert_eq!(check_claims(&ok, "https://idp", "cid", "m", 1500), Err(OidcError::Nonce));
        assert!(check_claims(&claims("https://idp", one("cid"), None, None), "https://idp", "cid", "n", 1500).is_err());
        assert!(check_claims(&ok, "https://other", "cid", "n", 1500).is_err());
        assert!(check_claims(&ok, "https://idp", "other", "n", 1500).is_err());
        assert!(check_claims(&ok, "https://idp", "cid", "n", 2000 + LEEWAY_SECS).is_err());
        assert!(check_claims(&ok, "https://idp", "cid", "n", 1000 - LEEWAY_SECS - 1).is_err());
        let many = claims("https://idp", Audience::Many(vec!["cid".into(), "x".into()]), None, Some("n"));
        assert!(check_claims(&many, "https://idp", "cid", "n", 1500).is_err(), "multi-aud needs azp");
        let many = claims("https://idp", Audience::Many(vec!["cid".into(), "x".into()]), Some("cid"), Some("n"));
        assert_eq!(check_claims(&many, "https://idp", "cid", "n", 1500), Ok(()));
    }

    #[test]
    fn microsoft_multi_tenant_issuer_is_filled_from_tid() {
        let tmpl = "https://login.microsoftonline.com/{tenantid}/v2.0";
        let good = claims("https://login.microsoftonline.com/t-123/v2.0", Audience::One("cid".into()), None, Some("n"));
        assert_eq!(check_claims(&good, tmpl, "cid", "n", 1500), Ok(()));
        let other = claims("https://login.microsoftonline.com/t-999/v2.0", Audience::One("cid".into()), None, Some("n"));
        assert!(check_claims(&other, tmpl, "cid", "n", 1500).is_err(), "token from another tenant's issuer");
    }

    #[test]
    fn pkce_matches_rfc7636_appendix_b() {
        assert_eq!(pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }
}
