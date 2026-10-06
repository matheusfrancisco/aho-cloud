//! `GitHubPullRequests` against wiremock: the generic `PullRequests` contract over a REST emulator
//! of [`GitHubWorld`] (same model as the fake), plus targeted checks of the Git Data request
//! bodies, error mapping, ref refusal and `upload_media`.

use std::sync::Arc;

use base64::Engine;
use mage_core::domain::{Integration, IntegrationKind, IntegrationStatus, OrgId, Repo};
use mage_core::testing::{FakeClock, MemJobQueue, SeqIdGen};
use mage_infra::github::{AppCredentials, GitHubClient, GitHubConfig};
use serde_json::{Value, json};
use time::macros::datetime;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use super::*;
use crate::ports::pull_requests::{MediaKind, MediaUpload};
use crate::testing::fix::{gated_plan, test_repo};
use crate::testing::pull_requests::{GitHubWorld, PullRequestsFixture, contract_pull_requests};
use crate::testing::{MemChangeFeed, MemOrgStore};

const APP_KEY: &str = include_str!("../../../../../../mage-infra/tests/fixtures/github_app_key.pem");
const INSTALLATION: u64 = 7;

/// GitHub REST over a `GitHubWorld` for `acme/shop` (installation 7).
struct Emulator {
    world: GitHubWorld,
}

fn json_response(status: u16, body: Value) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(body)
}

fn not_found() -> ResponseTemplate {
    json_response(404, json!({"message": "Not Found"}))
}

fn forbidden() -> ResponseTemplate {
    json_response(403, json!({"message": "Resource not accessible by integration"}))
}

impl Emulator {
    fn pull_json(&self, p: &crate::testing::pull_requests::WorldPull) -> Value {
        let head = self.world.branch(&p.head).map(|s| s.to_string()).unwrap_or_default();
        json!({
            "number": p.number, "html_url": self.world.pull_url(p.number), "state": if p.open { "open" } else { "closed" },
            "merged": p.merged, "draft": p.draft, "mergeable": null, "head": {"sha": head, "ref": p.head}
        })
    }

