//! `mage_infra::github` against wiremock: the `SourceHost` contract over a GitHub emulator, the
//! installation-token cache, rate-limit and status mapping, ETags and request hygiene.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod github_support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use github_support::emulator::{Emulator, PREFIX};
use github_support::{Minter, bearer, client, client_at, verify_app_jwt};
use mage_core::domain::ids::{CommitSha, InstallationId};
use mage_core::ports::clock::Clock;
use mage_core::ports::source_host::{
    FetchSpec, InstallationInfo, InstallationToken, PrListQuery, PrPage, PrSort, PrState, RemoteRepo, RepoCoords, SortDirection, SourceHost,
    SourceHostError, TokenScope,
};
use mage_core::testing::{FakeClock, contract_source_host};
use mage_infra::github::{ApiResponse, Auth, GitHubClient, GitHubConfig, GitHubSourceHost};
use reqwest::Method;
use serde_json::{Value, json};
use time::OffsetDateTime;
use url::Url;
use wiremock::matchers::{any, header, method, path, path_regex, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const INST: InstallationId = InstallationId(77);

/// Keeps the emulator's server alive as long as the host under test.
struct Hosted {
    _server: MockServer,
    host: GitHubSourceHost,
}

#[async_trait]
impl SourceHost for Hosted {
    async fn installation(&self, inst: InstallationId) -> Result<InstallationInfo, SourceHostError> {
        self.host.installation(inst).await
    }
    async fn installation_repos(&self, inst: InstallationId) -> Result<Vec<RemoteRepo>, SourceHostError> {
        self.host.installation_repos(inst).await
    }
    async fn repo(&self, inst: InstallationId, repo: &RepoCoords) -> Result<RemoteRepo, SourceHostError> {
        self.host.repo(inst, repo).await
    }
    async fn resolve_ref(&self, inst: InstallationId, repo: &RepoCoords, git_ref: &str) -> Result<CommitSha, SourceHostError> {
        self.host.resolve_ref(inst, repo, git_ref).await
    }
    async fn installation_token(&self, inst: InstallationId, scope: &TokenScope) -> Result<InstallationToken, SourceHostError> {
        self.host.installation_token(inst, scope).await
    }
    async fn fetch_spec(&self, inst: InstallationId, repo: &RepoCoords) -> Result<FetchSpec, SourceHostError> {
        self.host.fetch_spec(inst, repo).await
    }
    async fn list_pull_requests(&self, inst: InstallationId, repo: &RepoCoords, query: &PrListQuery) -> Result<PrPage, SourceHostError> {
        self.host.list_pull_requests(inst, repo, query).await
    }
}

#[tokio::test]
async fn github_source_host_passes_the_contract_against_an_emulator() {
    contract_source_host(|world, clock| async move {
        let server = MockServer::start().await;
        Mock::given(any()).respond_with(Emulator::new(clock.clone(), world, &server.uri())).mount(&server).await;
        // github.com web base: the contract checks the fetch URL; the API sits under a path prefix.
        let config = GitHubConfig { api_base: Url::parse(&format!("{}{PREFIX}", server.uri())).unwrap(), ..GitHubConfig::default() };
        let client = GitHubClient::new(&config, github_support::app(), clock).unwrap();
        Arc::new(Hosted { _server: server, host: GitHubSourceHost::new(client) }) as Arc<dyn SourceHost>
    })
    .await;
}

async fn minting_server(clock: &Arc<FakeClock>, delay: Duration) -> (MockServer, Minter) {
    let server = MockServer::start().await;
    let minter = Minter { delay, ..Minter::new(clock.clone()) };
    Mock::given(method("POST")).and(path_regex(r"^/app/installations/\d+/access_tokens$")).respond_with(minter.clone()).mount(&server).await;
    (server, minter)
}

async fn requests_to(server: &MockServer, p: &str) -> Vec<wiremock::Request> {
    server.received_requests().await.unwrap().into_iter().filter(|r| r.url.path() == p).collect()
}

#[tokio::test]
async fn installation_tokens_are_cached_per_scope_until_five_minutes_remain() {
    let clock = Arc::new(FakeClock::new());
    let (server, minter) = minting_server(&clock, Duration::ZERO).await;
    let gh = client(&server.uri(), clock.clone());
    let full = TokenScope::default();

    let first = gh.installation_token(INST, &full).await.unwrap();
    assert_eq!(first.token.expose(), "ghs_1");
    assert_eq!(first.expires_at, clock.now() + Duration::from_secs(3600));
    assert_eq!(gh.installation_token(INST, &full).await.unwrap().token, first.token, "same scope: cached");
    assert_eq!(minter.minted(), 1);

    let fetch = gh.installation_token(INST, &TokenScope::fetch("web")).await.unwrap();
    assert_eq!((fetch.token.expose(), fetch.repositories.clone()), ("ghs_2", Some(vec!["web".to_owned()])), "another scope mints its own");
    let bodies: Vec<Value> = requests_to(&server, "/app/installations/77/access_tokens").await.iter().map(|r| r.body_json().unwrap()).collect();
    assert_eq!(bodies, vec![json!({}), json!({"repositories": ["web"], "permissions": {"contents": "read", "pull_requests": "read"}})]);
    assert_eq!(gh.installation_token(InstallationId(78), &full).await.unwrap().token.expose(), "ghs_3", "another installation too");

    clock.advance(Duration::from_secs(55 * 60));
    assert_eq!(gh.installation_token(INST, &full).await.unwrap().token, first.token, "exactly 5 minutes left is still fresh");
    clock.advance(Duration::from_secs(1));
    let renewed = gh.installation_token(INST, &full).await.unwrap();
    assert_eq!(renewed.token.expose(), "ghs_4", "under 5 minutes left: re-minted");
    assert_eq!(minter.minted(), 4);

    let other = gh.installation_token(InstallationId(78), &full).await.unwrap();
    assert_eq!(other.token.expose(), "ghs_5", "78's token from the start is stale too");
    gh.evict_installation(INST);
    assert_eq!(gh.installation_token(INST, &full).await.unwrap().token.expose(), "ghs_6", "eviction forces a new token");
    assert_eq!(gh.installation_token(InstallationId(78), &full).await.unwrap().token, other.token, "other installations keep theirs");
}

#[tokio::test]
async fn concurrent_token_requests_mint_once() {
    let clock = Arc::new(FakeClock::new());
    let (server, minter) = minting_server(&clock, Duration::from_millis(200)).await;
    let gh = client(&server.uri(), clock);
    let scope = TokenScope::fetch("web");
    let tokens = futures::future::join_all((0..10).map(|_| gh.installation_token(INST, &scope))).await;
    assert!(tokens.iter().all(|t| t.as_ref().map(|t| t.token.expose()) == Ok("ghs_1")), "{tokens:?}");
    assert_eq!(minter.minted(), 1);
}

#[tokio::test]
async fn a_rejected_installation_token_is_replaced_and_the_request_retried_once() {
    let clock = Arc::new(FakeClock::new());
    let (server, minter) = minting_server(&clock, Duration::ZERO).await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/web"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({"message": "Bad credentials"})))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET")).and(path("/repos/acme/web")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 1}))).mount(&server).await;
    Mock::given(method("GET")).and(path("/repos/acme/gone")).respond_with(ResponseTemplate::new(401)).mount(&server).await;
    let gh = client(&server.uri(), clock);
    let scope = TokenScope::default();
    let auth = Auth::Installation { inst: INST, scope: &scope };

    let ok: ApiResponse<Value> = gh.get(auth, "/repos/acme/web").await.unwrap();
    assert_eq!(ok.body, json!({"id": 1}));
    let seen: Vec<Option<String>> = requests_to(&server, "/repos/acme/web").await.iter().map(bearer).collect();
    assert_eq!(seen, vec![Some("ghs_1".into()), Some("ghs_2".into())], "evicted, re-minted, retried");
    assert_eq!(gh.installation_token(INST, &scope).await.unwrap().token.expose(), "ghs_2", "the replacement is cached");

    assert_eq!(gh.get::<Value>(auth, "/repos/acme/gone").await, Err(SourceHostError::Unauthorized), "a second 401 is final");
    assert_eq!(requests_to(&server, "/repos/acme/gone").await.len(), 2, "one retry only");
    assert_eq!(minter.minted(), 3);
}

