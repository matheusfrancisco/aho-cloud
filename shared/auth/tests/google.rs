use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde_json::{Value, json};
use shared_auth::pkce::nonce_for_verifier;
use shared_auth::{AuthError, AuthorizeParams, GoogleConfig, GoogleOidc, Pkce, Provider};
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Test-only RSA-2048 key generated locally with `openssl genrsa -traditional 2048`.
const PRIVATE_KEY_PEM: &[u8] = include_bytes!("fixtures/google_test_rsa.pem");
/// Its public modulus and exponent, base64url (JWK `n`, `e`).
const MODULUS: &str = "3oKgQKI7Zk2r7c6Pd_momQHNzJNgi16wO4A43tdPb2cxHGay-bAVTM8s4sUrTyyJJhyyeF68s7PgpBKaj30qgvWOk-iTbe54j1d2tiDRQ5S9g9662mjv-8xO6lIdUYeNZO7aZzvGAHl4PY2EUoyDP-q7WnhS0zvGH8qTh6-ak1BDTQV-CSpiiVL0zC0cLTAsi6iRT3i-w3o1vcPq4qS2w27TLQjJs9p6nj1MrK0hwNCnVcS1ZZ8vntqWQlBtp6N7zNbeBBJClt5jDTKgAHOtBDWBeYoWEa_yhLpI5vK4dtECZsDbLS2hCMOB45BNOb_etdGHJTFEPNxHg4igiFj7Gw";
const EXPONENT: &str = "AQAB";

const CLIENT_ID: &str = "mage-client.apps.googleusercontent.com";
const REDIRECT: &str = "https://mage.example/auth/google/callback";
const NONCE: &str = "n-0S6_WzA2Mj";

fn provider(server: &MockServer) -> GoogleOidc {
    let mut config = GoogleConfig::new(CLIENT_ID, "google-secret");
    config.authorize_url = format!("{}/o/oauth2/v2/auth", server.uri());
    config.token_url = format!("{}/token", server.uri());
    config.jwks_url = format!("{}/oauth2/v3/certs", server.uri());
    GoogleOidc::new(config, reqwest::Client::new())
}

fn jwks(kids: &[&str]) -> Value {
    let keys: Vec<Value> = kids
        .iter()
        .map(|kid| {
            json!({"kty": "RSA", "alg": "RS256", "use": "sig", "kid": kid, "n": MODULUS, "e": EXPONENT})
        })
        .collect();
    json!({ "keys": keys })
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).expect("clock")
}

fn claims() -> Value {
    json!({
        "iss": "https://accounts.google.com",
        "azp": CLIENT_ID,
        "aud": CLIENT_ID,
        "sub": "110169484474386276334",
        "email": "ada@example.com",
        "email_verified": true,
        "nonce": NONCE,
        "name": "Ada Lovelace",
        "picture": "https://lh3.googleusercontent.com/a/ada",
        "iat": now(),
        "exp": now() + 3600,
    })
}

fn sign(kid: &str, claims: &Value) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(kid.to_owned());
    let key = EncodingKey::from_rsa_pem(PRIVATE_KEY_PEM).expect("test key");
    encode(&header, claims, &key).expect("sign")
}

fn with(mut base: Value, key: &str, value: Value) -> Value {
    base[key] = value;
    base
}