    fn repo_call(&self, req: &Request, rest: &str) -> ResponseTemplate {
        let w = &self.world;
        let access = w.access();
        let verb = req.method.as_str();
        let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
        let parts: Vec<&str> = rest.split('/').collect();
        match (verb, parts.as_slice()) {
            ("GET", ["git", "ref", "heads", branch @ ..]) => match w.branch(&branch.join("/")) {
                Some(sha) => json_response(200, json!({"ref": format!("refs/heads/{}", branch.join("/")), "object": {"sha": sha.to_string(), "type": "commit"}})),
                None => not_found(),
            },
            ("GET", ["git", "commits", sha]) => match sha.parse().ok().and_then(|s| w.commit(s)) {
                Some(c) => json_response(200, json!({"sha": sha, "tree": {"sha": c.tree}, "message": c.message,
                    "parents": c.parents.iter().map(|p| json!({"sha": p.to_string()})).collect::<Vec<_>>()})),
                None => not_found(),
            },
            ("GET", ["git", "trees", sha]) => match w.tree(sha) {
                Some(tree) => json_response(200, json!({"sha": sha, "truncated": false, "tree": tree.iter().map(|(p, (mode, blob))|
                    json!({"path": p, "mode": mode, "type": "blob", "sha": blob})).collect::<Vec<_>>()})),
                None => not_found(),
            },
            ("GET", ["git", "blobs", sha]) => match w.blob(sha) {
                Some(bytes) => {
                    // GitHub wraps base64 at 60 columns.
                    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
                    let wrapped: Vec<String> = encoded.as_bytes().chunks(60).map(|c| String::from_utf8_lossy(c).into_owned()).collect();
                    json_response(200, json!({"sha": sha, "encoding": "base64", "content": wrapped.join("\n")}))
                }
                None => not_found(),
            },
            ("POST", ["git", "blobs"]) if !access.contents => forbidden(),
            ("POST", ["git", "blobs"]) => {
                let content = body["content"].as_str().unwrap_or_default();
                let bytes = base64::engine::general_purpose::STANDARD.decode(content).unwrap_or_default();
                json_response(201, json!({"sha": w.create_blob(bytes)}))
            }
            ("POST", ["git", "trees"]) if !access.contents => forbidden(),
            ("POST", ["git", "trees"]) => {
                let entries: Vec<(String, String, Option<String>)> = body["tree"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|e| (e["path"].as_str().unwrap_or_default().to_owned(), e["mode"].as_str().unwrap_or_default().to_owned(), e["sha"].as_str().map(str::to_owned)))
                    .collect();
                match w.create_tree(body["base_tree"].as_str(), &entries) {
                    Some(sha) => json_response(201, json!({"sha": sha})),
                    None => json_response(422, json!({"message": "base_tree is invalid"})),
                }
            }
            ("POST", ["git", "commits"]) if !access.contents => forbidden(),
            ("POST", ["git", "commits"]) => {
                let parents: Vec<CommitSha> = body["parents"].as_array().into_iter().flatten().filter_map(|p| p.as_str()?.parse().ok()).collect();
                match w.create_commit(body["message"].as_str().unwrap_or_default(), body["tree"].as_str().unwrap_or_default(), &parents) {
                    Some(sha) => json_response(201, json!({"sha": sha.to_string()})),
                    None => json_response(422, json!({"message": "Tree SHA does not exist"})),
                }
            }
            ("POST", ["git", "refs"]) if !access.contents => forbidden(),
            ("POST", ["git", "refs"]) => {
                let git_ref = body["ref"].as_str().unwrap_or_default();
                let Some(branch) = git_ref.strip_prefix("refs/heads/") else { return json_response(422, json!({"message": "bad ref"})) };
                let Some(sha) = body["sha"].as_str().and_then(|s| s.parse().ok()) else { return json_response(422, json!({"message": "bad sha"})) };
                match w.create_ref(branch, sha) {
                    Ok(()) => json_response(201, json!({"ref": git_ref, "object": {"sha": sha.to_string()}})),
                    Err(_) => json_response(422, json!({"message": "Reference already exists"})),
                }
            }
            ("GET", ["compare", range]) => {
                let Some((base, head)) = range.split_once("...") else { return not_found() };
                match (base.parse().ok(), head.parse().ok()) {
                    (Some(b), Some(h)) => match w.compare(b, h) {
                        Some(c) => json_response(200, json!({"ahead_by": c.ahead_by, "behind_by": c.behind_by,
                            "files": c.changed_paths.iter().map(|p| json!({"filename": p.as_str()})).collect::<Vec<_>>()})),
                        None => not_found(),
                    },
                    _ => not_found(),
                }
            }
            ("GET", ["pulls"]) => {
                let head = req.url.query_pairs().find(|(k, _)| k == "head").map(|(_, v)| v.into_owned()).unwrap_or_default();
                let branch = head.split_once(':').map(|(_, b)| b.to_owned()).unwrap_or_default();
                let open: Vec<Value> = w.find_open_pull(&branch).iter().map(|p| self.pull_json(p)).collect();
                json_response(200, Value::Array(open))
            }
            ("POST", ["pulls"]) if !access.pull_requests => forbidden(),
            ("POST", ["pulls"]) => {
                let head = body["head"].as_str().unwrap_or_default();
                if w.branch(head).is_none() {
                    return json_response(422, json!({"message": "Validation Failed: head invalid"}));
                }
                match w.open_pull(head, body["base"].as_str().unwrap_or_default(), body["title"].as_str().unwrap_or_default(), body["body"].as_str().unwrap_or_default(), body["draft"].as_bool().unwrap_or(false)) {
                    Ok(p) => json_response(201, self.pull_json(&p)),
                    Err(n) => json_response(422, json!({"message": format!("Validation Failed: A pull request already exists for acme:{head} (#{n}).")})),
                }
            }
            ("GET", ["pulls", n]) => match n.parse().ok().and_then(|n| w.pull(n)) {
                Some(p) => json_response(200, self.pull_json(&p)),
                None => not_found(),
            },
            ("POST", ["issues", _, "labels"]) if !access.pull_requests => forbidden(),
            ("POST", ["issues", n, "labels"]) => {
                let labels: Vec<String> = body["labels"].as_array().into_iter().flatten().filter_map(|l| l.as_str().map(str::to_owned)).collect();
                if n.parse().is_ok_and(|n| w.add_labels(n, &labels)) { json_response(200, json!([])) } else { not_found() }
            }
            ("POST", ["pulls", n, "requested_reviewers"]) => {
                let strings = |key: &str| -> Vec<String> { body[key].as_array().into_iter().flatten().filter_map(|l| l.as_str().map(str::to_owned)).collect() };
                let who = Reviewers { users: strings("reviewers"), teams: strings("team_reviewers") };
                if n.parse().is_ok_and(|n| w.request_reviewers(n, &who)) { json_response(201, json!({})) } else { not_found() }
            }
            _ => not_found(),
        }
    }
}

