use std::collections::HashMap;

use serde_json::json;
use shared_auth::{AuthError, AuthorizeParams, GitHubConfig, GitHubOAuth, Pkce, Provider};
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKEN: &str = "gho_test_user_token";
const REDIRECT: &str = "https://mage.example/auth/github/callback";

fn provider(server: &MockServer) -> GitHubOAuth {
    let mut config = GitHubConfig::new("client-123", "secret-456");
    config.authorize_url = format!("{}/login/oauth/authorize", server.uri());
    config.token_url = format!("{}/login/oauth/access_token", server.uri());
    config.api_base = server.uri();
    GitHubOAuth::new(config, reqwest::Client::new())
}

async fn mount_token_ok(server: &MockServer, verifier: &str) {
    Mock::given(method("POST"))
        .and(path("/login/oauth/access_token"))
        .and(header("accept", "application/json"))
        .and(body_string_contains("client_id=client-123"))
        .and(body_string_contains("client_secret=secret-456"))
        .and(body_string_contains("code=the-code"))
        .and(body_string_contains(format!("code_verifier={verifier}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": TOKEN, "token_type": "bearer", "scope": ""
        })))
        .expect(1)
        .mount(server)
        .await;
}

async fn mount_user(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/user"))
        .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 583231, "login": "octocat", "name": "The Octocat",
            "avatar_url": "https://avatars.githubusercontent.com/u/583231?v=4",
            "email": "public@example.com", "type": "User"
        })))
        .expect(1)
        .mount(server)
        .await;
}

async fn mount_emails(server: &MockServer, response: ResponseTemplate) {
    Mock::given(method("GET"))
        .and(path("/user/emails"))
        .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
        .respond_with(response)
        .expect(1)
        .mount(server)
        .await;
}

#[tokio::test]
async fn authorize_url_carries_pkce_state_and_scope() {
    let server = MockServer::start().await;
    let pkce = Pkce::generate();
    let url = provider(&server).authorize_url(&AuthorizeParams {
        state: "st&te=1".into(),
        pkce_challenge: pkce.challenge.clone(),
        redirect_uri: REDIRECT.into(),
    });
    let parsed = url::Url::parse(&url).expect("valid url");
    assert_eq!(parsed.path(), "/login/oauth/authorize");
    let query: HashMap<String, String> = parsed.query_pairs().into_owned().collect();
    assert_eq!(query["client_id"], "client-123");
    assert_eq!(query["redirect_uri"], REDIRECT);
    assert_eq!(query["state"], "st&te=1");
    assert_eq!(query["code_challenge"], pkce.challenge);
    assert_eq!(query["code_challenge_method"], "S256");
    assert_eq!(query["scope"], "read:user user:email");
    assert!(!url.contains("secret-456"));
}

#[tokio::test]
async fn exchange_returns_identity_with_primary_verified_email() {
    let server = MockServer::start().await;
    let pkce = Pkce::generate();
    mount_token_ok(&server, &pkce.verifier).await;
    mount_user(&server).await;
    mount_emails(
        &server,
        ResponseTemplate::new(200).set_body_json(json!([
            {"email": "secondary@example.com", "primary": false, "verified": true, "visibility": null},
            {"email": "octo@example.com", "primary": true, "verified": true, "visibility": "private"}
        ])),
    )
    .await;

    let identity = provider(&server)
        .exchange("the-code", &pkce.verifier, REDIRECT)
        .await
        .expect("login succeeds");

    assert_eq!(identity.provider, Provider::GitHub);
    assert_eq!(identity.subject, "583231");
    assert_eq!(identity.login, "octocat");
    assert_eq!(identity.name.as_deref(), Some("The Octocat"));
    assert_eq!(identity.email.as_deref(), Some("octo@example.com"));
    assert_eq!(
        identity.avatar_url.as_deref(),
        Some("https://avatars.githubusercontent.com/u/583231?v=4")
    );
}

#[tokio::test]
async fn exchange_without_verified_primary_email_has_no_email() {
    let server = MockServer::start().await;
    let pkce = Pkce::generate();
    mount_token_ok(&server, &pkce.verifier).await;
    mount_user(&server).await;
    mount_emails(
        &server,
        ResponseTemplate::new(200).set_body_json(json!([
            {"email": "octo@example.com", "primary": true, "verified": false},
            {"email": "other@example.com", "primary": false, "verified": true}
        ])),
    )
    .await;

    let identity = provider(&server)
        .exchange("the-code", &pkce.verifier, REDIRECT)
        .await
        .expect("login succeeds without email");

    assert_eq!(identity.subject, "583231");
    assert_eq!(identity.email, None, "public /user email and non-primary emails are not used");
}

