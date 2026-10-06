//! Shared pieces of the GitHub adapter tests: client construction, app JWT verification and a
//! counting token minter.

pub mod emulator;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use mage_core::ports::clock::Clock;
use mage_core::testing::FakeClock;
use mage_infra::github::{AppCredentials, GitHubClient, GitHubConfig};
use serde::Deserialize;
use serde_json::json;
use time::format_description::well_known::Rfc3339;
use url::Url;
use wiremock::{Request, Respond, ResponseTemplate};

pub const CLIENT_ID: &str = "Iv1.mage-test";
const PRIVATE_KEY: &str = include_str!("../fixtures/github_app_key.pem");
const PUBLIC_KEY: &[u8] = include_bytes!("../fixtures/github_app_key.pub.pem");

pub fn app() -> AppCredentials {
    AppCredentials { client_id: CLIENT_ID.into(), private_key_pem: PRIVATE_KEY.into() }
}

/// A client whose API root is `{server}{api_prefix}` and web root `{server}/web/`.
pub fn client_at(server_uri: &str, api_prefix: &str, clock: Arc<FakeClock>, request_timeout_ms: u64) -> GitHubClient {
    let config = GitHubConfig {
        api_base: Url::parse(&format!("{server_uri}{api_prefix}")).unwrap(),
        web_base: Url::parse(&format!("{server_uri}/web/")).unwrap(),
        connect_timeout_ms: 2_000,
        request_timeout_ms,
    };
    GitHubClient::new(&config, app(), clock).unwrap()
}

pub fn client(server_uri: &str, clock: Arc<FakeClock>) -> GitHubClient {
    client_at(server_uri, "/", clock, 5_000)
}

#[derive(Debug, Deserialize)]
pub struct AppClaims {
    pub iat: i64,
    pub exp: i64,
    pub iss: String,
}

/// Checks an app JWT the way GitHub does: RS256 under the app's public key, `iss` = client id,
/// `iat <= now < exp` on the test clock, lifetime at most 10 minutes.
pub fn verify_app_jwt(jwt: &str, now: time::OffsetDateTime) -> Result<AppClaims, String> {
    let key = DecodingKey::from_rsa_pem(PUBLIC_KEY).map_err(|e| e.to_string())?;
    let mut validation = Validation::new(Algorithm::RS256);
    validation.validate_exp = false;
    validation.set_issuer(&[CLIENT_ID]);
    validation.set_required_spec_claims(&["iat", "exp", "iss"]);
    let claims = jsonwebtoken::decode::<AppClaims>(jwt, &key, &validation).map_err(|e| e.to_string())?.claims;
    let now = now.unix_timestamp();
    if !(claims.iat <= now && now < claims.exp && claims.exp - claims.iat <= 600) {
        return Err(format!("JWT window iat={} exp={} does not cover now={now}", claims.iat, claims.exp));
    }
    Ok(claims)
}

pub fn bearer(request: &Request) -> Option<String> {
    request.headers.get("authorization")?.to_str().ok()?.strip_prefix("Bearer ").map(str::to_owned)
}

/// `POST /app/installations/{id}/access_tokens`: checks the app JWT, answers `ghs_{n}` valid one
/// hour from the test clock (with `delay`), counts mints.
#[derive(Clone)]
pub struct Minter {
    pub clock: Arc<FakeClock>,
    pub count: Arc<AtomicUsize>,
    pub delay: Duration,
}

impl Minter {
    pub fn new(clock: Arc<FakeClock>) -> Self {
        Self { clock, count: Arc::new(AtomicUsize::new(0)), delay: Duration::ZERO }
    }

    pub fn minted(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }
}

impl Respond for Minter {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let now = self.clock.now();
        if let Err(e) = bearer(request).ok_or_else(|| "no bearer".to_owned()).and_then(|jwt| verify_app_jwt(&jwt, now)) {
            return ResponseTemplate::new(401).set_body_json(json!({ "message": format!("A JSON web token could not be decoded: {e}") }));
        }
        let n = self.count.fetch_add(1, Ordering::SeqCst) + 1;
        let expires_at = (now + Duration::from_secs(3600)).format(&Rfc3339).unwrap();
        ResponseTemplate::new(201)
            .set_body_json(json!({ "token": format!("ghs_{n}"), "expires_at": expires_at, "permissions": { "contents": "read", "metadata": "read" } }))
            .set_delay(self.delay)
    }
}
