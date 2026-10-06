//! The platform HTTP surface end to end over `tower::ServiceExt::oneshot`, with in-memory
//! fakes: login (state/PKCE one-time, return_to, invites, linking), sessions, CSRF, roles,
//! BYOK key, keys.json, 404s.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use url::Url;

use mage_core::domain::{MemberRole, Membership, Org, OrgId, UserId};
use mage_core::ports::{Clock, IdGen};
use mage_core::testing::{FakeClock, MemJobQueue, SeqIdGen};
use mage_core::wire::api::{ApiErrorBody, ApiErrorCode, ModelKeyMeta, ModelSettingsView, OrgSettings, SessionView};
use mage_core::wire::token::JwkSet;

use crate::adapters::driving::http::api_orgs::OrgsApiState;
use crate::adapters::driving::http::assets::Assets;
use crate::adapters::driving::http::auth::{AuthState, SessionGuard};
use crate::adapters::driving::http::feed_sse::{FeedState, SseTimings};
use crate::adapters::driving::http::health::HealthState;
use crate::adapters::driving::http::shell::{AreaRouters, PlatformHttp, RateLimits, origin_of, router};
use crate::app::auth::{Login, LoginConfig, SessionPolicy, Sessions};
use crate::app::gateway::{GatewayConfig, ModelPrice};
use crate::app::intake::lifecycle::{BindIntegration, ConnectConfig};
use crate::app::orgs::{Activity, CreateOrg, Directory, Members};
use crate::app::settings::{HarnessConfig, Integrations, ModelSettingsService, OrgSettingsService};
use crate::ports::audit::{Actor, AuditAction, AuditFilter, AuditLog};
use crate::ports::auth::{ExternalIdentity, IdentityProvider, IdentityProviderKind};
use crate::ports::org::{OrgStore, RetentionPolicy};
use crate::ports::vault::{Activation, SecretBytes, SecretName, SecretScope, SecretState, SecretVault, names};
use crate::testing::{
    FakeChat, FakeIdentityProvider, FakeSourceHostAdmin, FakeTokenSigner, FakeTracker, MemAuditLog, MemChangeFeed, MemOrgStore, MemRunStore,
    MemSecretVault, MemTicketStore, MemTokenStore, Scripted, ScriptedUpstream,
};

const PUBLIC_URL: &str = "https://mage.test";
const MODEL: &str = "claude-test-1";

struct World {
    clock: Arc<FakeClock>,
    audit: Arc<MemAuditLog>,
    vault: Arc<MemSecretVault>,
    github: Arc<FakeIdentityProvider>,
    google: Arc<FakeIdentityProvider>,
    upstream: Arc<ScriptedUpstream>,
    source_admin: Arc<FakeSourceHostAdmin>,
    app: Router,
    hoop: Org,
    owner: ExternalIdentity,
    admin: ExternalIdentity,
    member: ExternalIdentity,
    outsider: ExternalIdentity,
}