#[tokio::test]
async fn installation_requests_run_at_most_four_at_once() {
    let clock = Arc::new(FakeClock::new());
    let (server, _minter) = minting_server(&clock, Duration::ZERO).await;
    Mock::given(method("GET"))
        .and(path("/slow"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!(1)).set_delay(Duration::from_millis(300)))
        .mount(&server)
        .await;
    let gh = client(&server.uri(), clock);
    let scope = TokenScope::default();
    gh.installation_token(INST, &scope).await.unwrap();
    let started = Instant::now();
    let results = futures::future::join_all((0..8).map(|_| gh.get::<u32>(Auth::Installation { inst: INST, scope: &scope }, "/slow"))).await;
    assert!(results.iter().all(Result::is_ok));
    assert!(started.elapsed() >= Duration::from_millis(600), "8 requests at 4 at a time take two rounds: {:?}", started.elapsed());
}

fn reply(status: u16) -> ResponseTemplate {
    ResponseTemplate::new(status)
}

#[tokio::test]
async fn rate_limits_carry_their_retry_time() {
    let clock = Arc::new(FakeClock::new());
    let server = MockServer::start().await;
    let reset = 1_767_229_200_i64; // 2026-01-01T01:00:00Z
    Mock::given(path("/primary"))
        .respond_with(
            reply(403)
                .insert_header("x-ratelimit-remaining", "0")
                .insert_header("x-ratelimit-reset", reset.to_string())
                .set_body_json(json!({"message": "API rate limit exceeded for installation ID 77."})),
        )
        .mount(&server)
        .await;
    Mock::given(path("/retry-after")).respond_with(reply(429).insert_header("retry-after", "30")).mount(&server).await;
    Mock::given(path("/secondary"))
        .respond_with(reply(403).set_body_json(json!({"message": "You have exceeded a secondary rate limit. Please wait a few minutes."})))
        .mount(&server)
        .await;
    Mock::given(path("/ok")).respond_with(reply(200).set_body_json(json!(true))).mount(&server).await;
    let gh = client(&server.uri(), clock.clone());
    let now = clock.now();
    let limited = |retry_at| Err(SourceHostError::RateLimited { retry_at });

    assert_eq!(gh.get::<Value>(Auth::Anonymous, "/primary").await, limited(OffsetDateTime::from_unix_timestamp(reset).unwrap()));
    assert_eq!(gh.get::<Value>(Auth::Anonymous, "/retry-after").await, limited(now + Duration::from_secs(30)));
    assert_eq!(gh.get::<Value>(Auth::Anonymous, "/secondary").await, limited(now + Duration::from_secs(60)));
    assert_eq!(gh.get::<Value>(Auth::App, "/secondary").await, limited(now + Duration::from_secs(120)), "doubled on a repeat");
    assert_eq!(gh.get::<bool>(Auth::Anonymous, "/ok").await.map(|r| r.body), Ok(true));
    assert_eq!(gh.get::<Value>(Auth::Anonymous, "/secondary").await, limited(now + Duration::from_secs(60)), "a success resets the backoff");
}