impl Respond for Emulator {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let p = req.url.path().to_owned();
        if p == format!("/app/installations/{INSTALLATION}/access_tokens") && req.method.as_str() == "POST" {
            let a = self.world.access();
            let level = |on: bool| if on { "write" } else { "read" };
            return json_response(201, json!({"token": "ghs_fixapptoken", "expires_at": "2026-01-01T01:00:00Z",
                "permissions": {"contents": level(a.contents), "pull_requests": level(a.pull_requests), "workflows": level(a.workflows), "issues": level(a.issues), "metadata": "read"}}));
        }
        if p == format!("/app/installations/{INSTALLATION}") && req.method.as_str() == "GET" {
            let a = self.world.access();
            let level = |on: bool| if on { "write" } else { "read" };
            return json_response(200, json!({"id": INSTALLATION, "account": {"login": "acme", "type": "Organization"}, "repository_selection": "selected",
                "suspended_at": null, "permissions": {"contents": level(a.contents), "pull_requests": level(a.pull_requests), "workflows": level(a.workflows), "issues": level(a.issues)}}));
        }
        if p == "/user-attachments/assets" && req.method.as_str() == "POST" {
            if !self.world.uploads_accepted() {
                return not_found();
            }
            let q = |k: &str| req.url.query_pairs().find(|(key, _)| key == k).map(|(_, v)| v.into_owned()).unwrap_or_default();
            let url = self.world.record_upload(&q("name"), &q("content_type"), req.body.len());
            return json_response(201, json!({"url": url}));
        }
        let prefix = format!("/repos/{}/{}/", self.world.owner(), self.world.name());
        match p.strip_prefix(&prefix) {
            Some(rest) => self.repo_call(req, rest),
            None => not_found(),
        }
    }
}

struct Harness {
    server: MockServer,
    world: GitHubWorld,
    prs: GitHubPullRequests,
    repo: Repo,
    org: OrgId,
}

async fn harness() -> Harness {
    let server = MockServer::start().await;
    let world = GitHubWorld::new("acme", "shop");
    Mock::given(wiremock::matchers::any()).respond_with(Emulator { world: world.clone() }).with_priority(10).mount(&server).await;

    let clock = Arc::new(FakeClock::at(datetime!(2026-01-01 00:00 UTC)));
    let jobs = Arc::new(MemJobQueue::new(clock.clone(), Arc::new(SeqIdGen::new())));
    let orgs = Arc::new(MemOrgStore::new(clock.clone(), jobs, Arc::new(MemChangeFeed::new(clock.clone()))));
    let repo = test_repo("acme", "shop");
    orgs.upsert_integration(
        &Integration { org_id: repo.org_id, kind: IntegrationKind::GitHubFix, external_id: INSTALLATION.to_string(), status: IntegrationStatus::Active },
        &[],
    )
    .await
    .unwrap();

    let base = Url::parse(&format!("{}/", server.uri())).unwrap();
    let config = GitHubConfig { api_base: base.clone(), web_base: base.clone(), ..GitHubConfig::default() };
    let client = GitHubClient::new(&config, AppCredentials { client_id: "Iv1.fix".into(), private_key_pem: APP_KEY.into() }, clock.clone()).unwrap();
    let prs = GitHubPullRequests::new(
        client,
        orgs,
        clock,
        PullRequestsConfig { uploads_base: base, assets_prefix: "https://github.com/user-attachments/assets/".into(), upload_timeout_ms: 5_000 },
    )
    .unwrap();
    let org = repo.org_id;
    Harness { server, world, prs, repo, org }
}

async fn requests(server: &MockServer, verb: &str, suffix: &str) -> Vec<Request> {
    server.received_requests().await.unwrap_or_default().into_iter().filter(|r| r.method.as_str() == verb && r.url.path().ends_with(suffix)).collect()
}

