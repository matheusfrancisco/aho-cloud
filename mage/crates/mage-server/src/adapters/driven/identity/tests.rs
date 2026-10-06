#![allow(clippy::expect_used)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde_json::{Value, json};
use shared_auth::{GitHubConfig, GitHubOAuth, GoogleConfig, GoogleOidc, Pkce};
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::ports::auth::{AuthorizeParams, IdentityError, IdentityProvider, IdentityProviderKind};

fn query(url: &str) -> HashMap<String, String> {
    url::Url::parse(url).expect("valid url").query_pairs().into_owned().collect()
}

fn params(pkce: &Pkce, redirect: &str) -> AuthorizeParams {
    AuthorizeParams { state: "state-1".into(), pkce_challenge: pkce.challenge.clone(), redirect_uri: redirect.into() }
}

// ---------------------------------------------------------------------------------------------
// GitHub
// ---------------------------------------------------------------------------------------------

const GH_TOKEN: &str = "gho_test_user_token";
const GH_REDIRECT: &str = "https://mage.example/auth/github/callback";
const INSTALLATION_TOKEN: &str = "ghs_installation_token";

fn github(server: &MockServer) -> GitHubIdentity {
    let mut config = GitHubConfig::new("client-123", "secret-456");
    config.authorize_url = format!("{}/login/oauth/authorize", server.uri());
    config.token_url = format!("{}/login/oauth/access_token", server.uri());
    config.api_base = server.uri();
    GitHubIdentity::new(GitHubOAuth::new(config, reqwest::Client::new()))
}

async fn mount_github_token(server: &MockServer, verifier: &str) {
    Mock::given(method("POST"))
        .and(path("/login/oauth/access_token"))
        .and(body_string_contains("client_secret=secret-456"))
        .and(body_string_contains("code=the-code"))
        .and(body_string_contains(format!("code_verifier={verifier}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": GH_TOKEN, "token_type": "bearer", "scope": ""
        })))
        .expect(1)
        .mount(server)
        .await;
}

async fn mount_github_user(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/user"))
        .and(header("authorization", format!("Bearer {GH_TOKEN}").as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 583231, "login": "octocat", "name": "The Octocat",
            "avatar_url": "https://avatars.githubusercontent.com/u/583231?v=4",
            "email": "public@example.com", "type": "User"
        })))
        .expect(1)
        .mount(server)
        .await;
}

async fn mount_github_emails(server: &MockServer, response: ResponseTemplate) {
    Mock::given(method("GET"))
        .and(path("/user/emails"))
        .and(header("authorization", format!("Bearer {GH_TOKEN}").as_str()))
        .respond_with(response)
        .expect(1)
        .mount(server)
        .await;
}

async fn mount_github_token_error(server: &MockServer, code: &str) {
    Mock::given(method("POST"))
        .and(path("/login/oauth/access_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "error": code, "error_description": "nope"
        })))
        .mount(server)
        .await;
}