#[tokio::test]
async fn statuses_map_to_port_errors() {
    let clock = Arc::new(FakeClock::new());
    let server = MockServer::start().await;
    let mount = |p: &'static str, response: ResponseTemplate| Mock::given(path(p)).respond_with(response).mount(&server);
    mount("/401", reply(401)).await;
    mount("/403", reply(403).set_body_json(json!({"message": "Resource not accessible by integration"}))).await;
    mount("/404", reply(404).set_body_json(json!({"message": "Not Found"}))).await;
    mount("/409", reply(409).set_body_json(json!({"message": "Git Repository is empty."}))).await;
    mount(
        "/422",
        reply(422).set_body_json(json!({"message": "Validation Failed", "errors": [{"resource": "PullRequest", "code": "custom", "message": "A pull request already exists"}]})),
    )
    .await;
    mount("/500", reply(500)).await;
    mount("/502", reply(502).set_body_string("<html>bad gateway</html>")).await;
    mount("/html", reply(200).set_body_string("<html>")).await;
    mount("/slow", reply(200).set_body_json(json!(1)).set_delay(Duration::from_millis(1500))).await;
    let gh = client_at(&server.uri(), "/", clock.clone(), 300);
    let get = |p: &'static str| {
        let gh = gh.clone();
        async move { gh.get::<Value>(Auth::Anonymous, p).await.map(|r| r.body) }
    };

    assert_eq!(get("/401").await, Err(SourceHostError::Unauthorized));
    assert_eq!(get("/403").await, Err(SourceHostError::Unauthorized));
    assert_eq!(get("/404").await, Err(SourceHostError::NotFound));
    assert_eq!(get("/409").await, Err(SourceHostError::Conflict));
    assert_eq!(get("/422").await, Err(SourceHostError::Invalid("Validation Failed: A pull request already exists".into())));
    assert!(matches!(get("/500").await, Err(SourceHostError::Unavailable(_))));
    assert!(matches!(get("/502").await, Err(SourceHostError::Unavailable(m)) if m.contains("502")));
    assert!(matches!(get("/html").await, Err(SourceHostError::Unavailable(m)) if m.contains("unexpected response")));
    assert!(matches!(get("/slow").await, Err(SourceHostError::Unavailable(m)) if m.contains("timed out")), "request timeout");

    let unreachable = client_at("http://127.0.0.1:9", "/", clock, 1_000);
    assert!(matches!(unreachable.get::<Value>(Auth::Anonymous, "/x").await, Err(SourceHostError::Unavailable(_))));
}

