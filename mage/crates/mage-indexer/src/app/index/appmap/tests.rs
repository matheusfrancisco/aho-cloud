use std::sync::Arc;

use mage_core::domain::{CommitSha, FileClass, IndexPriority, Language, Layer, LayerState, RepoVersionKey};
use mage_core::ports::knowledge::{AppFact, AppFactRecord, AppNodeKind, FrameworkId, FrameworkScope, HandlerRef, PageReq, UrlPart, UrlTemplate};
use mage_core::ports::{KnowledgeStore, RepoStore};
use mage_core::testing::FakeClock;

use super::*;
use crate::ports::git::{FetchWants, GitRepoReader, MirrorRef};
use crate::testing::git::{FixtureCommit, FixtureRepo, MemGitRepo, git_blob_oid, test_fetch_spec};
use crate::testing::parser::FakeParser;
use crate::testing::world::{World, WorldFile, world_org, world_path};

const URL: &str = "https://github.com/acme/app.git";

const GO_MOD: &str = "module acme/app\nrequire github.com/gin-gonic/gin v1.9.1\n";
const PACKAGE_JSON: &str = r#"{"dependencies":{"axios":"1.6.0"}}"#;
const ROUTES_GO: &str = "package main\nfunc routes(r *gin.Engine) { r.GET(\"/api/connections\", list) }\n";
const API_TS: &str = "export const list = () => api.get('/connections')\n";
const VENDORED_GO: &str = "package x\n";

fn route_fact() -> AppFactRecord {
    AppFactRecord {
        ord: 0,
        framework: FrameworkId::Gin,
        fact: AppFact::RouteDecl {
            method: Some("GET".into()),
            path: UrlTemplate { parts: vec![UrlPart::Lit("/api/connections".into())] },
            group: Some("r".into()),
            handler: HandlerRef::Named { name: "list".into(), qualifier: None },
            middleware: Vec::new(),
        },
        line: 2,
        enclosing: None,
    }
}

fn call_fact() -> AppFactRecord {
    AppFactRecord {
        ord: 0,
        framework: FrameworkId::Axios,
        fact: AppFact::HttpCall {
            method: Some("GET".into()),
            url: UrlTemplate { parts: vec![UrlPart::Var(Some("baseURL".into())), UrlPart::Lit("/api/connections".into())] },
            client: "api".into(),
        },
        line: 1,
        enclosing: None,
    }
}

struct Env {
    w: World,
    git: Arc<MemGitRepo>,
    parser: Arc<FakeParser>,
    uc: IndexAppMap,
}

async fn env(parser: FakeParser) -> Env {
    let w = World::new();
    let git = Arc::new(MemGitRepo::new());
    let parser = Arc::new(parser);
    let uc = IndexAppMap::new(w.repos.clone(), w.knowledge.clone(), git.clone(), parser.clone(), w.clock.clone());
    Env { w, git, parser, uc }
}

impl Env {
    /// Commits `files` to the remote and fetches it; returns the head sha and its version key
    /// (Files and Symbols Ready, manifest recorded).
    async fn version(&self, files: &[(&str, &str, FileClass)]) -> (CommitSha, RepoVersionKey) {
        let repo = self.w.repo("app").await;
        let tree: Vec<(&str, &str)> = files.iter().map(|(p, c, _)| (*p, *c)).collect();
        let shas = self.git.add_remote(URL, &FixtureRepo::linear("main", vec![FixtureCommit::new("init", FakeClock::DEFAULT_START, &tree)]));
        let sha = shas[0];
        let mirror = MirrorRef::new(world_org(), repo.id);
        self.git.fetch(&mirror, &test_fetch_spec(URL), &FetchWants { all_heads_and_tags: true, shas: vec![sha] }).await.unwrap_or_else(|e| panic!("{e}"));
        let key = self.w.version(repo.id, sha, &[Layer::Files, Layer::Symbols]).await;
        let manifest: Vec<WorldFile<'_>> = files
            .iter()
            .map(|(path, content, class)| WorldFile {
                path,
                blob: git_blob_oid(content.as_bytes()),
                lang: self.parser.detect_language(&world_path(path), content.as_bytes()),
                class: *class,
                lines: 3,
            })
            .collect();
        self.w.manifest(&key, &manifest).await;
        (sha, key)
    }

    async fn run(&self, key: RepoVersionKey) -> Result<JobDone, AppError> {
        self.uc.run(world_org(), key.repo_id, key.sha, IndexPriority::Normal).await
    }

    async fn state(&self, key: RepoVersionKey) -> LayerState {
        let version = self.w.repos.version(world_org(), key).await.unwrap_or_else(|e| panic!("{e}"));
        version.layer(Layer::AppMap).cloned().unwrap_or(LayerState::Pending)
    }
}

fn scopes() -> Vec<FrameworkScope> {
    vec![
        FrameworkScope { root: None, framework: FrameworkId::Gin },
        FrameworkScope { root: Some(world_path("web")), framework: FrameworkId::Axios },
        FrameworkScope { root: None, framework: FrameworkId::SqlStrings },
    ]
}

