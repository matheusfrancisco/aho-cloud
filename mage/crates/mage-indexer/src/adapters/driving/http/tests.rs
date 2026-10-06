use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::Request;
use mage_core::domain::{CommitSha, FileClass, Language, Layer, OrgId, RepoId};
use mage_core::testing::FakeClock;
use mage_core::wire::indexer::{PatternKind, TextQuery};
use mage_core::wire::token::{Jwk, JwkSet, KeyState, MemoryCutoff, TokenClaims, TokenScope, TokenSigningKey};
use time::OffsetDateTime;
use tower::ServiceExt;
use uuid::Uuid;

use super::auth::{KeySource, KeySourceError, ServiceAuth};
use super::*;
use crate::adapters::driven::search::RipgrepSearch;
use crate::app::live::LiveConfig;
use crate::ports::git::{FetchWants, GitRepoReader, MirrorRef};
use crate::testing::git::{FixtureCommit, FixtureRepo, MemGitRepo, git_blob_oid, test_fetch_spec};
use crate::testing::world::{World, WorldFile, world_org};

const ISSUER: &str = "https://mage.test";
const URL: &str = "https://github.com/acme/app.git";

/// Serves a scripted JWK set and counts fetches.
struct FakeKeys {
    set: Mutex<Result<JwkSet, KeySourceError>>,
    fetches: AtomicU32,
}

impl FakeKeys {
    fn new(keys: &[&TokenSigningKey]) -> Self {
        FakeKeys { set: Mutex::new(Ok(jwks(keys))), fetches: AtomicU32::new(0) }
    }

    fn serve(&self, set: Result<JwkSet, KeySourceError>) {
        *self.set.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = set;
    }

    fn fetches(&self) -> u32 {
        self.fetches.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl KeySource for FakeKeys {
    async fn fetch(&self) -> Result<JwkSet, KeySourceError> {
        self.fetches.fetch_add(1, Ordering::SeqCst);
        self.set.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }
}

fn jwks(keys: &[&TokenSigningKey]) -> JwkSet {
    JwkSet { keys: keys.iter().map(|k| Jwk::ed25519(k.kid(), &k.public_key(), KeyState::Active)).collect() }
}

fn signing_key(version: u32) -> TokenSigningKey {
    TokenSigningKey::from_seed(version, &[version as u8; 32])
}

struct Env {
    w: World,
    keys: Arc<FakeKeys>,
    key: TokenSigningKey,
    router: Router,
    org: OrgId,
    repo: RepoId,
    sha: CommitSha,
    _data: tempfile::TempDir,
}

impl Env {
    /// One repo whose head has `src/conn.go` and generated `gen/conn.pb.go`, Files Ready.
    async fn new() -> Env {
        let w = World::new();
        let git = Arc::new(MemGitRepo::new());
        let files = [("src/conn.go", "package conn\n// connection refused\nfunc Dial() {}\n"), ("gen/conn.pb.go", "// connection refused\n")];
        let shas = git.add_remote(URL, &FixtureRepo::linear("main", vec![FixtureCommit::new("init", FakeClock::DEFAULT_START, &files)]));
        let repo = w.repo("app").await;
        git.fetch(&MirrorRef::new(world_org(), repo.id), &test_fetch_spec(URL), &FetchWants { all_heads_and_tags: true, shas: Vec::new() })
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let key_v = w.version(repo.id, shas[0], &[Layer::Files]).await;
        let mut generated = WorldFile::source("gen/conn.pb.go", git_blob_oid(files[1].1.as_bytes()), Language::Go);
        generated.class = FileClass::Generated;
        w.manifest(&key_v, &[WorldFile::source("src/conn.go", git_blob_oid(files[0].1.as_bytes()), Language::Go), generated]).await;
        let data = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let live = Arc::new(LiveService::new(w.repos.clone(), w.knowledge.clone(), git, Arc::new(RipgrepSearch::new()), &LiveConfig::new(data.path())));
        let key = signing_key(1);
        let keys = Arc::new(FakeKeys::new(&[&key]));
        let auth = Arc::new(ServiceAuth::new(ISSUER, keys.clone(), w.clock.clone()));
        Env { router: router(live, auth), keys, key, org: world_org(), repo: repo.id, sha: shas[0], w, _data: data }
    }

    fn now(&self) -> OffsetDateTime {
        mage_core::ports::Clock::now(&*self.w.clock)
    }

    fn token_with(&self, key: &TokenSigningKey, scope: TokenScope, iat: OffsetDateTime, exp: OffsetDateTime) -> String {
        let claims = TokenClaims { scope, iat, exp, jti: Uuid::new_v4() };
        mage_core::wire::token::TokenCodec::new(ISSUER).sign(key, &claims).unwrap_or_else(|e| panic!("{e}"))
    }

    fn token(&self) -> String {
        let now = self.now();
        self.token_with(&self.key, TokenScope::Indexer { org: self.org, repo: self.repo }, now, now + time::Duration::seconds(60))
    }

    fn search_body(&self) -> String {
        let req = SearchRequest {
            org_id: self.org,
            repo_id: self.repo,
            sha: self.sha,
            query: TextQuery::new("connection refused", PatternKind::Literal),
            include_generated: false,
        };
        serde_json::to_string(&req).unwrap_or_else(|e| panic!("{e}"))
    }

    async fn call(&self, method: &str, uri: &str, token: Option<&str>, body: String) -> (StatusCode, serde_json::Value) {
        let mut req = Request::builder().method(method).uri(uri).header(header::CONTENT_TYPE, "application/json");
        if let Some(token) = token {
            req = req.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let req = req.body(Body::from(body)).unwrap_or_else(|e| panic!("{e}"));
        let resp = self.router.clone().oneshot(req).await.unwrap_or_else(|e| panic!("{e}"));
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap_or_else(|e| panic!("{e}"));
        let json = serde_json::from_slice(&bytes).unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&bytes)));
        (status, json)
    }