#[tokio::test]
async fn adapter_passes_the_pull_requests_contract() {
    let h = harness().await;
    let w = h.world.clone();
    let plan = move |parent: CommitSha, writes: &[(&str, &[u8])], deletes: &[&str], message: &str| {
        let files = w.files_at(parent);
        gated_plan(parent, &|p| files.get(p).map(|f| f.1.clone()), writes, deletes, message)
    };
    contract_pull_requests(PullRequestsFixture { prs: &h.prs, world: &h.world, org: h.org, repo: h.repo.clone(), plan: &plan }).await;
}

#[tokio::test]
async fn create_commit_sends_blobs_tree_with_deletions_and_commit_without_moving_refs() {
    let h = harness().await;
    let base = h.world.seed_commit(&[("src/a.ts", FileMode::Regular, b"one\n"), ("old.txt", FileMode::Regular, b"x\n")], "base", &[]);
    h.world.set_branch("main", base);
    let files = h.world.files_at(base);
    let plan = gated_plan(base, &|p| files.get(p).map(|f| f.1.clone()), &[("src/a.ts", b"two\n")], &["old.txt"], "fix: x (ENG-1)\n\nMage-Change-Set: ab\n");
    let commit = h.prs.create_commit(h.org, &h.repo, &plan).await.unwrap();

    let blobs = requests(&h.server, "POST", "/git/blobs").await;
    assert_eq!(blobs.len(), 1, "one blob per written file");
    let blob: Value = blobs[0].body_json().unwrap();
    assert_eq!(blob, json!({"content": base64::engine::general_purpose::STANDARD.encode(b"two\n"), "encoding": "base64"}));

    let trees = requests(&h.server, "POST", "/git/trees").await;
    let tree: Value = trees[0].body_json().unwrap();
    let base_tree = h.world.commit(base).unwrap().tree;
    assert_eq!(tree["base_tree"], json!(base_tree));
    let entries = tree["tree"].as_array().unwrap();
    assert_eq!(entries[0], json!({"path": "old.txt", "mode": "100644", "type": "blob", "sha": null}), "deletion entry");
    assert_eq!(entries[1]["path"], json!("src/a.ts"));
    assert_eq!(entries[1]["mode"], json!("100644"));

    let commits = requests(&h.server, "POST", "/git/commits").await;
    let body: Value = commits[0].body_json().unwrap();
    assert_eq!(body["parents"], json!([base.to_string()]));
    assert_eq!(body["message"], json!("fix: x (ENG-1)\n\nMage-Change-Set: ab\n"));
    assert!(requests(&h.server, "POST", "/git/refs").await.is_empty(), "create_commit moves no ref");
    assert!(!h.world.files_at(commit).contains_key("old.txt"));

    // REST calls authenticate with the fix installation's token.
    let auth = commits[0].headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned();
    assert_eq!(auth, "Bearer ghs_fixapptoken");
}

#[tokio::test]
async fn create_branch_posts_mage_refs_and_resolves_existing_ones() {
    let h = harness().await;
    let base = h.world.seed_commit(&[("a", FileMode::Regular, b"a\n")], "base", &[]);
    let other = h.world.seed_commit(&[("a", FileMode::Regular, b"b\n")], "other", &[base]);
    let branch = BranchName::new("mage/eng-1-abc1234").unwrap();
    assert_eq!(h.prs.create_branch(h.org, &h.repo, &branch, base).await.unwrap(), BranchOutcome::Created);
    let refs = requests(&h.server, "POST", "/git/refs").await;
    let body: Value = refs[0].body_json().unwrap();
    assert_eq!(body, json!({"ref": "refs/heads/mage/eng-1-abc1234", "sha": base.to_string()}));
    assert_eq!(h.prs.create_branch(h.org, &h.repo, &branch, base).await.unwrap(), BranchOutcome::AlreadyAt);
    assert_eq!(h.prs.create_branch(h.org, &h.repo, &branch, other).await.unwrap(), BranchOutcome::ExistsAt(base));
    assert_eq!(h.world.branch(branch.as_str()), Some(base), "never force-updated");
}

#[test]
fn refs_outside_mage_are_refused_before_any_request() {
    for bad in ["main", "refs/heads/main", "feature/mage/x", "mage/../main"] {
        assert!(matches!(refuse_foreign(bad), Err(PullRequestsError::Invalid(_))), "{bad}");
    }
    assert!(refuse_foreign("mage/eng-1-abc1234").is_ok());
    // The type itself cannot name a foreign branch.
    assert!(BranchName::new("main").is_err());
}