#[tokio::test]
async fn extracts_scoped_blobs_links_and_completes() {
    let routes = git_blob_oid(ROUTES_GO.as_bytes());
    let api = git_blob_oid(API_TS.as_bytes());
    let parser = FakeParser::new().with_frameworks(scopes()).with_app_facts(routes, vec![route_fact()]).with_app_facts(api, vec![call_fact()]);
    let e = env(parser).await;
    let (_, key) = e
        .version(&[
            ("go.mod", GO_MOD, FileClass::Config),
            ("main.go", ROUTES_GO, FileClass::Source),
            ("web/package.json", PACKAGE_JSON, FileClass::Config),
            ("web/src/api.ts", API_TS, FileClass::Source),
            ("vendor/x/x.go", VENDORED_GO, FileClass::Vendored),
            ("main_test.go", "package main\n", FileClass::Test),
        ])
        .await;
    assert_eq!(e.run(key).await.unwrap_or_else(|err| panic!("{err}")), JobDone::done());

    let calls = e.parser.app_fact_calls();
    assert_eq!(calls.len(), 1, "one batch");
    let mut got = calls[0].clone();
    got.sort();
    let mut want = vec![
        (routes, vec![FrameworkId::Gin, FrameworkId::SqlStrings]),
        (api, vec![FrameworkId::Gin, FrameworkId::Axios, FrameworkId::SqlStrings]),
    ];
    want.sort();
    assert_eq!(got, want, "Source blobs only, with the frameworks of every root containing them");

    assert!(e.state(key).await.is_ready());
    let nodes = e.w.knowledge.app_nodes(world_org(), &key, &[], &PageReq::first(100)).await.unwrap_or_else(|err| panic!("{err}")).items;
    let endpoint = nodes.iter().find(|n| n.kind == AppNodeKind::Endpoint).unwrap_or_else(|| panic!("no endpoint: {nodes:#?}"));
    assert_eq!(endpoint.key, "GET /api/connections");
    let client = nodes.iter().find(|n| n.kind == AppNodeKind::ClientCall).unwrap_or_else(|| panic!("no client call: {nodes:#?}"));
    let graph = e.w.knowledge.app_neighborhood(world_org(), &key, &[client.id], 1, 10).await.unwrap_or_else(|err| panic!("{err}"));
    assert!(graph.edges.iter().any(|edge| edge.from == client.id && edge.to == endpoint.id), "{graph:#?}");

    // Done at this rev: a retry does nothing.
    assert_eq!(e.run(key).await.unwrap_or_else(|err| panic!("{err}")), JobDone::done());
    assert_eq!(e.parser.app_fact_calls().len(), 1);
}

#[tokio::test]
async fn unchanged_blobs_are_not_extracted_again() {
    let routes = git_blob_oid(ROUTES_GO.as_bytes());
    let parser = FakeParser::new().with_frameworks(scopes()).with_app_facts(routes, vec![route_fact()]);
    let e = env(parser).await;
    let (_, first) = e.version(&[("go.mod", GO_MOD, FileClass::Config), ("main.go", ROUTES_GO, FileClass::Source)]).await;
    e.run(first).await.unwrap_or_else(|err| panic!("{err}"));

    // A second sha of the same repo sharing main.go and adding one file.
    let repo = first.repo_id;
    let extra = "package main\nfunc other() {}\n";
    let shas = e.git.add_remote(URL, &FixtureRepo::linear("main", vec![FixtureCommit::new("next", FakeClock::DEFAULT_START, &[
        ("go.mod", GO_MOD),
        ("main.go", ROUTES_GO),
        ("other.go", extra),
    ])]));
    let mirror = MirrorRef::new(world_org(), repo);
    e.git.fetch(&mirror, &test_fetch_spec(URL), &FetchWants { all_heads_and_tags: true, shas: shas.clone() }).await.unwrap_or_else(|err| panic!("{err}"));
    let second = e.w.version(repo, shas[0], &[Layer::Files, Layer::Symbols]).await;
    e.w.manifest(&second, &[
        WorldFile { path: "go.mod", blob: git_blob_oid(GO_MOD.as_bytes()), lang: None, class: FileClass::Config, lines: 2 },
        WorldFile::source("main.go", routes, Language::Go),
        WorldFile::source("other.go", git_blob_oid(extra.as_bytes()), Language::Go),
    ])
    .await;
    e.run(second).await.unwrap_or_else(|err| panic!("{err}"));
    let calls = e.parser.app_fact_calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1].iter().map(|(oid, _)| *oid).collect::<Vec<_>>(), [git_blob_oid(extra.as_bytes())], "only the new blob");
    let nodes = e.w.knowledge.app_nodes(world_org(), &second, &[AppNodeKind::Endpoint], &PageReq::first(10)).await.unwrap_or_else(|err| panic!("{err}")).items;
    assert_eq!(nodes.len(), 1, "cached facts still link at the new sha");
}

#[tokio::test]
async fn no_framework_skips_the_layer() {
    let e = env(FakeParser::new().with_frameworks(scopes())).await;
    let (_, key) = e.version(&[("README.md", "# app\n", FileClass::Doc), ("main.go", ROUTES_GO, FileClass::Source)]).await;
    e.run(key).await.unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(e.state(key).await, LayerState::Skipped { reason: NO_FRAMEWORK.into() });
    assert!(e.parser.app_fact_calls().is_empty());
    // Skipped stays skipped on a retry.
    e.run(key).await.unwrap_or_else(|err| panic!("{err}"));
    assert!(matches!(e.state(key).await, LayerState::Skipped { .. }));
}

#[tokio::test]
async fn waits_for_symbols() {
    let e = env(FakeParser::new().with_frameworks(scopes())).await;
    let repo = e.w.repo("app").await;
    let key = e.w.version(repo.id, CommitSha::from_bytes([3; 20]), &[Layer::Files]).await;
    assert!(matches!(e.run(key).await, Err(AppError::Unavailable(_))));
    assert_eq!(e.state(key).await, LayerState::Pending);
}