    async fn search(&self, token: Option<&str>) -> (StatusCode, serde_json::Value) {
        self.call("POST", routes::SEARCH, token, self.search_body()).await
    }
}

fn code(body: &serde_json::Value) -> &str {
    body["code"].as_str().unwrap_or_else(|| panic!("no error code in {body}"))
}

#[tokio::test]
async fn valid_token_searches_the_checkout() {
    let e = Env::new().await;
    let (status, body) = e.search(Some(&e.token())).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let hits: TextHits = serde_json::from_value(body).unwrap_or_else(|err| panic!("{err}"));
    let at: Vec<(&str, u32)> = hits.hits.iter().map(|h| (h.path.as_str(), h.line)).collect();
    assert_eq!(at, [("src/conn.go", 2)], "generated file dropped");
    assert_eq!(e.keys.fetches(), 1);
}

#[tokio::test]
async fn missing_malformed_expired_and_wrong_kind_tokens_are_rejected() {
    let e = Env::new().await;
    let (status, body) = e.search(None).await;
    assert_eq!((status, code(&body)), (StatusCode::UNAUTHORIZED, "unauthorized"));
    let (status, _) = e.search(Some("mgi_not.a.token")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let now = e.now();
    let scope = TokenScope::Indexer { org: e.org, repo: e.repo };
    let expired = e.token_with(&e.key, scope.clone(), now - time::Duration::seconds(120), now - time::Duration::seconds(60));
    let (status, body) = e.search(Some(&expired)).await;
    assert_eq!((status, body["message"].as_str()), (StatusCode::UNAUTHORIZED, Some("token expired")));

    let long = e.token_with(&e.key, scope, now, now + time::Duration::minutes(10));
    let (status, body) = e.search(Some(&long)).await;
    assert_eq!((status, code(&body)), (StatusCode::UNAUTHORIZED, "unauthorized"), "lifetime above 60 s");

    let knowledge = TokenScope::Knowledge {
        org: e.org,
        run: mage_core::domain::RunId::from_uuid(Uuid::from_u128(5)),
        attempt: 1,
        repo: e.repo,
        sha: e.sha,
        memory: MemoryCutoff::default(),
        disabled_layers: Vec::new(),
    };
    let wrong_kind = e.token_with(&e.key, knowledge, now, now + time::Duration::seconds(60));
    let (status, body) = e.search(Some(&wrong_kind)).await;
    assert_eq!((status, body["message"].as_str()), (StatusCode::UNAUTHORIZED, Some("wrong token kind")));

    let forged = e.token_with(&signing_key(9), TokenScope::Indexer { org: e.org, repo: e.repo }, now, now + time::Duration::seconds(60));
    let (status, _) = e.search(Some(&forged)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "key not published by the server");
}

#[tokio::test]
async fn tokens_are_single_use_and_scoped_to_the_body() {
    let e = Env::new().await;
    let token = e.token();
    assert_eq!(e.search(Some(&token)).await.0, StatusCode::OK);
    let (status, body) = e.search(Some(&token)).await;
    assert_eq!((status, body["message"].as_str()), (StatusCode::UNAUTHORIZED, Some("token replayed")));

    let now = e.now();
    let other_repo = RepoId::from_uuid(Uuid::from_u128(77));
    let foreign = e.token_with(&e.key, TokenScope::Indexer { org: e.org, repo: other_repo }, now, now + time::Duration::seconds(60));
    let (status, body) = e.search(Some(&foreign)).await;
    assert_eq!((status, code(&body)), (StatusCode::FORBIDDEN, "forbidden"));

    let (status, body) = e.call("POST", routes::SEARCH, Some(&e.token()), "{\"org_id\":".into()).await;
    assert_eq!((status, code(&body)), (StatusCode::BAD_REQUEST, "invalid"));
}

#[tokio::test]
async fn keys_refresh_on_unknown_kid_at_most_once_a_minute() {
    let e = Env::new().await;
    assert_eq!(e.search(Some(&e.token())).await.0, StatusCode::OK);
    assert_eq!(e.keys.fetches(), 1);

    // The server rotates to v2: the first v2 token triggers one refetch and is accepted.
    let v2 = signing_key(2);
    e.keys.serve(Ok(jwks(&[&e.key, &v2])));
    let now = e.now();
    let scope = TokenScope::Indexer { org: e.org, repo: e.repo };
    let with_v2 = e.token_with(&v2, scope.clone(), now, now + time::Duration::seconds(60));
    assert_eq!(e.search(Some(&with_v2)).await.0, StatusCode::OK);
    assert_eq!(e.keys.fetches(), 2);

    // Another unknown kid within the minute: rejected without a fetch.
    let v3 = signing_key(3);
    e.keys.serve(Ok(jwks(&[&e.key, &v2, &v3])));
    let with_v3 = e.token_with(&v3, scope.clone(), now, now + time::Duration::seconds(60));
    assert_eq!(e.search(Some(&with_v3)).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(e.keys.fetches(), 2);

    // A minute later the refetch is allowed again.
    e.w.clock.advance(std::time::Duration::from_secs(61));
    let now = e.now();
    let with_v3 = e.token_with(&v3, scope.clone(), now, now + time::Duration::seconds(60));
    assert_eq!(e.search(Some(&with_v3)).await.0, StatusCode::OK);
    assert_eq!(e.keys.fetches(), 3);

    // A failed periodic refresh keeps the cached keys.
    e.keys.serve(Err(KeySourceError::Unavailable("server down".into())));
    e.w.clock.advance(std::time::Duration::from_secs(5 * 60));
    let now = e.now();
    let still = e.token_with(&v2, scope, now, now + time::Duration::seconds(60));
    assert_eq!(e.search(Some(&still)).await.0, StatusCode::OK);
    assert_eq!(e.keys.fetches(), 4);
}

#[tokio::test]
async fn no_keys_yet_is_unavailable_and_health_needs_no_token() {
    let e = Env::new().await;
    e.keys.serve(Err(KeySourceError::Unavailable("server down".into())));
    let (status, body) = e.search(Some(&e.token())).await;
    assert_eq!((status, code(&body)), (StatusCode::SERVICE_UNAVAILABLE, "unavailable"));

    let (status, body) = e.call("GET", routes::HEALTH, None, String::new()).await;
    assert_eq!(status, StatusCode::OK);
    let health: HealthResponse = serde_json::from_value(body).unwrap_or_else(|err| panic!("{err}"));
    assert!(health.ok);
    assert_eq!(health.mirrors, 1);
}