struct StaticToken(Option<&'static str>);

#[async_trait]
impl LookupToken for StaticToken {
    async fn token(&self) -> Result<Option<String>, IdentityError> {
        Ok(self.0.map(str::to_owned))
    }
}

struct FailingToken;

#[async_trait]
impl LookupToken for FailingToken {
    async fn token(&self) -> Result<Option<String>, IdentityError> {
        Err(IdentityError::Unavailable("installation token: github down".into()))
    }
}

#[tokio::test]
async fn github_authorize_url_maps_port_params() {
    let server = MockServer::start().await;
    let pkce = Pkce::generate();
    let provider = github(&server);
    assert_eq!(provider.kind(), IdentityProviderKind::GitHub);
    let url = provider.authorize_url(&params(&pkce, GH_REDIRECT));
    assert!(url.starts_with(&format!("{}/login/oauth/authorize?", server.uri())));
    let q = query(&url);
    assert_eq!(q["client_id"], "client-123");
    assert_eq!(q["state"], "state-1");
    assert_eq!(q["code_challenge"], pkce.challenge);
    assert_eq!(q["code_challenge_method"], "S256");
    assert_eq!(q["redirect_uri"], GH_REDIRECT);
}

#[tokio::test]
async fn github_exchange_sends_verifier_and_returns_identity() {
    let server = MockServer::start().await;
    let pkce = Pkce::generate();
    mount_github_token(&server, &pkce.verifier).await;
    mount_github_user(&server).await;
    mount_github_emails(
        &server,
        ResponseTemplate::new(200).set_body_json(json!([
            {"email": "old@example.com", "primary": false, "verified": true, "visibility": null},
            {"email": "octo@example.com", "primary": true, "verified": true, "visibility": "private"}
        ])),
    )
    .await;

    let identity = github(&server).exchange("the-code", &pkce.verifier, GH_REDIRECT).await.expect("login");

    assert_eq!(identity.provider, IdentityProviderKind::GitHub);
    assert_eq!(identity.subject, "583231");
    assert_eq!(identity.login, "octocat");
    assert_eq!(identity.name.as_deref(), Some("The Octocat"));
    assert_eq!(identity.email.as_deref(), Some("octo@example.com"));
    assert_eq!(identity.avatar_url.as_deref(), Some("https://avatars.githubusercontent.com/u/583231?v=4"));
    assert!(!format!("{identity:?}").contains(GH_TOKEN));
}

#[tokio::test]
async fn github_exchange_without_readable_email_has_no_email() {
    let server = MockServer::start().await;
    let pkce = Pkce::generate();
    mount_github_token(&server, &pkce.verifier).await;
    mount_github_user(&server).await;
    mount_github_emails(
        &server,
        ResponseTemplate::new(403).set_body_json(json!({"message": "Resource not accessible by integration"})),
    )
    .await;

    let identity = github(&server).exchange("the-code", &pkce.verifier, GH_REDIRECT).await.expect("login");

    assert_eq!(identity.subject, "583231");
    assert_eq!(identity.email, None);
}

#[tokio::test]
async fn github_rejected_code_or_consent_is_denied() {
    for code in ["bad_verification_code", "access_denied"] {
        let server = MockServer::start().await;
        mount_github_token_error(&server, code).await;
        let pkce = Pkce::generate();
        let result = github(&server).exchange("the-code", &pkce.verifier, GH_REDIRECT).await;
        assert_eq!(result, Err(IdentityError::Denied), "{code}");
    }
}

#[tokio::test]
async fn github_lookup_unknown_login_is_none() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/users/ghost-user"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"message": "Not Found"})))
        .expect(1)
        .mount(&server)
        .await;

    assert_eq!(github(&server).lookup_login("ghost-user").await, Ok(None));
}

#[tokio::test]
async fn github_lookup_sends_installation_token() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/users/octocat"))
        .and(header("authorization", format!("Bearer {INSTALLATION_TOKEN}").as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 583231, "login": "octocat", "name": "The Octocat",
            "avatar_url": "https://avatars.githubusercontent.com/u/583231?v=4",
            "email": "public@example.com", "type": "User"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let provider = github(&server).with_lookup_token(Arc::new(StaticToken(Some(INSTALLATION_TOKEN))));
    let identity = provider.lookup_login("octocat").await.expect("lookup").expect("known user");

    assert_eq!(identity.provider, IdentityProviderKind::GitHub);
    assert_eq!(identity.subject, "583231");
    assert_eq!(identity.login, "octocat");
    assert_eq!(identity.email, None, "public profile emails are unverified");
}

#[tokio::test]
async fn github_lookup_without_token_is_anonymous() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/users/octocat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 583231, "login": "octocat", "type": "User"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let provider = github(&server).with_lookup_token(Arc::new(StaticToken(None)));
    let identity = provider.lookup_login("octocat").await.expect("lookup").expect("known user");
    assert_eq!(identity.subject, "583231");

    let requests = server.received_requests().await.expect("recording enabled");
    assert!(requests.iter().all(|r| !r.headers.contains_key("authorization")));
}

#[tokio::test]
async fn github_lookup_token_failure_propagates_without_calling_github() {
    let server = MockServer::start().await;
    let provider = github(&server).with_lookup_token(Arc::new(FailingToken));

    let result = provider.lookup_login("octocat").await;

    assert!(matches!(result, Err(IdentityError::Unavailable(_))), "{result:?}");
    assert!(server.received_requests().await.expect("recording enabled").is_empty());
}

