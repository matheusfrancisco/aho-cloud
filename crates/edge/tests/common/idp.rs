//! In-process identity providers: an OIDC issuer (discovery, authorize, token, JWKS; RS256
//! id_tokens) and GitHub OAuth (authorize, access_token, /user, /user/emails). Both enforce
//! PKCE S256, single-use codes, client credentials and the exact redirect_uri.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{Form, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use parking_lot::Mutex;
use rsa::pkcs8::DecodePrivateKey;
use rsa::signature::{SignatureEncoding, Signer};
use rsa::traits::PublicKeyParts;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::task::JoinHandle;

pub const CLIENT_ID: &str = "edge-client";
pub const CLIENT_SECRET: &str = "edge-client-secret";
const KID: &str = "idp-key-1";

struct Grant {
    client_id: String,
    redirect_uri: String,
    nonce: Option<String>,
    challenge: String,
}

#[derive(Default)]
pub struct IdpState {
    pub issuer: String,
    grants: HashMap<String, Grant>,
    /// When set, id_tokens carry this nonce instead of the one the edge asked for.
    pub nonce_override: Option<String>,
    pub sub: String,
    pub email: String,
    pub groups: Vec<String>,
    /// Every token request's form, for assertions.
    pub token_requests: Vec<HashMap<String, String>>,
    counter: u64,
}

pub struct FakeIdp {
    pub addr: SocketAddr,
    pub issuer: String,
    pub state: Arc<Mutex<IdpState>>,
    task: JoinHandle<()>,
}

impl Drop for FakeIdp {
    fn drop(&mut self) {
        self.task.abort();
    }
}

type Shared = Arc<Mutex<IdpState>>;

fn challenge_of(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn rsa_key() -> rsa::RsaPrivateKey {
    rsa::RsaPrivateKey::from_pkcs8_pem(include_str!("../fixtures/idp-test-rsa.pem")).unwrap()
}

fn sign_rs256(claims: &Value) -> String {
    let header = URL_SAFE_NO_PAD.encode(json!({ "alg": "RS256", "kid": KID, "typ": "JWT" }).to_string());
    let payload = URL_SAFE_NO_PAD.encode(claims.to_string());
    let input = format!("{header}.{payload}");
    let key = rsa::pkcs1v15::SigningKey::<Sha256>::new(rsa_key());
    let sig = key.sign(input.as_bytes()).to_vec();
    format!("{input}.{}", URL_SAFE_NO_PAD.encode(sig))
}

/// Records an authorization request and redirects back with a code, as a consenting user would.
fn authorize(state: &Shared, q: &HashMap<String, String>, oidc: bool) -> Response {
    let mut s = state.lock();
    if q.get("code_challenge_method").map(String::as_str) != Some("S256") {
        return (StatusCode::BAD_REQUEST, "pkce required").into_response();
    }
    if oidc && (q.get("response_type").map(String::as_str) != Some("code") || !q.contains_key("nonce")) {
        return (StatusCode::BAD_REQUEST, "bad oidc request").into_response();
    }
    s.counter += 1;
    let code = format!("code-{}", s.counter);
    s.grants.insert(
        code.clone(),
        Grant {
            client_id: q["client_id"].clone(),
            redirect_uri: q["redirect_uri"].clone(),
            nonce: q.get("nonce").cloned(),
            challenge: q["code_challenge"].clone(),
        },
    );
    let target = format!(
        "{}?code={code}&state={}",
        q["redirect_uri"],
        url::form_urlencoded::byte_serialize(q["state"].as_bytes()).collect::<String>()
    );
    Redirect::to(&target).into_response()
}

/// Validates a code exchange; returns the grant or the OAuth error code.
fn redeem(state: &Shared, form: &HashMap<String, String>) -> Result<Grant, &'static str> {
    let mut s = state.lock();
    s.token_requests.push(form.clone());
    let grant = s.grants.remove(form.get("code").map(String::as_str).unwrap_or_default()).ok_or("invalid_grant")?;
    if form.get("client_id") != Some(&grant.client_id) || grant.client_id != CLIENT_ID {
        return Err("invalid_client");
    }
    if form.get("client_secret").map(String::as_str) != Some(CLIENT_SECRET) {
        return Err("invalid_client");
    }
    if form.get("redirect_uri") != Some(&grant.redirect_uri) {
        return Err("invalid_grant");
    }
    if form.get("code_verifier").map(|v| challenge_of(v)) != Some(grant.challenge.clone()) {
        return Err("invalid_grant");
    }
    Ok(grant)
}

fn refused(error: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))).into_response()
}