async fn mount_jwks(server: &MockServer, kids: &[&str]) {
    Mock::given(method("GET"))
        .and(path("/oauth2/v3/certs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(jwks(kids)))
        .mount(server)
        .await;
}

async fn mount_token(server: &MockServer, verifier: &str, id_token: &str) {
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

async fn jwks_fetches(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.url.path() == "/oauth2/v3/certs")
        .count()
}

#[tokio::test]
async fn authorize_url_carries_nonce_pkce_and_openid_scope() {
    let server = MockServer::start().await;
    let pkce = Pkce::generate();
    let url = provider(&server).authorize_url(
        &AuthorizeParams {
            state: "state-1".into(),
            pkce_challenge: pkce.challenge.clone(),
            redirect_uri: REDIRECT.into(),
        },
        NONCE,
    );
    let parsed = url::Url::parse(&url).expect("valid url");
    let query: HashMap<String, String> = parsed.query_pairs().into_owned().collect();
    assert_eq!(query["response_type"], "code");
    assert_eq!(query["client_id"], CLIENT_ID);
    assert_eq!(query["redirect_uri"], REDIRECT);
    assert_eq!(query["scope"], "openid email profile");
    assert_eq!(query["state"], "state-1");
    assert_eq!(query["nonce"], NONCE);
    assert_eq!(query["code_challenge"], pkce.challenge);
    assert_eq!(query["code_challenge_method"], "S256");
}

#[tokio::test]
async fn exchange_verifies_id_token_and_returns_identity() {
    let server = MockServer::start().await;
    let pkce = Pkce::generate();
    mount_jwks(&server, &["k1"]).await;
    mount_token(&server, &pkce.verifier, &sign("k1", &claims())).await;

    let identity = provider(&server)
        .exchange("the-code", &pkce.verifier, REDIRECT, NONCE)
        .await
        .expect("login succeeds");

    assert_eq!(identity.provider, Provider::Google);
    assert_eq!(identity.subject, "110169484474386276334");
    assert_eq!(identity.login, "ada@example.com");
    assert_eq!(identity.email.as_deref(), Some("ada@example.com"));
    assert_eq!(identity.name.as_deref(), Some("Ada Lovelace"));
    assert_eq!(identity.avatar_url.as_deref(), Some("https://lh3.googleusercontent.com/a/ada"));
}

#[tokio::test]
async fn bare_issuer_and_string_email_verified_are_accepted() {
    let server = MockServer::start().await;
    mount_jwks(&server, &["k1"]).await;
    let token = sign(
        "k1",
        &with(with(claims(), "iss", json!("accounts.google.com")), "email_verified", json!("true")),
    );
    let identity = provider(&server).verify_id_token(&token, NONCE).await;
    assert_eq!(identity.map(|i| i.subject), Ok("110169484474386276334".to_owned()));
}

#[tokio::test]
async fn derived_nonce_round_trips() {
    let server = MockServer::start().await;
    let pkce = Pkce::generate();
    let nonce = nonce_for_verifier(&pkce.verifier);
    mount_jwks(&server, &["k1"]).await;
    mount_token(&server, &pkce.verifier, &sign("k1", &with(claims(), "nonce", json!(nonce))))
        .await;

    let result = provider(&server)
        .exchange("the-code", &pkce.verifier, REDIRECT, &nonce_for_verifier(&pkce.verifier))
        .await;
    assert!(result.is_ok(), "{result:?}");
}

#[tokio::test]
async fn wrong_audience_is_invalid() {
    let server = MockServer::start().await;
    mount_jwks(&server, &["k1"]).await;
    let token = sign("k1", &with(claims(), "aud", json!("someone-else.apps.googleusercontent.com")));
    let result = provider(&server).verify_id_token(&token, NONCE).await;
    assert!(matches!(result, Err(AuthError::Invalid(_))), "{result:?}");
}

#[tokio::test]
async fn wrong_issuer_is_invalid() {
    let server = MockServer::start().await;
    mount_jwks(&server, &["k1"]).await;
    let token = sign("k1", &with(claims(), "iss", json!("https://evil.example")));
    let result = provider(&server).verify_id_token(&token, NONCE).await;
    assert!(matches!(result, Err(AuthError::Invalid(_))), "{result:?}");
}

#[tokio::test]
async fn wrong_or_missing_nonce_is_invalid() {
    let server = MockServer::start().await;
    let pkce = Pkce::generate();
    mount_jwks(&server, &["k1"]).await;
    mount_token(&server, &pkce.verifier, &sign("k1", &claims())).await;

    let google = provider(&server);
    let result = google.exchange("the-code", &pkce.verifier, REDIRECT, "other-nonce").await;
    assert!(matches!(&result, Err(AuthError::Invalid(m)) if m.contains("nonce")), "{result:?}");

    let mut no_nonce = claims();
    no_nonce.as_object_mut().map(|o| o.remove("nonce"));
    let result = google.verify_id_token(&sign("k1", &no_nonce), NONCE).await;
    assert!(matches!(&result, Err(AuthError::Invalid(m)) if m.contains("nonce")), "{result:?}");
}

#[tokio::test]
async fn expired_token_is_invalid_beyond_leeway() {
    let server = MockServer::start().await;
    mount_jwks(&server, &["k1"]).await;
    let google = provider(&server);

    let expired = sign("k1", &with(claims(), "exp", json!(now() - 120)));
    let result = google.verify_id_token(&expired, NONCE).await;
    assert!(matches!(result, Err(AuthError::Invalid(_))), "{result:?}");

    let within_leeway = sign("k1", &with(claims(), "exp", json!(now() - 5)));
    assert!(google.verify_id_token(&within_leeway, NONCE).await.is_ok());
}

#[tokio::test]
async fn unverified_or_missing_email_is_denied() {
    let server = MockServer::start().await;
    mount_jwks(&server, &["k1"]).await;
    let google = provider(&server);

    let unverified = sign("k1", &with(claims(), "email_verified", json!(false)));
    assert_eq!(google.verify_id_token(&unverified, NONCE).await, Err(AuthError::Denied));

    let mut no_flag = claims();
    no_flag.as_object_mut().map(|o| o.remove("email_verified"));
    assert_eq!(google.verify_id_token(&sign("k1", &no_flag), NONCE).await, Err(AuthError::Denied));

    let mut no_email = claims();
    no_email.as_object_mut().map(|o| o.remove("email"));
    assert_eq!(google.verify_id_token(&sign("k1", &no_email), NONCE).await, Err(AuthError::Denied));
}

#[tokio::test]
async fn tampered_signature_is_invalid() {
    let server = MockServer::start().await;
    mount_jwks(&server, &["k1"]).await;
    let token = sign("k1", &claims());
    let forged_payload = sign("k1", &with(claims(), "sub", json!("attacker")));
    let mut parts: Vec<&str> = token.split('.').collect();
    parts[1] = forged_payload.split('.').nth(1).expect("payload");
    let tampered = parts.join(".");

    let result = provider(&server).verify_id_token(&tampered, NONCE).await;
    assert!(matches!(result, Err(AuthError::Invalid(_))), "{result:?}");
}

#[tokio::test]
async fn jwks_is_cached_across_logins() {
    let server = MockServer::start().await;
    mount_jwks(&server, &["k1"]).await;
    let google = provider(&server);
    let token = sign("k1", &claims());

    assert!(google.verify_id_token(&token, NONCE).await.is_ok());
    assert!(google.verify_id_token(&token, NONCE).await.is_ok());
    assert_eq!(jwks_fetches(&server).await, 1);
}

#[tokio::test]
async fn unknown_kid_triggers_one_jwks_refresh() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/oauth2/v3/certs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(jwks(&["k1"])))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    mount_jwks(&server, &["k1", "k2"]).await;
    let google = provider(&server);

    assert!(google.verify_id_token(&sign("k1", &claims()), NONCE).await.is_ok());
    assert_eq!(jwks_fetches(&server).await, 1);

    let rotated = google.verify_id_token(&sign("k2", &claims()), NONCE).await;
    assert!(rotated.is_ok(), "{rotated:?}");
    assert_eq!(jwks_fetches(&server).await, 2);

    assert!(google.verify_id_token(&sign("k2", &claims()), NONCE).await.is_ok());
    assert_eq!(jwks_fetches(&server).await, 2, "refreshed set is cached");

    let unknown = google.verify_id_token(&sign("k3", &claims()), NONCE).await;
    assert!(matches!(&unknown, Err(AuthError::Invalid(m)) if m.contains("unknown key")), "{unknown:?}");
}

#[tokio::test]
async fn rejected_code_is_denied() {
    let server = MockServer::start().await;
    let pkce = Pkce::generate();
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": "invalid_grant", "error_description": "Bad Request"
        })))
        .mount(&server)
        .await;

    let result = provider(&server).exchange("the-code", &pkce.verifier, REDIRECT, NONCE).await;
    assert_eq!(result, Err(AuthError::Denied));
}

#[tokio::test]
async fn jwks_outage_is_unavailable() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/oauth2/v3/certs"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;

    let result = provider(&server).verify_id_token(&sign("k1", &claims()), NONCE).await;
    assert!(matches!(result, Err(AuthError::Unavailable(_))), "{result:?}");
}