#[tokio::test]
async fn github_login_config_overrides_endpoints_and_uses_secret() {
    let server = MockServer::start().await;
    let config: GitHubLoginConfig = toml::from_str(&format!(
        "client_id = \"client-123\"\napi_base = \"{0}\"\nauthorize_url = \"{0}/login/oauth/authorize\"\n\
         token_url = \"{0}/login/oauth/access_token\"\n",
        server.uri()
    ))
    .expect("config");
    let provider = config.build("secret-456", reqwest::Client::new());
    let pkce = Pkce::generate();
    mount_github_token(&server, &pkce.verifier).await;
    mount_github_user(&server).await;
    mount_github_emails(&server, ResponseTemplate::new(200).set_body_json(json!([]))).await;

    assert!(provider.authorize_url(&params(&pkce, GH_REDIRECT)).starts_with(&server.uri()));
    let identity = provider.exchange("the-code", &pkce.verifier, GH_REDIRECT).await.expect("login");
    assert_eq!(identity.subject, "583231");
    let requests = server.received_requests().await.expect("recording enabled");
    let user_agent = requests
        .iter()
        .find(|r| r.url.path() == "/user")
        .and_then(|r| r.headers.get("user-agent"))
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    assert_eq!(user_agent.as_deref(), Some(super::USER_AGENT));
}

#[test]
fn github_login_config_defaults_to_github_com_and_rejects_unknown_keys() {
    let config: GitHubLoginConfig = toml::from_str("client_id = \"abc\"").expect("config");
    let url = config.build("s", reqwest::Client::new()).authorize_url(&params(&Pkce::generate(), GH_REDIRECT));
    assert!(url.starts_with("https://github.com/login/oauth/authorize?"), "{url}");
    assert!(toml::from_str::<GitHubLoginConfig>("client_id = \"abc\"\nclient_secret = \"x\"").is_err());
}

// ---------------------------------------------------------------------------------------------
// Google
// ---------------------------------------------------------------------------------------------

/// Test-only RSA-2048 key shared with `shared/auth/tests/google.rs`.
const PRIVATE_KEY_PEM: &[u8] = include_bytes!("../../../../../../../shared/auth/tests/fixtures/google_test_rsa.pem");
/// Its public modulus and exponent, base64url (JWK `n`, `e`).
const MODULUS: &str = "3oKgQKI7Zk2r7c6Pd_momQHNzJNgi16wO4A43tdPb2cxHGay-bAVTM8s4sUrTyyJJhyyeF68s7PgpBKaj30qgvWOk-iTbe54j1d2tiDRQ5S9g9662mjv-8xO6lIdUYeNZO7aZzvGAHl4PY2EUoyDP-q7WnhS0zvGH8qTh6-ak1BDTQV-CSpiiVL0zC0cLTAsi6iRT3i-w3o1vcPq4qS2w27TLQjJs9p6nj1MrK0hwNCnVcS1ZZ8vntqWQlBtp6N7zNbeBBJClt5jDTKgAHOtBDWBeYoWEa_yhLpI5vK4dtECZsDbLS2hCMOB45BNOb_etdGHJTFEPNxHg4igiFj7Gw";
const EXPONENT: &str = "AQAB";
const KID: &str = "google-kid-1";
const GOOGLE_CLIENT_ID: &str = "mage-client.apps.googleusercontent.com";
const GOOGLE_REDIRECT: &str = "https://mage.example/auth/google/callback";

fn google(server: &MockServer) -> GoogleIdentity {
    let mut config = GoogleConfig::new(GOOGLE_CLIENT_ID, "google-secret");
    config.authorize_url = format!("{}/o/oauth2/v2/auth", server.uri());
    config.token_url = format!("{}/token", server.uri());
    config.jwks_url = format!("{}/oauth2/v3/certs", server.uri());
    GoogleIdentity::new(Arc::new(GoogleOidc::new(config, reqwest::Client::new())))
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).expect("clock")
}

fn google_claims(nonce: &str) -> Value {
    json!({
        "iss": "https://accounts.google.com",
        "azp": GOOGLE_CLIENT_ID,
        "aud": GOOGLE_CLIENT_ID,
        "sub": "110169484474386276334",
        "email": "ada@example.com",
        "email_verified": true,
        "nonce": nonce,
        "name": "Ada Lovelace",
        "picture": "https://lh3.googleusercontent.com/a/ada",
        "iat": now(),
        "exp": now() + 3600,
    })
}