impl FakeIdp {
    pub async fn start() -> Self {
        let (listener, addr) = super::listener().await;
        let issuer = format!("http://{addr}");
        let state: Shared = Arc::new(Mutex::new(IdpState {
            issuer: issuer.clone(),
            sub: "user-123".into(),
            email: "Ann@Acme.com".into(),
            groups: vec!["eng".into()],
            ..IdpState::default()
        }));

        async fn discovery(State(s): State<Shared>) -> Json<Value> {
            let iss = s.lock().issuer.clone();
            Json(json!({
                "issuer": iss,
                "authorization_endpoint": format!("{iss}/authorize"),
                "token_endpoint": format!("{iss}/token"),
                "jwks_uri": format!("{iss}/jwks"),
            }))
        }
        async fn authorize_h(State(s): State<Shared>, Query(q): Query<HashMap<String, String>>) -> Response {
            authorize(&s, &q, true)
        }
        async fn token(State(s): State<Shared>, Form(form): Form<HashMap<String, String>>) -> Response {
            let grant = match redeem(&s, &form) {
                Ok(g) => g,
                Err(e) => return refused(e),
            };
            let st = s.lock();
            let now = super::unix_now();
            let claims = json!({
                "iss": st.issuer,
                "sub": st.sub,
                "aud": CLIENT_ID,
                "exp": now + 300,
                "iat": now,
                "nonce": st.nonce_override.clone().or(grant.nonce),
                "email": st.email,
                "email_verified": true,
                "groups": st.groups,
            });
            Json(json!({ "access_token": "at", "token_type": "Bearer", "id_token": sign_rs256(&claims) })).into_response()
        }
        async fn jwks() -> Json<Value> {
            let key = rsa_key().to_public_key();
            Json(json!({ "keys": [{
                "kty": "RSA", "kid": KID, "alg": "RS256", "use": "sig",
                "n": URL_SAFE_NO_PAD.encode(key.n().to_bytes_be()),
                "e": URL_SAFE_NO_PAD.encode(key.e().to_bytes_be()),
            }]}))
        }

        let app = Router::new()
            .route("/.well-known/openid-configuration", get(discovery))
            .route("/authorize", get(authorize_h))
            .route("/token", post(token))
            .route("/jwks", get(jwks))
            .with_state(state.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { addr, issuer, state, task }
    }
}

pub struct FakeGithub {
    pub base: String,
    pub state: Arc<Mutex<IdpState>>,
    task: JoinHandle<()>,
}

impl Drop for FakeGithub {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeGithub {
    pub async fn start() -> Self {
        let (listener, addr) = super::listener().await;
        let base = format!("http://{addr}");
        let state: Shared = Arc::new(Mutex::new(IdpState { issuer: base.clone(), ..IdpState::default() }));

        async fn authorize_h(State(s): State<Shared>, Query(q): Query<HashMap<String, String>>) -> Response {
            authorize(&s, &q, false)
        }
        async fn access_token(State(s): State<Shared>, headers: HeaderMap, Form(form): Form<HashMap<String, String>>) -> Response {
            if headers.get("accept").and_then(|v| v.to_str().ok()) != Some("application/json") {
                return (StatusCode::NOT_ACCEPTABLE, "ask for json").into_response();
            }
            match redeem(&s, &form) {
                Ok(_) => {
                    Json(json!({ "access_token": "gho_test", "token_type": "bearer", "scope": "read:user,user:email" })).into_response()
                }
                Err(e) => refused(e),
            }
        }
        fn authed(headers: &HeaderMap) -> bool {
            headers.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer gho_test") && headers.contains_key("user-agent")
        }
        async fn user(headers: HeaderMap) -> Response {
            if !authed(&headers) {
                return StatusCode::UNAUTHORIZED.into_response();
            }
            Json(json!({ "id": 4242, "login": "octo", "email": null })).into_response()
        }
        async fn emails(headers: HeaderMap) -> Response {
            if !authed(&headers) {
                return StatusCode::UNAUTHORIZED.into_response();
            }
            Json(json!([
                { "email": "octo@users.noreply.github.com", "primary": false, "verified": true },
                { "email": "Octo@Acme.com", "primary": true, "verified": true },
                { "email": "unverified@acme.com", "primary": false, "verified": false },
            ]))
            .into_response()
        }

        let app = Router::new()
            .route("/login/oauth/authorize", get(authorize_h))
            .route("/login/oauth/access_token", post(access_token))
            .route("/user", get(user))
            .route("/user/emails", get(emails))
            .with_state(state.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { base, state, task }
    }
}