#[tokio::test]
async fn exchange_without_email_permission_has_no_email() {
    let server = MockServer::start().await;
    let pkce = Pkce::generate();
    mount_token_ok(&server, &pkce.verifier).await;
    mount_user(&server).await;
    mount_emails(
        &server,
        ResponseTemplate::new(403).set_body_json(json!({
            "message": "Resource not accessible by integration"
        })),
    )
    .await;

    let identity = provider(&server)
        .exchange("the-code", &pkce.verifier, REDIRECT)
        .await
        .expect("login succeeds without email");
    assert_eq!(identity.email, None);
}

#[tokio::test]
async fn rejected_code_is_denied_and_no_api_call_is_made() {
    let server = MockServer::start().await;
    let pkce = Pkce::generate();
    Mock::given(method("POST"))
        .and(path("/login/oauth/access_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "error": "bad_verification_code",
            "error_description": "The code passed is incorrect or expired."
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;

    let result = provider(&server).exchange("the-code", &pkce.verifier, REDIRECT).await;
    assert_eq!(result, Err(AuthError::Denied));
}

#[tokio::test]
async fn misconfigured_client_is_invalid_not_denied() {
    let server = MockServer::start().await;
    let pkce = Pkce::generate();
    Mock::given(method("POST"))
        .and(path("/login/oauth/access_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "error": "incorrect_client_credentials"
        })))
        .mount(&server)
        .await;

    let result = provider(&server).exchange("the-code", &pkce.verifier, REDIRECT).await;
    assert!(matches!(result, Err(AuthError::Invalid(m)) if m.contains("incorrect_client_credentials")));
}

#[tokio::test]
async fn revoked_token_on_user_is_denied() {
    let server = MockServer::start().await;
    let pkce = Pkce::generate();
    mount_token_ok(&server, &pkce.verifier).await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({"message": "Bad credentials"})))
        .mount(&server)
        .await;

    let result = provider(&server).exchange("the-code", &pkce.verifier, REDIRECT).await;
    assert_eq!(result, Err(AuthError::Denied));
}

#[tokio::test]
async fn github_outage_is_unavailable() {
    let server = MockServer::start().await;
    let pkce = Pkce::generate();
    Mock::given(method("POST"))
        .and(path("/login/oauth/access_token"))
        .respond_with(ResponseTemplate::new(502))
        .mount(&server)
        .await;

    let result = provider(&server).exchange("the-code", &pkce.verifier, REDIRECT).await;
    assert!(matches!(result, Err(AuthError::Unavailable(_))));
}

#[tokio::test]
async fn malformed_verifier_is_rejected_before_any_request() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let result = provider(&server).exchange("the-code", "too-short", REDIRECT).await;
    assert!(matches!(result, Err(AuthError::Invalid(_))));
}

#[tokio::test]
async fn lookup_login_resolves_stable_id_with_installation_token() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/users/octocat"))
        .and(header("authorization", "Bearer ghs_installation"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 583231, "login": "octocat", "name": null,
            "avatar_url": "https://avatars.githubusercontent.com/u/583231?v=4",
            "email": "public@example.com", "type": "User"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let identity = provider(&server)
        .lookup_login("octocat", Some("ghs_installation"))
        .await
        .expect("lookup succeeds")
        .expect("user exists");
    assert_eq!(identity.subject, "583231");
    assert_eq!(identity.login, "octocat");
    assert_eq!(identity.name, None);
    assert_eq!(identity.email, None, "public profile email is unverified");
}

#[tokio::test]
async fn lookup_unknown_login_or_organization_is_none() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/users/ghost-user"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"message": "Not Found"})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/users/hoophq"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 7, "login": "hoophq", "type": "Organization"
        })))
        .mount(&server)
        .await;

    let github = provider(&server);
    assert_eq!(github.lookup_login("ghost-user", None).await, Ok(None));
    assert_eq!(github.lookup_login("hoophq", None).await, Ok(None));
}

#[tokio::test]
async fn lookup_rejects_path_injection() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let github = provider(&server);
    let too_long = "a".repeat(40);
    for login in ["../user", "a/b", "", "x?y=1", too_long.as_str()] {
        assert!(matches!(github.lookup_login(login, None).await, Err(AuthError::Invalid(_))));
    }
}