async fn world() -> World {
    let clock = Arc::new(FakeClock::new());
    let ids: Arc<dyn IdGen> = Arc::new(SeqIdGen::new());
    let jobs = Arc::new(MemJobQueue::new(clock.clone(), ids.clone()));
    let feed = Arc::new(MemChangeFeed::new(clock.clone()));
    let orgs = Arc::new(MemOrgStore::new(clock.clone(), jobs.clone(), feed.clone()));
    let tickets = Arc::new(MemTicketStore::new(clock.clone(), jobs.clone(), feed.clone()));
    let runs = Arc::new(MemRunStore::new(clock.clone(), jobs.clone()));
    let tokens = Arc::new(MemTokenStore::new(clock.clone()));
    let audit = Arc::new(MemAuditLog::new(clock.clone()));
    let vault = Arc::new(MemSecretVault::new(clock.clone()));
    vault
        .put(&SecretScope::Platform, &SecretName::new(names::CSRF_KEY).unwrap(), SecretBytes::new(vec![9; 32]), Activation::Now, &Actor::System)
        .await
        .unwrap();
    let github = Arc::new(FakeIdentityProvider::new(IdentityProviderKind::GitHub));
    let google = Arc::new(FakeIdentityProvider::new(IdentityProviderKind::Google));
    let upstream = Arc::new(ScriptedUpstream::new());
    let signer = Arc::new(FakeTokenSigner::new(PUBLIC_URL));
    let public_url = Url::parse(PUBLIC_URL).unwrap();

    // People and orgs.
    let owner = github.identity("1", "olivia", Some("olivia@example.com"));
    let admin = github.identity("2", "adam", Some("adam@example.com"));
    let member = github.identity("3", "mia", Some("mia@example.com"));
    let outsider = github.identity("4", "otto", Some("otto@example.com"));
    let mut users = Vec::new();
    for identity in [&owner, &admin, &member, &outsider] {
        users.push(orgs.upsert_user_by_identity(identity, ids.new_id()).await.unwrap().id);
    }
    let hoop = Org { id: ids.new_id(), slug: "hoop".into(), name: "Hoop".into(), created_at: clock.now() };
    orgs.create_org(&hoop, users[0], &RetentionPolicy::default()).await.unwrap();
    orgs.put_membership(&Membership { org_id: hoop.id, user_id: users[1], role: MemberRole::Admin }).await.unwrap();
    orgs.put_membership(&Membership { org_id: hoop.id, user_id: users[2], role: MemberRole::Member }).await.unwrap();
    let other = Org { id: ids.new_id(), slug: "other".into(), name: "Other".into(), created_at: clock.now() };
    orgs.create_org(&other, users[3], &RetentionPolicy::default()).await.unwrap();

    let sessions = Arc::new(Sessions::new(tokens.clone(), vault.clone(), clock.clone(), SessionPolicy::default()));
    let providers: Vec<Arc<dyn IdentityProvider>> = vec![github.clone(), google.clone()];
    let login = Arc::new(Login::new(
        providers,
        tokens.clone(),
        orgs.clone(),
        sessions.clone(),
        audit.clone(),
        clock.clone(),
        ids.clone(),
        LoginConfig { public_url: public_url.clone() },
    ));
    let directory = Arc::new(Directory::new(orgs.clone(), tickets, runs.clone()));
    let mut gateway = GatewayConfig::default();
    gateway.pricing.insert(MODEL.into(), ModelPrice { input: 1, output: 1, cache_write: 1, cache_read: 1 });
    let source_admin = Arc::new(FakeSourceHostAdmin::new());
    let bind = Arc::new(BindIntegration::new(
        orgs.clone(),
        source_admin.clone(),
        Arc::new(FakeTracker::new()),
        Arc::new(FakeChat::new(clock.clone())),
        vault.clone(),
        clock.clone(),
        ConnectConfig::default(),
    ));
    let orgs_api = OrgsApiState {
        sessions: sessions.clone(),
        login: login.clone(),
        directory: directory.clone(),
        create: Arc::new(CreateOrg::new(orgs.clone(), Some(github.clone()), audit.clone(), clock.clone(), ids.clone())),
        public_org_create: false,
        members: Arc::new(Members::new(orgs.clone(), github.clone(), audit.clone(), clock.clone(), ids.clone())),
        settings: Arc::new(OrgSettingsService::new(orgs.clone(), audit.clone(), HarnessConfig::default(), true)),
        model: Arc::new(ModelSettingsService::new(orgs.clone(), vault.clone(), upstream.clone(), audit.clone(), gateway)),
        integrations: Arc::new(Integrations::new(orgs.clone(), tokens.clone(), bind, audit.clone(), clock.clone(), public_url.clone())),
        activity: Arc::new(Activity::new(audit.clone(), runs)),
        features: Vec::new(),
    };
    let guard = SessionGuard { sessions: sessions.clone(), directory, public_origin: origin_of(&public_url) };
    let platform = PlatformHttp {
        public_url,
        guard: guard.clone(),
        auth: AuthState { login, sessions: sessions.clone(), guard, clock: clock.clone() },
        orgs_api,
        feed: FeedState { feed, sessions, shutdown: CancellationToken::new(), timings: SseTimings::default() },
        health: HealthState::new(Vec::new()),
        signer,
        ui: Some(Assets::from_map([("index.html".to_owned(), bytes::Bytes::from_static(b"<html></html>"))]).unwrap()),
        trace: None,
        limits: RateLimits { auth_per_min: 10_000.0, ..RateLimits::default() },
    };
    let app = router(platform, AreaRouters::default());
    World { clock, audit, vault, github, google, upstream, source_admin, app, hoop, owner, admin, member, outsider }
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Reply {
    fn location(&self) -> &str {
        self.headers.get(header::LOCATION).and_then(|v| v.to_str().ok()).unwrap_or("")
    }

    /// Value set for cookie `name` (`None` if not set by this response).
    fn set_cookie(&self, name: &str) -> Option<String> {
        self.headers
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .filter_map(|v| v.split(';').next())
            .filter_map(|kv| kv.split_once('='))
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.to_owned())
    }

    fn json<T: serde::de::DeserializeOwned>(&self) -> T {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&self.body)))
    }

    fn error_code(&self) -> ApiErrorCode {
        self.json::<ApiErrorBody>().error.code
    }
}

#[derive(Default, Clone)]
struct Req<'a> {
    cookies: Vec<(&'a str, String)>,
    csrf: Option<String>,
    origin: Option<&'a str>,
    fetch_site: Option<&'a str>,
    content_type: Option<&'a str>,
    body: Option<String>,
}