#[tokio::test]
async fn etags_serve_unchanged_bodies_from_the_cache() {
    let clock = Arc::new(FakeClock::new());
    let (server, _minter) = minting_server(&clock, Duration::ZERO).await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/web"))
        .and(header("if-none-match", "\"v1\""))
        .respond_with(reply(304).insert_header("etag", "\"v1\""))
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/web"))
        .respond_with(reply(200).insert_header("etag", "\"v1\"").set_body_json(json!({"name": "web"})))
        .mount(&server)
        .await;
    let gh = client(&server.uri(), clock);
    let scope = TokenScope::default();
    let auth = Auth::Installation { inst: INST, scope: &scope };

    let first: ApiResponse<Value> = gh.get(auth, "/repos/acme/web").await.unwrap();
    let second: ApiResponse<Value> = gh.get(auth, "/repos/acme/web").await.unwrap();
    assert_eq!(second, first, "a 304 returns the cached response");
    assert_eq!(second.status, 200);
    gh.evict_installation(INST);
    let third: ApiResponse<Value> = gh.get(auth, "/repos/acme/web").await.unwrap();
    assert_eq!(third.body, json!({"name": "web"}));

    let conditional: Vec<Option<String>> = requests_to(&server, "/repos/acme/web")
        .await
        .iter()
        .map(|r| r.headers.get("if-none-match").map(|v| v.to_str().unwrap().to_owned()))
        .collect();
    assert_eq!(conditional, vec![None, Some("\"v1\"".into()), None], "eviction drops the installation's ETags");
}