fn sign(claims: &Value) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(KID.to_owned());
    let key = EncodingKey::from_rsa_pem(PRIVATE_KEY_PEM).expect("test key");
    encode(&header, claims, &key).expect("sign")
}

async fn mount_google(server: &MockServer, verifier: &str, id_token: &str) {
    Mock::given(method("GET"))
        .and(path("/oauth2/v3/certs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"keys": [
            {"kty": "RSA", "alg": "RS256", "use": "sig", "kid": KID, "n": MODULUS, "e": EXPONENT}
        ]})))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("grant_type=authorization_code"))
        .and(body_string_contains("code=the-code"))
        .and(body_string_contains("client_secret=google-secret"))
        .and(body_string_contains(format!("code_verifier={verifier}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "ya29.test", "expires_in": 3599, "token_type": "Bearer",
            "scope": "openid email profile", "id_token": id_token
        })))
        .expect(1)
        .mount(server)
        .await;
}

#[tokio::test]
async fn google_authorize_nonce_is_the_nonce_exchange_expects() {
    let server = MockServer::start().await;
    let provider = google(&server);
    assert_eq!(provider.kind(), IdentityProviderKind::Google);
    let pkce = Pkce::generate();
    let q = query(&provider.authorize_url(&params(&pkce, GOOGLE_REDIRECT)));
    assert_eq!(q["code_challenge"], pkce.challenge);
    assert_eq!(q["state"], "state-1");
    let nonce = q["nonce"].clone();
    assert!(!nonce.is_empty());
    assert_ne!(nonce, pkce.challenge, "the nonce is not the public challenge itself");

    mount_google(&server, &pkce.verifier, &sign(&google_claims(&nonce))).await;
    let identity = provider.exchange("the-code", &pkce.verifier, GOOGLE_REDIRECT).await.expect("login");

    assert_eq!(identity.provider, IdentityProviderKind::Google);
    assert_eq!(identity.subject, "110169484474386276334");
    assert_eq!(identity.login, "ada@example.com");
    assert_eq!(identity.email.as_deref(), Some("ada@example.com"));
    assert_eq!(identity.name.as_deref(), Some("Ada Lovelace"));
    assert_eq!(identity.avatar_url.as_deref(), Some("https://lh3.googleusercontent.com/a/ada"));
}

#[tokio::test]
async fn google_id_token_for_another_login_nonce_is_invalid() {
    let server = MockServer::start().await;
    let provider = google(&server);
    let ours = Pkce::generate();
    let other = Pkce::generate();
    let other_nonce = query(&provider.authorize_url(&params(&other, GOOGLE_REDIRECT)))["nonce"].clone();

    mount_google(&server, &ours.verifier, &sign(&google_claims(&other_nonce))).await;
    let result = provider.exchange("the-code", &ours.verifier, GOOGLE_REDIRECT).await;

    assert!(matches!(result, Err(IdentityError::Invalid(_))), "{result:?}");
}

#[tokio::test]
async fn google_unverified_email_is_denied() {
    let server = MockServer::start().await;
    let provider = google(&server);
    let pkce = Pkce::generate();
    let nonce = query(&provider.authorize_url(&params(&pkce, GOOGLE_REDIRECT)))["nonce"].clone();
    let mut claims = google_claims(&nonce);
    claims["email_verified"] = json!(false);

    mount_google(&server, &pkce.verifier, &sign(&claims)).await;
    let result = provider.exchange("the-code", &pkce.verifier, GOOGLE_REDIRECT).await;

    assert_eq!(result, Err(IdentityError::Denied));
}

#[tokio::test]
async fn google_lookup_login_resolves_nothing() {
    let server = MockServer::start().await;
    assert_eq!(google(&server).lookup_login("ada@example.com").await, Ok(None));
    assert!(server.received_requests().await.expect("recording enabled").is_empty());
}

#[test]
fn google_login_config_uses_google_endpoints_and_client_id() {
    let config: GoogleLoginConfig = toml::from_str("client_id = \"abc.apps.googleusercontent.com\"").expect("config");
    let pkce = Pkce::generate();
    let url = config.build("s", reqwest::Client::new()).authorize_url(&params(&pkce, GOOGLE_REDIRECT));
    assert!(url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?"), "{url}");
    assert_eq!(query(&url)["client_id"], "abc.apps.googleusercontent.com");
    assert!(!url.contains("client_secret"));
}