async fn send(w: &World, method: Method, path: &str, r: Req<'_>) -> Reply {
    let mut b = Request::builder().method(method).uri(path).header(header::HOST, "mage.test");
    if !r.cookies.is_empty() {
        let cookie = r.cookies.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("; ");
        b = b.header(header::COOKIE, cookie);
    }
    if let Some(csrf) = r.csrf {
        b = b.header("x-csrf-token", csrf);
    }
    if let Some(origin) = r.origin {
        b = b.header(header::ORIGIN, origin);
    }
    if let Some(site) = r.fetch_site {
        b = b.header("sec-fetch-site", site);
    }
    let body = match r.body {
        Some(body) => {
            b = b.header(header::CONTENT_TYPE, r.content_type.unwrap_or("application/json")).header(header::CONTENT_LENGTH, body.len());
            Body::from(body)
        }
        None => Body::empty(),
    };
    let response = w.app.clone().oneshot(b.body(body).unwrap()).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap().to_vec();
    Reply { status, headers, body }
}

/// A logged-in browser.
#[derive(Clone)]
struct Client {
    sid: String,
    csrf: String,
}

impl Client {
    fn get(&self) -> Req<'static> {
        Req { cookies: vec![("__Host-mage_sid", self.sid.clone())], ..Req::default() }
    }

    fn mutate(&self, body: Option<serde_json::Value>) -> Req<'static> {
        Req {
            cookies: vec![("__Host-mage_sid", self.sid.clone())],
            csrf: Some(self.csrf.clone()),
            origin: Some(PUBLIC_URL),
            body: body.map(|b| b.to_string()),
            ..Req::default()
        }
    }
}

/// Starts a login (or link) and returns `(state cookie, authorize location)`.
async fn start(w: &World, path: &str, req: Req<'_>, method: Method) -> (String, Reply) {
    let reply = send(w, method, path, req).await;
    let state = reply.set_cookie("__Host-mage_oauth").expect("oauth cookie");
    (state, reply)
}

async fn callback(w: &World, provider: &str, code: &str, state: &str, cookie_state: &str, sid: Option<&str>) -> Reply {
    let mut cookies = vec![("__Host-mage_oauth", cookie_state.to_owned())];
    if let Some(sid) = sid {
        cookies.push(("__Host-mage_sid", sid.to_owned()));
    }
    send(w, Method::GET, &format!("/auth/{provider}/callback?code={code}&state={state}"), Req { cookies, ..Req::default() }).await
}

async fn login_as(w: &World, idp: &FakeIdentityProvider, identity: &ExternalIdentity) -> Client {
    let provider = identity.provider.as_str();
    let code = format!("code-{}-{}", identity.subject, w.clock.now().unix_timestamp_nanos());
    idp.on_code(&code, Ok(identity.clone()));
    let (state, _) = start(w, &format!("/auth/{provider}/login"), Req::default(), Method::GET).await;
    let reply = callback(w, provider, &code, &state, &state, None).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.location());
    let sid = reply.set_cookie("__Host-mage_sid").expect("session cookie");
    let session = send(w, Method::GET, "/api/v1/session", Req { cookies: vec![("__Host-mage_sid", sid.clone())], ..Req::default() }).await;
    assert_eq!(session.status, StatusCode::OK);
    Client { sid, csrf: session.json::<SessionView>().csrf_token }
}