#[tokio::test]
async fn permission_and_rate_limit_responses_are_mapped() {
    let h = harness().await;
    let base = h.world.seed_commit(&[("a", FileMode::Regular, b"a\n")], "base", &[]);
    let files = h.world.files_at(base);
    let plan = gated_plan(base, &|p| files.get(p).map(|f| f.1.clone()), &[("a", b"b\n")], &[], "fix: a\n\nMage-Change-Set: 01\n");

    h.world.set_access(WriteAccess { contents: false, pull_requests: true, workflows: false, issues: true });
    let denied = h.prs.create_commit(h.org, &h.repo, &plan).await;
    assert_eq!(denied, Err(PullRequestsError::PermissionDenied { missing: vec!["contents"] }));
    assert!(!h.prs.write_access(h.org, &h.repo).await.unwrap().contents);
    h.world.set_access(WriteAccess { contents: true, pull_requests: true, workflows: false, issues: true });

    Mock::given(method("GET"))
        .and(path_regex(r"/repos/acme/shop/pulls/\d+$"))
        .respond_with(
            ResponseTemplate::new(403)
                .insert_header("x-ratelimit-remaining", "0")
                .insert_header("x-ratelimit-reset", (datetime!(2026-01-01 00:02 UTC).unix_timestamp()).to_string())
                .set_body_json(json!({"message": "API rate limit exceeded"})),
        )
        .with_priority(1)
        .mount(&h.server)
        .await;
    let limited = h.prs.state(h.org, &h.repo, 1).await;
    assert_eq!(limited, Err(PullRequestsError::RateLimited { retry_after: std::time::Duration::from_secs(120) }));
}

#[tokio::test]
async fn missing_fix_integration_is_permission_denied() {
    let h = harness().await;
    let other = OrgId::from_uuid(uuid::Uuid::from_u128(99));
    let res = h.prs.branch_head(other, &h.repo, "main").await;
    assert_eq!(res, Err(PullRequestsError::PermissionDenied { missing: vec!["fix_app_installation"] }));
}

#[tokio::test]
async fn upload_media_posts_raw_bytes_and_returns_the_asset_url() {
    let h = harness().await;
    let media = MediaUpload { name: "after.mp4".into(), kind: MediaKind::Mp4, bytes: b"\x00\x00\x00\x18ftypmp42".to_vec() };
    let url = h.prs.upload_media(h.org, &h.repo, &media).await.unwrap();
    assert!(url.0.as_str().starts_with("https://github.com/user-attachments/assets/"));

    let uploads = requests(&h.server, "POST", "/user-attachments/assets").await;
    let req = &uploads[0];
    let q: std::collections::HashMap<String, String> = req.url.query_pairs().into_owned().collect();
    assert_eq!((q["name"].as_str(), q["content_type"].as_str(), q["repository_id"].as_str()), ("after.mp4", "video/mp4", "4242"));
    assert_eq!(req.body, media.bytes);
    let header = |k: &str| req.headers.get(k).and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned();
    assert_eq!(header("authorization"), "token ghs_fixapptoken");
    assert_eq!(header("content-type"), "application/octet-stream");
}

#[tokio::test]
async fn upload_media_failures_are_errors_never_urls() {
    let h = harness().await;
    let media = MediaUpload { name: "before.gif".into(), kind: MediaKind::Gif, bytes: b"GIF89a".to_vec() };
    h.world.set_uploads_accepted(false);
    assert_eq!(h.prs.upload_media(h.org, &h.repo, &media).await, Err(PullRequestsError::NotFound), "installation tokens get 404");

    h.world.set_uploads_accepted(true);
    Mock::given(method("POST"))
        .and(path("/user-attachments/assets"))
        .respond_with(json_response(201, json!({"url": "https://evil.example/x.gif"})))
        .with_priority(1)
        .mount(&h.server)
        .await;
    assert!(matches!(h.prs.upload_media(h.org, &h.repo, &media).await, Err(PullRequestsError::Unavailable(_))), "foreign URL refused");

    let huge = MediaUpload { name: "big.gif".into(), kind: MediaKind::Gif, bytes: vec![0; 10_000_001] };
    let before = requests(&h.server, "POST", "/user-attachments/assets").await.len();
    assert!(matches!(h.prs.upload_media(h.org, &h.repo, &huge).await, Err(PullRequestsError::Invalid(_))));
    assert_eq!(requests(&h.server, "POST", "/user-attachments/assets").await.len(), before, "oversize never sent");
}