#[tokio::test]
async fn requests_carry_github_headers_and_never_leave_github() {
    let clock = Arc::new(FakeClock::new());
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/meta"))
        .and(query_param("x", "1"))
        .and(header("accept", "application/vnd.github+json"))
        .and(header("x-github-api-version", "2022-11-28"))
        .and(header("user-agent", "ahomage"))
        .respond_with(reply(200).set_body_json(json!({"ok": true})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/web/login/oauth/access_token"))
        .and(header("accept", "application/json"))
        .and(header("content-type", "application/json"))
        .respond_with(reply(200).set_body_json(json!({"access_token": "gho_x"})))
        .mount(&server)
        .await;
    Mock::given(method("DELETE")).and(path("/thing")).respond_with(reply(204)).mount(&server).await;
    let gh = client(&server.uri(), clock.clone());

    let meta: ApiResponse<Value> = gh.get(Auth::Anonymous, "/meta?x=1").await.unwrap();
    assert_eq!(meta.body, json!({"ok": true}));
    let absolute: ApiResponse<Value> = gh.get(Auth::Anonymous, &format!("{}/meta?x=1", server.uri())).await.unwrap();
    assert_eq!(absolute.body, json!({"ok": true}), "absolute URLs under api_base work");

    let web = format!("{}login/oauth/access_token", gh.config().web_base);
    let token: ApiResponse<Value> = gh.send(Method::POST, Auth::Anonymous, &web, Some(&json!({"code": "c"}))).await.unwrap();
    assert_eq!(token.body["access_token"], "gho_x", "web_base URLs get plain JSON accept");

    let deleted: ApiResponse<()> = gh.send::<(), ()>(Method::DELETE, Auth::App, "/thing", None).await.unwrap();
    assert_eq!(deleted.status, 204, "an empty body decodes as ()");
    let app_jwt = bearer(&requests_to(&server, "/thing").await[0]).unwrap();
    assert!(verify_app_jwt(&app_jwt, clock.now()).is_ok(), "Auth::App sends the app JWT");

    let before = server.received_requests().await.unwrap().len();
    let outside = gh.send(Method::POST, Auth::Anonymous, "https://evil.example/collect", Some(&json!({"code": "c"}))).await;
    assert!(matches!(outside.map(|r: ApiResponse<Value>| r.body), Err(SourceHostError::Invalid(_))));
    let token = mage_core::ports::source_host::AccessToken::new("gho_secret");
    let outside = gh.get::<Value>(Auth::Bearer(&token), "https://evil.example/user").await;
    assert!(matches!(outside, Err(SourceHostError::Invalid(_))));
    assert_eq!(server.received_requests().await.unwrap().len(), before, "nothing was sent");
}

#[tokio::test]
async fn app_jwt_is_rs256_with_github_claims_and_reused_for_eight_minutes() {
    let clock = Arc::new(FakeClock::new());
    let gh = client("http://127.0.0.1:9", clock.clone());
    let jwt = gh.app_jwt().await.unwrap();
    assert_eq!(format!("{jwt:?}"), "***");
    let header = jsonwebtoken::decode_header(jwt.expose()).unwrap();
    assert_eq!(header.alg, jsonwebtoken::Algorithm::RS256);
    let claims = verify_app_jwt(jwt.expose(), clock.now()).unwrap();
    let now = clock.now().unix_timestamp();
    assert_eq!((claims.iat, claims.exp, claims.iss.as_str()), (now - 60, now + 540, github_support::CLIENT_ID));

    clock.advance(Duration::from_secs(7 * 60 + 59));
    assert_eq!(gh.app_jwt().await.unwrap(), jwt);
    clock.advance(Duration::from_secs(1));
    let renewed = gh.app_jwt().await.unwrap();
    assert_ne!(renewed, jwt);
    assert!(verify_app_jwt(renewed.expose(), clock.now()).is_ok());
}

#[tokio::test]
async fn bad_credentials_and_bases_are_invalid() {
    let clock: Arc<FakeClock> = Arc::new(FakeClock::new());
    let app = mage_infra::github::AppCredentials { client_id: "x".into(), private_key_pem: "-----BEGIN RSA PRIVATE KEY-----\nAAAA\n-----END RSA PRIVATE KEY-----\n".into() };
    assert!(matches!(GitHubClient::new(&GitHubConfig::default(), app, clock.clone()).err(), Some(SourceHostError::Invalid(_))));
    let config = GitHubConfig { api_base: Url::parse("ftp://api.github.com/").unwrap(), ..GitHubConfig::default() };
    assert!(matches!(GitHubClient::new(&config, github_support::app(), clock).err(), Some(SourceHostError::Invalid(_))));
}

fn pr(number: u32, updated_at: &str, merged_at: Option<&str>, body: Option<&str>) -> Value {
    json!({
        "number": number, "title": format!("PR {number}"), "body": body,
        "head": {"ref": format!("eng-{number}")}, "base": {"ref": "main"},
        "merge_commit_sha": "abababababababababababababababababababab", "merged_at": merged_at,
        "updated_at": updated_at, "html_url": format!("https://github.com/acme/web/pull/{number}"), "user": {"login": "dev"}
    })
}

#[tokio::test]
async fn pull_requests_map_fields_and_stop_at_updated_since() {
    let clock = Arc::new(FakeClock::new());
    let (server, _minter) = minting_server(&clock, Duration::ZERO).await;
    let long_body = format!("{}é tail", "x".repeat(2047));
    let next = format!("<{}/repositories/1/pulls?state=closed&page=3>; rel=\"next\"", server.uri());
    Mock::given(method("GET"))
        .and(path("/repos/acme/web/pulls"))
        .and(query_param("state", "closed"))
        .and(query_param("per_page", "2"))
        .and(query_param("page", "2"))
        .respond_with(
            reply(200)
                .insert_header("link", next.as_str())
                .set_body_json(json!([pr(9, "2026-01-01T00:00:00Z", Some("2025-12-31T23:00:00Z"), Some(&long_body)), pr(8, "2025-12-31T00:00:00Z", None, None)])),
        )
        .mount(&server)
        .await;
    let host = GitHubSourceHost::new(client(&server.uri(), clock));
    let web = RepoCoords { owner: "acme".into(), name: "web".into() };
    let since = time::macros::datetime!(2025-12-31 12:00 UTC);
    let mut query = PrListQuery {
        state: PrState::Closed,
        sort: PrSort::Updated,
        direction: SortDirection::Desc,
        updated_since: Some(since),
        page_token: Some("2".into()),
        per_page: 2,
    };

    let page = host.list_pull_requests(INST, &web, &query).await.unwrap();
    assert_eq!(page.items.iter().map(|p| p.number).collect::<Vec<_>>(), vec![9], "older than since: filtered");
    assert_eq!(page.next, None, "updated desc: an older item ends the listing");
    let merged = &page.items[0];
    assert_eq!(merged.body_head.len(), 2047, "2 KiB cut on a char boundary");
    assert_eq!(merged.merge_commit_sha, Some(CommitSha::from_bytes([0xab; 20])));
    assert_eq!(merged.merged_at, Some(time::macros::datetime!(2025-12-31 23:00 UTC)));
    assert_eq!((merged.head_ref.as_str(), merged.base_ref.as_str(), merged.author_login.as_str()), ("eng-9", "main", "dev"));

    query.sort = PrSort::Created;
    let requests_before = requests_to(&server, "/repos/acme/web/pulls").await.len();
    let created = host.list_pull_requests(INST, &web, &query).await;
    // The mock only matches sort-agnostic params; the URL must carry the new sort.
    let last = requests_to(&server, "/repos/acme/web/pulls").await.pop().unwrap();
    assert_eq!(requests_to(&server, "/repos/acme/web/pulls").await.len(), requests_before + 1);
    assert!(last.url.query().unwrap().contains("sort=created&direction=desc"));
    assert_eq!(created.unwrap().next, Some("3".into()), "the page token is the next page number");

    query.page_token = Some("zero".into());
    assert!(matches!(host.list_pull_requests(INST, &web, &query).await, Err(SourceHostError::Invalid(_))));
}