fn s256(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

async fn actions(w: &World, org: Option<OrgId>) -> Vec<AuditAction> {
    match org {
        Some(org) => w.audit.list(org, AuditFilter::default(), 200).await.unwrap().into_iter().map(|r| r.event.action).collect(),
        None => w.audit.records().into_iter().map(|r| r.event.action).collect(),
    }
}

// ---------------------------------------------------------------------------------------------
// Login
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn login_uses_pkce_and_a_one_time_state() {
    let w = world().await;
    w.github.on_code("c1", Ok(w.member.clone()));
    let (state, started) = start(&w, "/auth/github/login?return_to=/o/hoop/tickets", Req::default(), Method::GET).await;
    assert_eq!(started.status, StatusCode::SEE_OTHER);
    assert!(started.location().starts_with("https://idp.test/github/authorize?"));
    let authorize = w.github.authorizations().pop().expect("authorize");
    assert_eq!(authorize.state, state);
    assert_eq!(authorize.redirect_uri, "https://mage.test/auth/github/callback");

    let done = callback(&w, "github", "c1", &state, &state, None).await;
    assert_eq!(done.location(), "/o/hoop/tickets");
    assert!(done.set_cookie("__Host-mage_sid").is_some_and(|v| v.len() == 43));
    assert_eq!(done.set_cookie("__Host-mage_oauth").as_deref(), Some(""), "pre-login cookie cleared");
    let exchange = w.github.exchanges().pop().expect("exchange");
    assert_eq!(s256(&exchange.pkce_verifier), authorize.pkce_challenge, "S256(verifier) = challenge");
    assert_eq!(exchange.redirect_uri, authorize.redirect_uri);
    assert!(actions(&w, None).await.contains(&AuditAction::AuthLogin));

    // Replaying the same callback: the state was consumed.
    w.github.on_code("c1", Ok(w.member.clone()));
    let replay = callback(&w, "github", "c1", &state, &state, None).await;
    assert_eq!(replay.location(), "/login?error=state_expired");
    assert!(replay.set_cookie("__Host-mage_sid").is_none());
}

#[tokio::test]
async fn login_state_must_match_the_pre_login_cookie_and_expires() {
    let w = world().await;
    w.github.on_code("c2", Ok(w.member.clone()));
    let (state, _) = start(&w, "/auth/github/login", Req::default(), Method::GET).await;
    let (other_state, _) = start(&w, "/auth/github/login", Req::default(), Method::GET).await;
    let mismatch = callback(&w, "github", "c2", &state, &other_state, None).await;
    assert_eq!(mismatch.location(), "/login?error=state_mismatch");
    assert!(w.github.exchanges().is_empty(), "no exchange before the state checks pass");

    w.clock.advance(Duration::from_secs(11 * 60));
    let expired = callback(&w, "github", "c2", &other_state, &other_state, None).await;
    assert_eq!(expired.location(), "/login?error=state_expired");
    assert!(actions(&w, None).await.contains(&AuditAction::AuthLoginFailed));
}

#[tokio::test]
async fn provider_denial_and_unknown_provider() {
    let w = world().await;
    let (state, _) = start(&w, "/auth/github/login", Req::default(), Method::GET).await;
    let denied = callback(&w, "github", "never-scripted", &state, &state, None).await;
    assert_eq!(denied.location(), "/login?error=denied");
    let unknown = send(&w, Method::GET, "/auth/gitlab/login", Req::default()).await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn return_to_only_accepts_relative_paths() {
    let w = world().await;
    for (raw, expected) in [
        ("//evil.example", "/"),
        ("/%5Cevil.example", "/"),
        ("https://evil.example/x", "/"),
        ("/%09/evil.example", "/"),
        ("evil", "/"),
        ("/o/hoop/runs?x=1", "/o/hoop/runs?x=1"),
    ] {
        let code = format!("rt-{raw}");
        w.github.on_code(&code, Ok(w.member.clone()));
        let (state, _) = start(&w, &format!("/auth/github/login?return_to={}", raw.replace('?', "%3F").replace('=', "%3D")), Req::default(), Method::GET).await;
        let done = callback(&w, "github", &url::form_urlencoded::byte_serialize(code.as_bytes()).collect::<String>(), &state, &state, None).await;
        assert_eq!(done.location(), expected, "return_to={raw}");
    }
}

#[tokio::test]
async fn pending_invites_become_memberships_on_first_login() {
    let w = world().await;
    let newbie = w.github.identity("77", "newbie", Some("newbie@example.com"));
    w.github.add_login(newbie.clone());
    let admin = login_as(&w, &w.github, &w.admin).await;

    // Admins cannot invite Owners; unknown logins are 422.
    let too_high = send(&w, Method::POST, "/api/v1/orgs/hoop/members/invites", admin.mutate(Some(serde_json::json!({"github_login": "newbie", "role": "owner"})))).await;
    assert_eq!(too_high.status, StatusCode::FORBIDDEN);
    let unknown = send(&w, Method::POST, "/api/v1/orgs/hoop/members/invites", admin.mutate(Some(serde_json::json!({"github_login": "ghost", "role": "member"})))).await;
    assert_eq!(unknown.status, StatusCode::UNPROCESSABLE_ENTITY);
    let invited = send(&w, Method::POST, "/api/v1/orgs/hoop/members/invites", admin.mutate(Some(serde_json::json!({"github_login": "NewBie", "role": "admin"})))).await;
    assert_eq!(invited.status, StatusCode::CREATED, "{}", String::from_utf8_lossy(&invited.body));

    let newbie_client = login_as(&w, &w.github, &newbie).await;
    let session: SessionView = send(&w, Method::GET, "/api/v1/session", newbie_client.get()).await.json();
    assert_eq!(session.memberships.len(), 1);
    assert_eq!((session.memberships[0].org.as_str(), session.memberships[0].role), ("hoop", MemberRole::Admin));
    let hoop_actions = actions(&w, Some(w.hoop.id)).await;
    assert!(hoop_actions.contains(&AuditAction::MemberInvited) && hoop_actions.contains(&AuditAction::MemberJoined));
    let members = send(&w, Method::GET, "/api/v1/orgs/hoop/members", newbie_client.get()).await;
    assert_eq!(members.status, StatusCode::OK);
}

#[tokio::test]
async fn a_second_provider_is_linked_only_from_a_session() {
    let w = world().await;
    let owner = login_as(&w, &w.github, &w.owner).await;
    let owner_id = send(&w, Method::GET, "/api/v1/session", owner.get()).await.json::<SessionView>().user.id;

    // Same email as the GitHub account, never auto-linked: a Google login is refused.
    let google_owner = w.google.identity("g-1", "olivia@example.com", Some("olivia@example.com"));
    w.google.on_code("g-auto", Ok(google_owner.clone()));
    let (state, _) = start(&w, "/auth/google/login", Req::default(), Method::GET).await;
    assert_eq!(callback(&w, "google", "g-auto", &state, &state, None).await.location(), "/login?error=identity_conflict");

    // Link from the settings page (session + CSRF).
    let link = send(&w, Method::POST, "/auth/google/link?return_to=/o/hoop/settings", owner.mutate(None)).await;
    assert_eq!(link.status, StatusCode::OK, "{}", String::from_utf8_lossy(&link.body));
    let state = link.set_cookie("__Host-mage_oauth").expect("oauth cookie");
    w.google.on_code("g-link", Ok(google_owner.clone()));
    // The callback without the session that started the link is refused.
    let no_session = callback(&w, "google", "g-link", &state, &state, None).await;
    assert_eq!(no_session.location(), "/login?error=link_session_mismatch");

    let link = send(&w, Method::POST, "/auth/google/link?return_to=/o/hoop/settings", owner.mutate(None)).await;
    let state = link.set_cookie("__Host-mage_oauth").expect("oauth cookie");
    w.google.on_code("g-link-2", Ok(google_owner.clone()));
    let linked = callback(&w, "google", "g-link-2", &state, &state, Some(&owner.sid)).await;
    assert_eq!(linked.location(), "/o/hoop/settings");
    assert!(linked.set_cookie("__Host-mage_sid").is_none(), "a link keeps the current session");

    let via_google = login_as(&w, &w.google, &google_owner).await;
    let session: SessionView = send(&w, Method::GET, "/api/v1/session", via_google.get()).await.json();
    assert_eq!(session.user.id, owner_id);

    // Starting a link needs a session.
    let anonymous = send(&w, Method::POST, "/auth/google/link", Req { origin: Some(PUBLIC_URL), ..Req::default() }).await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------------------------
// Sessions and CSRF
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn csrf_token_origin_and_json_are_required_on_mutations() {
    let w = world().await;
    let admin = login_as(&w, &w.github, &w.admin).await;
    let settings = serde_json::to_value(OrgSettings { general: Default::default(), triage: Default::default(), fix: Default::default() }).unwrap();
    let path = "/api/v1/orgs/hoop/settings";

    let ok = send(&w, Method::PUT, path, admin.mutate(Some(settings.clone()))).await;
    assert_eq!(ok.status, StatusCode::OK, "{}", String::from_utf8_lossy(&ok.body));

    let mut missing = admin.mutate(Some(settings.clone()));
    missing.csrf = None;
    assert_eq!(send(&w, Method::PUT, path, missing).await.status, StatusCode::FORBIDDEN);

    let mut wrong = admin.mutate(Some(settings.clone()));
    wrong.csrf = Some(URL_SAFE_NO_PAD.encode([0u8; 32]));
    assert_eq!(send(&w, Method::PUT, path, wrong).await.status, StatusCode::FORBIDDEN);

    let other = login_as(&w, &w.github, &w.member).await;
    let mut foreign = admin.mutate(Some(settings.clone()));
    foreign.csrf = Some(other.csrf.clone());
    assert_eq!(send(&w, Method::PUT, path, foreign).await.status, StatusCode::FORBIDDEN, "another session's token");

    let mut cross = admin.mutate(Some(settings.clone()));
    cross.origin = Some("https://evil.example");
    let reply = send(&w, Method::PUT, path, cross).await;
    assert_eq!((reply.status, reply.error_code()), (StatusCode::FORBIDDEN, ApiErrorCode::Forbidden));

    let mut no_origin = admin.mutate(Some(settings.clone()));
    no_origin.origin = None;
    assert_eq!(send(&w, Method::PUT, path, no_origin.clone()).await.status, StatusCode::FORBIDDEN);
    no_origin.fetch_site = Some("cross-site");
    assert_eq!(send(&w, Method::PUT, path, no_origin.clone()).await.status, StatusCode::FORBIDDEN);
    no_origin.fetch_site = Some("same-origin");
    assert_eq!(send(&w, Method::PUT, path, no_origin).await.status, StatusCode::OK);

    let mut form = admin.mutate(Some(settings));
    form.content_type = Some("text/plain");
    assert_eq!(send(&w, Method::PUT, path, form).await.status, StatusCode::UNPROCESSABLE_ENTITY);

    // GETs never need the token.
    assert_eq!(send(&w, Method::GET, path, admin.get()).await.status, StatusCode::OK);
}

#[tokio::test]
async fn sessions_expire_when_idle_and_logout_revokes() {
    let w = world().await;
    let member = login_as(&w, &w.github, &w.member).await;
    assert_eq!(send(&w, Method::GET, "/api/v1/session", member.get()).await.status, StatusCode::OK);
    w.clock.advance(Duration::from_secs(12 * 3600 + 1));
    let expired = send(&w, Method::GET, "/api/v1/session", member.get()).await;
    assert_eq!((expired.status, expired.error_code()), (StatusCode::UNAUTHORIZED, ApiErrorCode::Unauthenticated));
    assert_eq!(expired.set_cookie("__Host-mage_sid").as_deref(), Some(""), "stale cookie cleared");
    assert!(expired.json::<ApiErrorBody>().error.request_id.starts_with("req_"));

    let member = login_as(&w, &w.github, &w.member).await;
    let second = login_as(&w, &w.github, &w.member).await;
    assert_ne!(member.sid, second.sid, "every login rotates the session id");
    let out = send(&w, Method::POST, "/auth/logout", member.mutate(None)).await;
    assert_eq!(out.status, StatusCode::NO_CONTENT);
    assert_eq!(send(&w, Method::GET, "/api/v1/session", member.get()).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(send(&w, Method::GET, "/api/v1/session", second.get()).await.status, StatusCode::OK);
    let all = send(&w, Method::POST, "/auth/logout?all=true", second.mutate(None)).await;
    assert_eq!(all.status, StatusCode::NO_CONTENT);
    assert_eq!(send(&w, Method::GET, "/api/v1/session", second.get()).await.status, StatusCode::UNAUTHORIZED);
    assert!(actions(&w, None).await.contains(&AuditAction::AuthSessionsRevoked));
}

// ---------------------------------------------------------------------------------------------
// Roles and tenancy
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn org_routes_enforce_minimum_roles_and_hide_other_orgs() {
    let w = world().await;
    let owner = login_as(&w, &w.github, &w.owner).await;
    let admin = login_as(&w, &w.github, &w.admin).await;
    let member = login_as(&w, &w.github, &w.member).await;
    let outsider = login_as(&w, &w.github, &w.outsider).await;

    assert_eq!(send(&w, Method::GET, "/api/v1/orgs/hoop/members", Req::default()).await.status, StatusCode::UNAUTHORIZED);
    for client in [&outsider] {
        let hidden = send(&w, Method::GET, "/api/v1/orgs/hoop/members", client.get()).await;
        assert_eq!((hidden.status, hidden.error_code()), (StatusCode::NOT_FOUND, ApiErrorCode::NotFound));
    }
    assert_eq!(send(&w, Method::GET, "/api/v1/orgs/nope/members", owner.get()).await.status, StatusCode::NOT_FOUND);

    let cases: [(&Client, Method, &str, StatusCode); 9] = [
        (&member, Method::GET, "/api/v1/orgs/hoop/settings", StatusCode::OK),
        (&member, Method::GET, "/api/v1/orgs/hoop/audit", StatusCode::FORBIDDEN),
        (&member, Method::GET, "/api/v1/orgs/hoop/integrations", StatusCode::FORBIDDEN),
        (&admin, Method::GET, "/api/v1/orgs/hoop/audit", StatusCode::OK),
        (&admin, Method::GET, "/api/v1/orgs/hoop/settings/model", StatusCode::OK),
        (&admin, Method::GET, "/api/v1/orgs/hoop/integrations", StatusCode::OK),
        (&member, Method::GET, "/api/v1/orgs/hoop/settings/model", StatusCode::FORBIDDEN),
        (&owner, Method::GET, "/api/v1/orgs/hoop/settings/model", StatusCode::OK),
        (&member, Method::GET, "/api/v1/orgs/hoop/usage?from=2026-01-01T00:00:00Z&to=2026-02-01T00:00:00Z&group_by=model", StatusCode::FORBIDDEN),
    ];
    for (client, method, path, expected) in cases {
        assert_eq!(send(&w, method.clone(), path, client.get()).await.status, expected, "{method} {path}");
    }
    let update = serde_json::json!({"route": "anthropic", "allowed_models": [MODEL]});
    assert_eq!(send(&w, Method::PUT, "/api/v1/orgs/hoop/settings/model", admin.mutate(Some(update.clone()))).await.status, StatusCode::FORBIDDEN);
    assert_eq!(send(&w, Method::PUT, "/api/v1/orgs/hoop/settings/model", owner.mutate(Some(update))).await.status, StatusCode::OK);
    let unpriced = serde_json::json!({"route": "anthropic", "allowed_models": ["claude-unpriced"]});
    assert_eq!(send(&w, Method::PUT, "/api/v1/orgs/hoop/settings/model", owner.mutate(Some(unpriced))).await.status, StatusCode::UNPROCESSABLE_ENTITY);
    let vertex = serde_json::json!({"route": "vertex", "allowed_models": []});
    assert_eq!(send(&w, Method::PUT, "/api/v1/orgs/hoop/settings/model", owner.mutate(Some(vertex))).await.status, StatusCode::UNPROCESSABLE_ENTITY);

    // An Admin removes Members but not the Owner; the last Owner cannot be demoted.
    let session: SessionView = send(&w, Method::GET, "/api/v1/session", owner.get()).await.json();
    let owner_id: UserId = session.user.id;
    let member_id = send(&w, Method::GET, "/api/v1/session", member.get()).await.json::<SessionView>().user.id;
    assert_eq!(send(&w, Method::DELETE, &format!("/api/v1/orgs/hoop/members/{owner_id}"), admin.mutate(None)).await.status, StatusCode::FORBIDDEN);
    let demote = send(&w, Method::PUT, &format!("/api/v1/orgs/hoop/members/{owner_id}"), owner.mutate(Some(serde_json::json!({"role": "admin"})))).await;
    assert_eq!(demote.status, StatusCode::CONFLICT);
    assert_eq!(send(&w, Method::DELETE, &format!("/api/v1/orgs/hoop/members/{member_id}"), admin.mutate(None)).await.status, StatusCode::NO_CONTENT);
    assert_eq!(send(&w, Method::GET, "/api/v1/orgs/hoop/settings", member.get()).await.status, StatusCode::NOT_FOUND, "removed member loses access");

    // ClaudeCode is refused in production until the gateway path is verified.
    let mut settings = OrgSettings { general: Default::default(), triage: Default::default(), fix: Default::default() };
    settings.general.harness = mage_core::domain::HarnessKind::ClaudeCode;
    let refused = send(&w, Method::PUT, "/api/v1/orgs/hoop/settings", admin.mutate(Some(serde_json::to_value(settings).unwrap()))).await;
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);

    // v1: orgs are created by the CLI only.
    let create = send(&w, Method::POST, "/api/v1/orgs", owner.mutate(Some(serde_json::json!({"slug": "new-org", "name": "New"})))).await;
    assert_eq!(create.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn byok_key_is_validated_upstream_write_only_and_replaces_old_versions() {
    let w = world().await;
    let owner = login_as(&w, &w.github, &w.owner).await;
    let path = "/api/v1/orgs/hoop/settings/model/key";
    let key = "sk-ant-api03-0123456789abcdefghijklmnop-wxyz";

    let malformed = send(&w, Method::PUT, path, owner.mutate(Some(serde_json::json!({"api_key": "not-a-key"})))).await;
    assert_eq!(malformed.status, StatusCode::UNPROCESSABLE_ENTITY);

    w.upstream.push(Scripted::json(401, r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#));
    let rejected = send(&w, Method::PUT, path, owner.mutate(Some(serde_json::json!({"api_key": key})))).await;
    assert_eq!(rejected.status, StatusCode::UNPROCESSABLE_ENTITY);
    let name = SecretName::new(names::MODEL_ANTHROPIC_API_KEY).unwrap();
    assert!(w.vault.versions(&SecretScope::Org(w.hoop.id), &name).await.unwrap().is_empty(), "rejected key not stored");
    assert!(actions(&w, Some(w.hoop.id)).await.contains(&AuditAction::ByokKeyRejected));

    for _ in 0..2 {
        w.upstream.push(Scripted::json(200, r#"{"input_tokens":3}"#));
        let set = send(&w, Method::PUT, path, owner.mutate(Some(serde_json::json!({"api_key": key})))).await;
        assert_eq!(set.status, StatusCode::OK, "{}", String::from_utf8_lossy(&set.body));
        let meta: ModelKeyMeta = set.json();
        assert_eq!(meta.fingerprint, "sk-ant-…wxyz");
        assert!(!String::from_utf8_lossy(&set.body).contains("0123456789"), "the key never comes back");
    }
    let states: Vec<SecretState> = w.vault.versions(&SecretScope::Org(w.hoop.id), &name).await.unwrap().into_iter().map(|m| m.state).collect();
    assert_eq!(states.iter().filter(|s| **s == SecretState::Active).count(), 1);
    assert_eq!(states.iter().filter(|s| **s == SecretState::Destroyed).count(), 1, "previous version destroyed: {states:?}");
    let view: ModelSettingsView = send(&w, Method::GET, "/api/v1/orgs/hoop/settings/model", owner.get()).await.json();
    assert_eq!(view.key.map(|k| k.fingerprint).as_deref(), Some("sk-ant-…wxyz"));

    assert_eq!(send(&w, Method::DELETE, path, owner.mutate(None)).await.status, StatusCode::NO_CONTENT);
    let view: ModelSettingsView = send(&w, Method::GET, "/api/v1/orgs/hoop/settings/model", owner.get()).await.json();
    assert!(view.key.is_none());
    assert!(actions(&w, Some(w.hoop.id)).await.contains(&AuditAction::ByokKeyDeleted));
}

// ---------------------------------------------------------------------------------------------
// Public routes
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn keys_json_probes_shell_and_unknown_paths() {
    let w = world().await;
    let keys = send(&w, Method::GET, "/.well-known/mage/keys.json", Req::default()).await;
    assert_eq!(keys.status, StatusCode::OK);
    let set: JwkSet = keys.json();
    assert_eq!(set.keys.len(), 1);
    assert_eq!(set.keys[0].crv, "Ed25519");

    assert_eq!(send(&w, Method::GET, "/health", Req::default()).await.status, StatusCode::OK);
    assert_eq!(send(&w, Method::GET, "/ready", Req::default()).await.status, StatusCode::OK);
    let shell = send(&w, Method::GET, "/o/hoop/tickets", Req::default()).await;
    assert_eq!(shell.status, StatusCode::OK);
    let csp = shell.headers.get(header::CONTENT_SECURITY_POLICY).and_then(|v| v.to_str().ok()).unwrap_or("");
    assert!(csp.contains("frame-ancestors 'none'") && csp.contains("default-src 'none'"));
    assert_eq!(shell.headers.get(header::REFERRER_POLICY).map(|v| v.as_bytes()), Some(&b"no-referrer"[..]));
    assert!(shell.headers.contains_key(header::STRICT_TRANSPORT_SECURITY));

    let api_404 = send(&w, Method::GET, "/api/v1/nope", Req::default()).await;
    assert_eq!((api_404.status, api_404.error_code()), (StatusCode::NOT_FOUND, ApiErrorCode::NotFound));
    assert!(api_404.headers.contains_key("x-request-id"));
    assert_eq!(send(&w, Method::GET, "/hooks/unknown", Req::default()).await.status, StatusCode::NOT_FOUND);
    assert_eq!(send(&w, Method::GET, "/random", Req::default()).await.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn integration_installs_check_role_state_owner_and_installation() {
    use mage_core::domain::InstallationId;
    use mage_core::ports::source_host::{AccountKind, InstallationInfo, RepoSelection};
    use mage_core::wire::api::{ConnectIntegrationResponse, IntegrationList};

    use crate::ports::source_host_admin::GithubApp;

    let w = world().await;
    let owner = login_as(&w, &w.github, &w.owner).await;
    let admin = login_as(&w, &w.github, &w.admin).await;
    let member = login_as(&w, &w.github, &w.member).await;
    let connect = |client: &Client, kind: &str| {
        let req = client.mutate(None);
        let path = format!("/api/v1/orgs/hoop/integrations/{kind}/connect");
        let w = &w;
        async move { send(w, Method::POST, &path, req).await }
    };
    let state_of = |reply: &Reply| -> String {
        let url = Url::parse(&reply.json::<ConnectIntegrationResponse>().url).expect("provider url");
        url.query_pairs().find(|(k, _)| k == "state").map(|(_, v)| v.into_owned()).expect("state in url")
    };
    let finish = |client: &Client, state: &str, code: &str| {
        let req = client.get();
        let path = format!("/integrations/github/callback?installation_id=42&setup_action=install&code={code}&state={state}");
        let w = &w;
        async move { send(w, Method::GET, &path, req).await }
    };

    assert_eq!(connect(&member, "github").await.status, StatusCode::FORBIDDEN);
    assert_eq!(connect(&admin, "github-fix").await.status, StatusCode::FORBIDDEN, "the fix app needs Owner");

    // The state is bound to the user who started the install.
    let started = connect(&admin, "github").await;
    assert_eq!(started.status, StatusCode::OK, "{}", String::from_utf8_lossy(&started.body));
    let stolen = finish(&owner, &state_of(&started), "c-ok").await;
    assert_eq!(stolen.location(), "/o/hoop/settings/integrations?error=forbidden");

    // A spoofed installation id (not visible to the GitHub user) is rejected and audited.
    let started = connect(&admin, "github").await;
    let spoofed = finish(&admin, &state_of(&started), "c-ok").await;
    assert_eq!(spoofed.location(), "/o/hoop/settings/integrations?error=rejected");
    assert!(actions(&w, Some(w.hoop.id)).await.contains(&AuditAction::IntegrationInstallRejected));

    w.source_admin.grant(
        GithubApp::Read,
        "c-ok",
        InstallationInfo {
            id: InstallationId(42),
            account_login: "hoophq".into(),
            account_kind: AccountKind::Organization,
            permissions: Default::default(),
            repository_selection: RepoSelection::All,
            suspended: false,
        },
    );
    let started = connect(&admin, "github").await;
    let state = state_of(&started);
    let done = finish(&admin, &state, "c-ok").await;
    assert_eq!(done.location(), "/o/hoop/settings/integrations");
    let list: IntegrationList = send(&w, Method::GET, "/api/v1/orgs/hoop/integrations", admin.get()).await.json();
    assert_eq!(list.integrations.iter().map(|i| i.external_id.as_str()).collect::<Vec<_>>(), ["42"]);
    assert!(actions(&w, Some(w.hoop.id)).await.contains(&AuditAction::IntegrationConnected));

    // One-time state.
    assert_eq!(finish(&admin, &state, "c-ok").await.location(), "/?integration_error=state_expired");

    let revoked = send(&w, Method::DELETE, "/api/v1/orgs/hoop/integrations/github/42", admin.mutate(None)).await;
    assert_eq!(revoked.status, StatusCode::NO_CONTENT, "{}", String::from_utf8_lossy(&revoked.body));
    assert!(actions(&w, Some(w.hoop.id)).await.contains(&AuditAction::IntegrationRevoked));
}
