//! Pipeline over fakes (knowledge.md "Testing", Use cases): `MemRepoStore`, `MemKnowledgeStore`,
//! `MemJobQueue`, `MemArtifactStore`, `FakeSourceHost`, `MemGitRepo`, `FakeParser`, driven by a
//! minimal lease loop. `index.app_map` / `index.history` belong to IndexerGraph's use cases: the
//! loop completes them without running them, so their layers stay `Pending` here.

use std::collections::BTreeMap;
use std::sync::Arc;

use mage_core::domain::knowledge::tokenize;
use mage_core::domain::{
    BlobOid, CommitSha, IndexPriority, InstallationId, JobKind, JobLane, JobPayload, Language, Layer, LayerState, LineRange, OrgId, Repo,
    RepoAccess, RepoId, RepoVersion, RepoVersionKey, TicketId, VersionReason,
};
use mage_core::ports::blob::BlobKey;
use mage_core::ports::jobs::{JobState, LeaseRequest};
use mage_core::ports::knowledge::{
    Catalog, CatalogEntry, CatalogFormat, Construct, ErrorQuery, LitPart, LiteralContext, LiteralSite, NameMatch, PageReq, ParsedBlob,
    ParserRev, AppMapRev, StageStatus, SymbolDef, SymbolKind, SymbolQuery,
};
use mage_core::ports::source_host::{Access, AccountKind, InstallationInfo, RemoteRepo, RepoCoords, RepoSelection};
use mage_core::ports::{ArtifactStore, Clock, JobQueue, KnowledgeStore, RepoStore};
use mage_core::testing::{
    FakeClock, FakeSourceHost, MemArtifactStore, MemJobQueue, MemKnowledgeStore, MemRepoStore, SeqIdGen, SourceHostMethod, SourceHostWorld,
    WorldInstallation, WorldRepo,
};
use time::OffsetDateTime;
use time::macros::datetime;
use uuid::Uuid;

use super::errors::ERRORS_REV;
use super::layers::reasons;
use super::{IndexConfig, IndexDeps, IndexPipeline, IndexTarget, JobDone};
use crate::error::AppError;
use crate::ports::git::{GitError, MirrorRef};
use crate::testing::git::{FixtureCommit, FixtureRepo, GitMethod, MemGitRepo, git_blob_oid};
use crate::testing::parser::FakeParser;

const URL: &str = "https://github.com/acme/api.git";
const T0: OffsetDateTime = datetime!(2026-10-01 09:00 UTC);

const MAIN_GO: &str = "package main\n\nimport \"fmt\"\n\nfunc connect(name string) error {\n\treturn fmt.Errorf(\"failed fetching connection %s: %w\", name, errTimeout)\n}\n";
const STRINGS_GO: &str = "package util\n\nfunc Upper(s string) string { return s }\n";
const STRINGS_GO_V2: &str = "package util\n\nfunc Upper(s string) string { return strings.ToUpper(s) }\n";
const APP_TS: &str = "export function app() { return 1 }\n";
const ERRORS_JSON: &str = "{\"errors\": {\"conn_not_found\": \"Connection {{name}} not found\"}}\n";
const VENDOR_GO: &str = "package y\n";
const README: &str = "# api\n";

fn files_v1() -> Vec<(&'static str, &'static str)> {
    vec![
        ("README.md", README),
        ("main.go", MAIN_GO),
        ("util/strings.go", STRINGS_GO),
        ("vendor/x/y.go", VENDOR_GO),
        ("web/app.ts", APP_TS),
        ("web/locales/en/errors.json", ERRORS_JSON),
    ]
}

fn files_v2() -> Vec<(&'static str, &'static str)> {
    files_v1().into_iter().map(|(p, c)| if p == "util/strings.go" { (p, STRINGS_GO_V2) } else { (p, c) }).collect()
}

struct Harness {
    clock: Arc<FakeClock>,
    jobs: Arc<MemJobQueue>,
    repos: Arc<MemRepoStore>,
    knowledge: Arc<MemKnowledgeStore>,
    artifacts: Arc<MemArtifactStore>,
    source: Arc<FakeSourceHost>,
    git: Arc<MemGitRepo>,
    parser: Arc<FakeParser>,
    data: tempfile::TempDir,
    org: OrgId,
    repo: RepoId,
    shas: Vec<CommitSha>,
}

fn org() -> OrgId {
    OrgId::from_uuid(Uuid::from_u128(0x0190_0000_0000_7000_8000_0000_0000_0001))
}

fn repo_id() -> RepoId {
    RepoId::from_uuid(Uuid::from_u128(0x0190_0000_0000_7000_8000_0000_0000_0002))
}

/// The parse result of `main.go`: one function and its `fmt.Errorf` literal.
fn main_go_parsed() -> ParsedBlob {
    ParsedBlob {
        lang: Language::Go,
        status: StageStatus::Ok,
        symbols: vec![SymbolDef {
            ord: 0,
            name: "connect".into(),
            qualified: "main.connect".into(),
            kind: SymbolKind::Function,
            container: None,
            span: LineRange::new(5, 7).unwrap(),
            name_pos: (5, 6),
            signature: Some("func connect(name string) error {".into()),
            doc: None,
            exported: false,
        }],
        refs: Vec::new(),
        literals: vec![LiteralSite {
            ord: 0,
            parts: vec![LitPart::Text("failed fetching connection %s: %w".into())],
            pos: (6, 20),
            enclosing: Some(0),
            context: LiteralContext {
                callee: Some("fmt.Errorf".into()),
                arg_index: Some(0),
                siblings: Vec::new(),
                construct: Construct::Return,
                exception_type: None,
                map_key: None,
                attribute: None,
            },
        }],
    }
}

fn catalog() -> Catalog {
    Catalog {
        format: CatalogFormat::Json,
        entries: vec![CatalogEntry { key: "errors.conn_not_found".into(), locale: Some("en".into()), message: "Connection {{name}} not found".into(), line: 1 }],
    }
}

fn base_parser() -> FakeParser {
    FakeParser::new()
        .with_parsed(git_blob_oid(MAIN_GO.as_bytes()), main_go_parsed())
        .with_catalog(git_blob_oid(ERRORS_JSON.as_bytes()), catalog())
}

async fn harness_with(fixture: FixtureRepo, parser: FakeParser) -> Harness {
    let clock = Arc::new(FakeClock::at(datetime!(2026-10-05 10:00 UTC)));
    let ids = Arc::new(SeqIdGen::new());
    let jobs = Arc::new(MemJobQueue::new(clock.clone(), ids.clone()));
    let repos = Arc::new(MemRepoStore::new(clock.clone(), jobs.clone()));
    let knowledge = Arc::new(MemKnowledgeStore::new(clock.clone(), repos.clone()));
    let artifacts = Arc::new(MemArtifactStore::new(clock.clone()));
    let git = Arc::new(MemGitRepo::new());
    let shas = git.add_remote(URL, &fixture);
    let inst = InstallationId(42);
    let head = shas.last().copied().unwrap();
    let world = SourceHostWorld {
        installations: vec![WorldInstallation {
            info: InstallationInfo {
                id: inst,
                account_login: "acme".into(),
                account_kind: AccountKind::Organization,
                permissions: BTreeMap::from([("contents".to_owned(), Access::Read), ("pull_requests".to_owned(), Access::Read)]),
                repository_selection: RepoSelection::All,
                suspended: false,
            },
            repos: vec![WorldRepo {
                remote: RemoteRepo {
                    external_id: 7,
                    coords: RepoCoords { owner: "acme".into(), name: "api".into() },
                    default_branch: "main".into(),
                    private: true,
                    archived: false,
                },
                branches: BTreeMap::from([("main".to_owned(), head)]),
                tags: BTreeMap::new(),
                commits: shas.clone(),
                pull_requests: Vec::new(),
            }],
        }],
    };
    let source = Arc::new(FakeSourceHost::new(clock.clone(), world));
    let (org, repo) = (org(), repo_id());
    repos
        .upsert_repo(
            &Repo {
                id: repo,
                org_id: org,
                external_id: 7,
                owner: "acme".into(),
                name: "api".into(),
                default_branch: "main".into(),
                installation_id: inst,
                access: RepoAccess::Granted,
                created_at: T0,
            },
            &[],
        )
        .await
        .unwrap();
    let data = tempfile::tempdir().unwrap();
    Harness { clock, jobs, repos, knowledge, artifacts, source, git, parser: Arc::new(parser), data, org, repo, shas }
}

async fn harness() -> Harness {
    let fixture = FixtureRepo::linear("main", vec![FixtureCommit::new("init", T0, &files_v1()), FixtureCommit::new("upper", T0 + time::Duration::hours(1), &files_v2())]);
    harness_with(fixture, base_parser()).await
}

impl Harness {
    fn pipeline(&self) -> IndexPipeline {
        self.pipeline_with(self.parser.clone(), IndexConfig::new(self.data.path()))
    }

    fn pipeline_with(&self, parser: Arc<FakeParser>, config: IndexConfig) -> IndexPipeline {
        let deps = IndexDeps {
            repos: self.repos.clone(),
            jobs: self.jobs.clone(),
            knowledge: self.knowledge.clone(),
            artifacts: self.artifacts.clone(),
            source: self.source.clone(),
            git: self.git.clone(),
            parser,
            clock: self.clock.clone(),
            ids: Arc::new(SeqIdGen::new()),
        };
        IndexPipeline::new(deps, config)
    }

    fn key(&self, n: usize) -> RepoVersionKey {
        RepoVersionKey::new(self.repo, self.shas[n])
    }

    fn mirror(&self) -> MirrorRef {
        MirrorRef::new(self.org, self.repo)
    }

    fn target(&self, n: usize) -> IndexTarget {
        IndexTarget::new(self.org, self.repo, self.shas[n], IndexPriority::Highest)
    }

    async fn enqueue_version(&self, n: usize, reason: VersionReason) {
        let sha = self.shas[n];
        self.jobs.enqueue(mage_core::ports::jobs::NewJob::new(JobPayload::IndexVersion { org_id: self.org, repo_id: self.repo, sha, reason })).await.unwrap();
    }

    async fn version(&self, n: usize) -> RepoVersion {
        self.repos.version(self.org, self.key(n)).await.unwrap()
    }

    fn kinds(&self, kind: JobKind) -> Vec<JobState> {
        self.jobs.jobs().into_iter().filter(|j| j.kind == kind).map(|j| j.state).collect()
    }
}

async fn dispatch(p: &IndexPipeline, payload: &JobPayload) -> Result<JobDone, AppError> {
    match payload {
        JobPayload::IndexVersion { org_id, repo_id, sha, reason } => p.version(*org_id, *repo_id, *sha, reason.clone()).await,
        JobPayload::IndexFetch { org_id, repo_id, sha, prio } => p.fetch(IndexTarget::new(*org_id, *repo_id, *sha, *prio)).await,
        JobPayload::IndexManifest { org_id, repo_id, sha, prio } => p.manifest(IndexTarget::new(*org_id, *repo_id, *sha, *prio)).await,
        JobPayload::IndexParse { org_id, repo_id, sha, prio, batch, oids } => p.parse(IndexTarget::new(*org_id, *repo_id, *sha, *prio), *batch, oids).await,
        JobPayload::IndexErrorSites { org_id, repo_id, sha, prio } => p.error_sites(IndexTarget::new(*org_id, *repo_id, *sha, *prio)).await,
        JobPayload::IndexBundleSource { org_id, repo_id, sha, prio } => p.bundle_source(IndexTarget::new(*org_id, *repo_id, *sha, *prio)).await,
        JobPayload::IndexReconcile => p.reconcile().await,
        JobPayload::IndexGc => p.gc().await,
        // IndexerGraph's use cases.
        JobPayload::IndexAppMap { .. } | JobPayload::IndexHistory { .. } | JobPayload::IndexIngestScip { .. } => Ok(JobDone::done()),
        other => panic!("not an indexer job: {other:?}"),
    }
}

/// Lease and run every ready indexer job; returns their kinds in run order.
async fn drive(h: &Harness, p: &IndexPipeline) -> Vec<JobKind> {
    let mut ran = Vec::new();
    loop {
        let request = LeaseRequest { lane: JobLane::Indexer, kinds: None, worker: "test".into(), max: 1 };
        let Some(job) = h.jobs.lease(&request).await.unwrap().into_iter().next() else { break };
        ran.push(job.kind);
        match dispatch(p, &job.payload).await {
            Ok(JobDone::Complete(follow_ups)) => h.jobs.complete(&job.lease, &follow_ups).await.unwrap(),
            Ok(JobDone::Defer(at)) => h.jobs.defer(&job.lease, at).await.unwrap(),
            Err(err) => panic!("{} failed: {err}", job.kind),
        }
        assert!(ran.len() < 200, "runaway DAG: {ran:?}");
    }
    ran
}

fn state(version: &RepoVersion, layer: Layer) -> &LayerState {
    version.layer(layer).unwrap()
}

fn is_ready(version: &RepoVersion, layer: Layer) -> bool {
    state(version, layer).is_ready()
}

fn skipped(reason: &str) -> LayerState {
    LayerState::Skipped { reason: reason.to_owned() }
}

fn failed(reason: &str) -> LayerState {
    LayerState::Failed { reason: reason.to_owned() }
}

fn oid(content: &str) -> BlobOid {
    git_blob_oid(content.as_bytes())
}

#[tokio::test]
async fn a_version_goes_through_the_dag_layer_by_layer() {
    let h = harness().await;
    let p = h.pipeline();
    h.repos.set_default_head(h.org, h.repo, h.shas[0]).await.unwrap();
    h.enqueue_version(0, VersionReason::DefaultHead).await;
    let ran = drive(&h, &p).await;

    let pos = |kind| ran.iter().position(|k| *k == kind).unwrap_or_else(|| panic!("{kind} never ran: {ran:?}"));
    assert_eq!(ran[0], JobKind::IndexVersion);
    assert_eq!(ran[1], JobKind::IndexFetch);
    for successor in [JobKind::IndexManifest, JobKind::IndexHistory, JobKind::IndexBundleSource] {
        assert!(pos(successor) > pos(JobKind::IndexFetch));
    }
    assert!(pos(JobKind::IndexParse) > pos(JobKind::IndexManifest));
    assert!(pos(JobKind::IndexErrorSites) > pos(JobKind::IndexParse));
    assert!(pos(JobKind::IndexAppMap) > pos(JobKind::IndexParse));
    assert_eq!(ran.iter().filter(|k| **k == JobKind::IndexErrorSites).count(), 1);
    assert_eq!(ran.iter().filter(|k| **k == JobKind::IndexAppMap).count(), 1);

    let v = h.version(0).await;
    for layer in [Layer::Files, Layer::Symbols, Layer::ErrorSites] {
        assert!(is_ready(&v, layer), "{layer:?} is {:?}", state(&v, layer));
    }
    assert_eq!(state(&v, Layer::Refs), &skipped(reasons::NO_RECIPE));
    assert_eq!(state(&v, Layer::Semantic), &skipped(reasons::DISABLED));
    assert_eq!(v.revs, Some(p.current_revs()));

    // Files: manifest with classes and languages; text contents stored once per oid.
    let entries = h.knowledge.manifest_all(h.org, &h.key(0), &PageReq::first(100)).await.unwrap().items;
    let by_path: BTreeMap<&str, _> = entries.iter().map(|e| (e.path.as_str(), (e.class, e.lang, e.line_count))).collect();
    use mage_core::domain::FileClass::*;
    assert_eq!(by_path["main.go"], (Source, Some(Language::Go), 7));
    assert_eq!(by_path["vendor/x/y.go"], (Vendored, Some(Language::Go), 1));
    assert_eq!(by_path["README.md"], (Doc, Some(Language::Markdown), 1));
    assert_eq!(by_path["web/locales/en/errors.json"].0, Config);
    let content = h.artifacts.get(&BlobKey::blob_content(h.org, oid(MAIN_GO)), 1 << 20).await.unwrap();
    assert_eq!(content, MAIN_GO.as_bytes());

    // Symbols: only parse candidates (Source/Test in a parsed language) were parsed.
    let mut parsed = h.parser.parsed_oids();
    parsed.sort();
    let mut expected = vec![oid(MAIN_GO), oid(STRINGS_GO), oid(APP_TS)];
    expected.sort();
    assert_eq!(parsed, expected);
    let q = SymbolQuery { name: "connect".into(), qualifier: None, mode: NameMatch::Exact, kinds: vec![], lang: None, path_prefix: None, include_tests: true };
    let hits = h.knowledge.find_symbols(h.org, &h.key(0), &q, &PageReq::first(10)).await.unwrap().items;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].at.path.as_str(), "main.go");

    // ErrorSites: the fmt.Errorf site and the catalog entry match real log lines.
    let line = "level=error msg=\"failed fetching connection pg-prod: context deadline exceeded\"";
    let query = ErrorQuery { segments: vec![line.into()], tokens: tokenize(line), max_candidates: 50, limit: 10, include_low_specificity: false };
    let matches = h.knowledge.match_errors(h.org, &h.key(0), ERRORS_REV, &query).await.unwrap();
    assert_eq!(matches.len(), 1);
    assert!(matches[0].verified);
    assert_eq!((matches[0].path.as_str(), matches[0].site.pos), ("main.go", (6, 20)));
    let ui = "Connection pg-1 not found";
    let query = ErrorQuery { segments: vec![ui.into()], tokens: tokenize(ui), max_candidates: 50, limit: 10, include_low_specificity: false };
    let catalog = h.knowledge.match_catalog(h.org, &h.key(0), ERRORS_REV, &query).await.unwrap();
    assert_eq!(catalog.iter().map(|m| m.entry.key.as_str()).collect::<Vec<_>>(), vec!["errors.conn_not_found"]);

    // Bundle: stored under the tree key, recorded, runs notified exactly once; sha pinned.
    let bundle = v.bundle.clone().expect("bundle recorded");
    let stored = h.artifacts.get(&BlobKey::parse(&bundle.key).unwrap(), 1 << 30).await.unwrap();
    assert_eq!(stored.len() as u64, bundle.bytes);
    use sha2::Digest as _;
    assert_eq!(sha2::Sha256::digest(&stored).as_slice(), &bundle.sha256[..]);
    assert_eq!(BlobKey::parse(&bundle.key).unwrap(), BlobKey::source_bundle(h.org, h.repo, bundle.tree));
    assert_eq!(h.kinds(JobKind::RunsSourceReady), vec![JobState::Queued]);
    assert_eq!(h.git.pins(&h.mirror()), vec![h.shas[0]]);
    assert!(std::fs::read_dir(h.data.path().join("tmp")).unwrap().next().is_none(), "bundle staging file removed");
}

#[tokio::test]
async fn a_second_version_parses_only_the_changed_blob() {
    let h = harness().await;
    let p = h.pipeline();
    h.enqueue_version(0, VersionReason::Release("v1".into())).await;
    drive(&h, &p).await;
    let first_parse = h.parser.parsed_oids().len();
    let first_contents = h.artifacts.keys().len();

    h.enqueue_version(1, VersionReason::OnDemand(TicketId::from_uuid(Uuid::from_u128(5)))).await;
    drive(&h, &p).await;
    assert_eq!(&h.parser.parsed_oids()[first_parse..], &[oid(STRINGS_GO_V2)]);
    let v = h.version(1).await;
    assert!(is_ready(&v, Layer::Symbols) && is_ready(&v, Layer::ErrorSites));
    // One new content object and one new bundle (different tree).
    assert_eq!(h.artifacts.keys().len(), first_contents + 2);
    // The unchanged error site is visible at the new version too.
    let line = "failed fetching connection a: b";
    let query = ErrorQuery { segments: vec![line.into()], tokens: tokenize(line), max_candidates: 50, limit: 10, include_low_specificity: false };
    assert_eq!(h.knowledge.match_errors(h.org, &h.key(1), ERRORS_REV, &query).await.unwrap().len(), 1);
    // Commit 1 arrived with the branch fetch of the first version: no second network fetch.
    assert_eq!(h.git.calls().iter().filter(|(m, _)| *m == GitMethod::Fetch).count(), 1);
}

#[tokio::test]
async fn a_present_sha_skips_the_network() {
    let h = harness().await;
    let p = h.pipeline();
    p.fetch(h.target(1)).await.unwrap();
    let fetches = |h: &Harness| h.git.calls().iter().filter(|(m, _)| *m == GitMethod::Fetch).count();
    assert_eq!(fetches(&h), 1);
    // Commit 0 came with the branch fetch: no second network fetch, no token.
    let specs_before = h.source.calls().iter().filter(|c| c.method == SourceHostMethod::FetchSpec).count();
    let done = p.fetch(h.target(0)).await.unwrap();
    assert_eq!(fetches(&h), 1);
    assert_eq!(h.source.calls().iter().filter(|c| c.method == SourceHostMethod::FetchSpec).count(), specs_before);
    let JobDone::Complete(jobs) = done else { panic!("fetch defers") };
    let kinds: Vec<JobKind> = jobs.iter().map(|j| j.kind).collect();
    assert_eq!(kinds, vec![JobKind::IndexManifest, JobKind::IndexHistory, JobKind::IndexBundleSource]);
    assert!(jobs.iter().all(|j| j.payload.priority() == IndexPriority::Highest.value()));
    let mut pins = h.git.pins(&h.mirror());
    pins.sort();
    let mut expected = vec![h.shas[0], h.shas[1]];
    expected.sort();
    assert_eq!(pins, expected);
}

#[tokio::test]
async fn an_unreachable_sha_fails_every_layer_and_notifies_runs() {
    let h = harness().await;
    let p = h.pipeline();
    let ghost = CommitSha::from_bytes([0xee; 20]);
    h.repos.add_version_reason(h.org, h.repo, ghost, VersionReason::OnDemand(TicketId::from_uuid(Uuid::from_u128(5)))).await.unwrap();
    let t = IndexTarget::new(h.org, h.repo, ghost, IndexPriority::Highest);
    let JobDone::Complete(jobs) = p.fetch(t).await.unwrap() else { panic!("defer") };
    assert_eq!(jobs.iter().map(|j| j.kind).collect::<Vec<_>>(), vec![JobKind::RunsSourceReady]);
    let v = h.repos.version(h.org, t.key()).await.unwrap();
    for layer in Layer::ALL {
        assert_eq!(state(&v, layer), &failed(reasons::SHA_NOT_REACHABLE), "{layer:?}");
    }
}

#[tokio::test]
async fn revoked_access_fails_layers_without_retrying() {
    let h = harness().await;
    let p = h.pipeline();
    h.repos.add_version_reason(h.org, h.repo, h.shas[1], VersionReason::DefaultHead).await.unwrap();
    h.source.fail_next(SourceHostMethod::FetchSpec, mage_core::ports::source_host::SourceHostError::Unauthorized);
    let JobDone::Complete(jobs) = p.fetch(h.target(1)).await.unwrap() else { panic!("defer") };
    assert_eq!(jobs.iter().map(|j| j.kind).collect::<Vec<_>>(), vec![JobKind::RunsSourceReady]);
    assert_eq!(state(&h.version(1).await, Layer::Files), &failed(reasons::ACCESS_REVOKED));

    // The git remote rejecting the token has the same effect.
    let h = harness().await;
    let p = h.pipeline();
    h.repos.add_version_reason(h.org, h.repo, h.shas[1], VersionReason::DefaultHead).await.unwrap();
    h.git.fail_next(GitMethod::Fetch, GitError::Unauthorized);
    p.fetch(h.target(1)).await.unwrap();
    assert_eq!(state(&h.version(1).await, Layer::History), &failed(reasons::ACCESS_REVOKED));
    // A transient git failure is retried instead.
    let h2 = harness().await;
    h2.git.fail_next(GitMethod::Fetch, GitError::Unavailable("connection reset".into()));
    h2.repos.add_version_reason(h2.org, h2.repo, h2.shas[1], VersionReason::DefaultHead).await.unwrap();
    assert!(matches!(h2.pipeline().fetch(h2.target(1)).await, Err(AppError::Unavailable(_))));
}

#[tokio::test]
async fn a_superseded_default_head_is_skipped_and_the_head_does_not_move_back() {
    let h = harness().await;
    let p = h.pipeline();
    h.repos.set_default_head(h.org, h.repo, h.shas[0]).await.unwrap();
    h.repos.set_default_head(h.org, h.repo, h.shas[1]).await.unwrap();
    let done = p.version(h.org, h.repo, h.shas[0], VersionReason::DefaultHead).await.unwrap();
    assert_eq!(done, JobDone::done(), "no fetch for a superseded head");
    let v = h.version(0).await;
    assert!(v.layers.values().all(|s| *s == skipped(reasons::SUPERSEDED)));
    assert_eq!(h.repos.default_head(h.org, h.repo).await.unwrap(), Some(h.shas[1]));

    // A superseded head that another reason needs is still indexed.
    let h = harness().await;
    let p = h.pipeline();
    h.repos.set_default_head(h.org, h.repo, h.shas[0]).await.unwrap();
    h.repos.add_version_reason(h.org, h.repo, h.shas[0], VersionReason::Deployed("prod".into())).await.unwrap();
    h.repos.set_default_head(h.org, h.repo, h.shas[1]).await.unwrap();
    let JobDone::Complete(jobs) = p.version(h.org, h.repo, h.shas[0], VersionReason::DefaultHead).await.unwrap() else { panic!("defer") };
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].payload, JobPayload::IndexFetch { org_id: h.org, repo_id: h.repo, sha: h.shas[0], prio: IndexPriority::High });
}

#[tokio::test]
async fn a_settled_version_is_not_reindexed() {
    let h = harness().await;
    let p = h.pipeline();
    h.enqueue_version(0, VersionReason::Release("v1".into())).await;
    drive(&h, &p).await;
    // AppMap / History are IndexerGraph's: settle them the way their use cases would.
    for layer in [Layer::AppMap, Layer::History] {
        h.repos.apply_layer_event(h.org, h.key(0), layer, mage_core::domain::LayerEvent::Start, &[]).await.unwrap();
        h.repos.apply_layer_event(h.org, h.key(0), layer, mage_core::domain::LayerEvent::Complete { at: h.clock.now() }, &[]).await.unwrap();
    }
    let done = p.version(h.org, h.repo, h.shas[0], VersionReason::Release("v1.0.1".into())).await.unwrap();
    assert_eq!(done, JobDone::done());
    assert_eq!(h.version(0).await.reasons.len(), 2, "the new reason is still recorded");
}

#[tokio::test]
async fn retried_last_batch_enqueues_successors_once() {
    // 300 parse candidates → 2 batches.
    let mut files: Vec<(String, String)> = (0..300).map(|i| (format!("pkg/f{i:03}.go"), format!("package pkg\n\nconst N{i} = {i}\n"))).collect();
    files.push(("main.go".into(), MAIN_GO.into()));
    let refs: Vec<(&str, &str)> = files.iter().map(|(p, c)| (p.as_str(), c.as_str())).collect();
    let h = harness_with(FixtureRepo::linear("main", vec![FixtureCommit::new("init", T0, &refs)]), base_parser()).await;
    let p = h.pipeline();
    h.repos.add_version_reason(h.org, h.repo, h.shas[0], VersionReason::Release("v1".into())).await.unwrap();
    p.fetch(h.target(0)).await.unwrap();
    p.manifest(h.target(0)).await.unwrap();
    let parse_jobs: Vec<(u32, Vec<BlobOid>)> = h
        .jobs
        .jobs()
        .into_iter()
        .filter_map(|j| match j.payload {
            JobPayload::IndexParse { batch, oids, .. } => Some((batch, oids)),
            _ => None,
        })
        .collect();
    assert_eq!(parse_jobs.iter().map(|(b, o)| (*b, o.len())).collect::<Vec<_>>(), vec![(1, 256), (2, 45)]);
    assert_eq!(state(&h.version(0).await, Layer::Symbols), &LayerState::Running);

    p.parse(h.target(0), 1, &parse_jobs[0].1).await.unwrap();
    assert_eq!(state(&h.version(0).await, Layer::Symbols), &LayerState::Running);
    assert!(h.kinds(JobKind::IndexErrorSites).is_empty());
    p.parse(h.target(0), 2, &parse_jobs[1].1).await.unwrap();
    assert!(is_ready(&h.version(0).await, Layer::Symbols));
    assert_eq!(h.kinds(JobKind::IndexErrorSites), vec![JobState::Queued]);
    assert_eq!(h.kinds(JobKind::IndexAppMap), vec![JobState::Queued]);

    // error_sites runs and finishes; a retried last batch must not enqueue it again.
    p.error_sites(h.target(0)).await.unwrap();
    for job in h.jobs.jobs().into_iter().filter(|j| j.state == JobState::Queued) {
        h.jobs.cancel(job.id).await.unwrap();
    }
    p.parse(h.target(0), 2, &parse_jobs[1].1).await.unwrap();
    assert_eq!(h.kinds(JobKind::IndexErrorSites), vec![JobState::Cancelled]);
}

#[tokio::test]
async fn per_blob_parse_failures_are_markers_not_job_failures() {
    let parser = base_parser().with_parse_error(oid(APP_TS), crate::ports::parser::ParseError::Timeout);
    let fixture = FixtureRepo::linear("main", vec![FixtureCommit::new("init", T0, &files_v1())]);
    let h = harness_with(fixture, parser).await;
    let p = h.pipeline();
    h.enqueue_version(0, VersionReason::Release("v1".into())).await;
    drive(&h, &p).await;
    let v = h.version(0).await;
    assert!(is_ready(&v, Layer::Symbols));
    // The failed blob keeps its marker: a re-plan does not parse it again.
    let missing = h.knowledge.missing(h.org, mage_core::ports::knowledge::BlobStage::Parse(ParserRev(1)), &[oid(APP_TS)]).await.unwrap();
    assert!(missing.is_empty());
}

#[tokio::test]
async fn dead_jobs_fail_their_layer_and_skip_dependents() {
    let h = harness().await;
    let p = h.pipeline();
    let t = h.target(0);
    h.repos.add_version_reason(h.org, h.repo, t.sha, VersionReason::Release("v1".into())).await.unwrap();
    p.version(h.org, h.repo, t.sha, VersionReason::Release("v1".into())).await.unwrap();
    p.fetch(t).await.unwrap();
    p.manifest(t).await.unwrap();
    let parse = JobPayload::IndexParse { org_id: h.org, repo_id: h.repo, sha: t.sha, prio: t.prio, batch: 1, oids: vec![] };
    assert!(p.on_dead(&parse, "parser crashed").await.unwrap().is_empty());
    let v = h.version(0).await;
    assert_eq!(state(&v, Layer::Symbols), &failed("parser crashed"));
    assert_eq!(state(&v, Layer::ErrorSites), &skipped(reasons::SYMBOLS_FAILED));
    assert_eq!(state(&v, Layer::AppMap), &skipped(reasons::SYMBOLS_FAILED));
    assert_eq!(state(&v, Layer::History), &LayerState::Pending);
    assert!(is_ready(&v, Layer::Files));
    // Idempotent.
    p.on_dead(&parse, "again").await.unwrap();
    assert_eq!(state(&h.version(0).await, Layer::Symbols), &failed("parser crashed"));

    // A dead manifest fails Files and skips everything but History.
    let h = harness().await;
    let p = h.pipeline();
    p.version(h.org, h.repo, h.shas[1], VersionReason::Release("v2".into())).await.unwrap();
    let manifest = JobPayload::IndexManifest { org_id: h.org, repo_id: h.repo, sha: h.shas[1], prio: IndexPriority::Low };
    p.on_dead(&manifest, "disk full").await.unwrap();
    let v = h.version(1).await;
    assert_eq!(state(&v, Layer::Files), &failed("disk full"));
    for layer in [Layer::Symbols, Layer::ErrorSites, Layer::AppMap] {
        assert_eq!(state(&v, layer), &skipped(reasons::FILES_FAILED), "{layer:?}");
    }
    assert_eq!(state(&v, Layer::History), &LayerState::Pending);

    // A dead fetch fails every open layer and still lets runs know; a dead bundle notifies runs.
    let fetch = JobPayload::IndexFetch { org_id: h.org, repo_id: h.repo, sha: h.shas[1], prio: IndexPriority::Low };
    let jobs = p.on_dead(&fetch, "github down").await.unwrap();
    assert_eq!(jobs.iter().map(|j| j.kind).collect::<Vec<_>>(), vec![JobKind::RunsSourceReady]);
    assert_eq!(state(&h.version(1).await, Layer::History), &failed("github down"));
    let bundle = JobPayload::IndexBundleSource { org_id: h.org, repo_id: h.repo, sha: h.shas[1], prio: IndexPriority::Low };
    assert_eq!(p.on_dead(&bundle, "too large").await.unwrap()[0].payload, JobPayload::RunsSourceReady { org_id: h.org, repo_id: h.repo, sha: h.shas[1] });
    // Dead periodic jobs reschedule their chain.
    let jobs = p.on_dead(&JobPayload::IndexReconcile, "x").await.unwrap();
    assert_eq!(jobs[0].run_after, Some(h.clock.now() + super::reconcile::INTERVAL));
}

#[tokio::test]
async fn a_rev_bump_rebuilds_in_shadow_and_switches_revs_at_the_end() {
    let h = harness().await;
    let p1 = h.pipeline();
    h.enqueue_version(0, VersionReason::Release("v1".into())).await;
    drive(&h, &p1).await;
    assert_eq!(h.version(0).await.revs.map(|r| r.parser), Some(ParserRev(1)));

    // New deploy: ParserRev 2. Every candidate is re-parsed; Symbols stays Ready meanwhile.
    let p2 = h.pipeline_with(Arc::new(base_parser().with_revs(ParserRev(2), AppMapRev(1))), IndexConfig::new(h.data.path()));
    let t = h.target(0);
    p2.version(h.org, h.repo, t.sha, VersionReason::Release("v1".into())).await.unwrap();
    p2.fetch(t).await.unwrap();
    p2.manifest(t).await.unwrap();
    let v = h.version(0).await;
    assert!(is_ready(&v, Layer::Symbols), "readers keep the old facts during the rebuild");
    assert_eq!(v.revs.map(|r| r.parser), Some(ParserRev(1)));
    let q = SymbolQuery { name: "connect".into(), qualifier: None, mode: NameMatch::Exact, kinds: vec![], lang: None, path_prefix: None, include_tests: true };
    assert_eq!(h.knowledge.find_symbols(h.org, &h.key(0), &q, &PageReq::first(10)).await.unwrap().items.len(), 1);

    let batch = h
        .jobs
        .jobs()
        .into_iter()
        .rev()
        .find_map(|j| match j.payload {
            JobPayload::IndexParse { batch, oids, .. } => Some((batch, oids)),
            _ => None,
        })
        .unwrap();
    assert_eq!(batch.1.len(), 3);
    p2.parse(t, batch.0, &batch.1).await.unwrap();
    let v = h.version(0).await;
    assert!(is_ready(&v, Layer::Symbols));
    assert_eq!(v.revs.map(|r| (r.parser, r.errors)), Some((ParserRev(2), ERRORS_REV)));
    assert_eq!(h.knowledge.find_symbols(h.org, &h.key(0), &q, &PageReq::first(10)).await.unwrap().items.len(), 1);
}

#[tokio::test]
async fn identical_trees_share_one_bundle_and_oversized_bundles_fail_permanently() {
    let commits = vec![FixtureCommit::new("init", T0, &files_v1()), FixtureCommit::new("empty commit", T0 + time::Duration::hours(1), &files_v1())];
    let h = harness_with(FixtureRepo::linear("main", commits), base_parser()).await;
    let p = h.pipeline();
    for n in 0..2 {
        h.repos.add_version_reason(h.org, h.repo, h.shas[n], VersionReason::Release(format!("v{n}"))).await.unwrap();
    }
    p.fetch(h.target(1)).await.unwrap();
    p.bundle_source(h.target(0)).await.unwrap();
    p.bundle_source(h.target(1)).await.unwrap();
    assert_eq!(h.git.calls().iter().filter(|(m, _)| *m == GitMethod::Archive).count(), 1);
    assert_eq!(h.version(0).await.bundle, h.version(1).await.bundle);
    assert_eq!(h.kinds(JobKind::RunsSourceReady).len(), 2, "one per sha");

    let h = harness().await;
    let mut tiny = IndexConfig::new(h.data.path());
    tiny.bundle_max_bytes = 10;
    h.repos.add_version_reason(h.org, h.repo, h.shas[0], VersionReason::Release("v0".into())).await.unwrap();
    let p = h.pipeline_with(h.parser.clone(), tiny);
    p.fetch(h.target(0)).await.unwrap();
    assert!(matches!(p.bundle_source(h.target(0)).await, Err(AppError::Invalid(_))));
    assert!(h.version(0).await.bundle.is_none());
    assert!(std::fs::read_dir(h.data.path().join("tmp")).unwrap().next().is_none(), "staging file removed on failure");
}

#[tokio::test]
async fn reconcile_catches_missed_pushes_rebuilds_and_reschedules() {
    let h = harness().await;
    let p = h.pipeline();
    // The stored head is commit 0, but the branch is at commit 1 (missed webhook).
    h.repos.set_default_head(h.org, h.repo, h.shas[0]).await.unwrap();
    h.enqueue_version(0, VersionReason::DefaultHead).await;
    drive(&h, &p).await;
    let JobDone::Complete(jobs) = p.reconcile().await.unwrap() else { panic!("defer") };
    assert_eq!(h.repos.default_head(h.org, h.repo).await.unwrap(), Some(h.shas[1]));
    assert!(jobs.contains(&mage_core::ports::jobs::NewJob::new(JobPayload::IndexVersion {
        org_id: h.org,
        repo_id: h.repo,
        sha: h.shas[1],
        reason: VersionReason::DefaultHead
    })));
    let next = jobs.iter().find(|j| j.kind == JobKind::IndexReconcile).unwrap();
    assert_eq!(next.run_after, Some(h.clock.now() + super::reconcile::INTERVAL));

    // After a rev bump, versions that still hold a reason are rebuilt (newest first).
    let h = harness().await;
    let p1 = h.pipeline();
    h.enqueue_version(0, VersionReason::Release("v0".into())).await;
    drive(&h, &p1).await;
    let p2 = h.pipeline_with(Arc::new(base_parser().with_revs(ParserRev(2), AppMapRev(1))), IndexConfig::new(h.data.path()));
    let rebuild = mage_core::ports::jobs::NewJob::new(JobPayload::IndexVersion {
        org_id: h.org,
        repo_id: h.repo,
        sha: h.shas[0],
        reason: VersionReason::Release("v0".into()),
    });
    // Files just completed: a rebuild may be in flight, so none is requested yet.
    let JobDone::Complete(jobs) = p2.reconcile().await.unwrap() else { panic!("defer") };
    assert!(!jobs.contains(&rebuild));
    h.clock.advance(std::time::Duration::from_secs(2 * 3600));
    let JobDone::Complete(jobs) = p2.reconcile().await.unwrap() else { panic!("defer") };
    assert!(jobs.contains(&rebuild));
}

#[tokio::test]
async fn reconcile_fails_layers_of_jobs_the_reaper_killed() {
    let h = harness().await;
    let p = h.pipeline();
    let t = h.target(1);
    p.version(h.org, h.repo, t.sha, VersionReason::Release("v2".into())).await.unwrap();
    // A history job whose lease expired with no attempts left: the reaper kills it.
    h.repos.apply_layer_event(h.org, t.key(), Layer::History, mage_core::domain::LayerEvent::Start, &[]).await.unwrap();
    h.jobs.enqueue(t.history_job().with_max_attempts(1)).await.unwrap();
    let request = LeaseRequest { lane: JobLane::Indexer, kinds: Some(vec![JobKind::IndexHistory]), worker: "w".into(), max: 1 };
    assert_eq!(h.jobs.lease(&request).await.unwrap().len(), 1);
    h.clock.advance(std::time::Duration::from_secs(3 * 3600));
    assert_eq!(h.jobs.reap_expired(10).await.unwrap(), 1);
    p.reconcile().await.unwrap();
    assert!(matches!(state(&h.version(1).await, Layer::History), LayerState::Failed { .. }));
}

#[tokio::test]
async fn gc_deletes_expired_versions_their_pins_and_unshared_bundles() {
    let h = harness().await;
    let p = h.pipeline();
    let ticket = TicketId::from_uuid(Uuid::from_u128(5));
    h.enqueue_version(0, VersionReason::OnDemand(ticket)).await;
    drive(&h, &p).await;
    let bundle = h.version(0).await.bundle.unwrap();
    assert_eq!(h.git.pins(&h.mirror()), vec![h.shas[0]]);

    // 91 days later the OnDemand version is past retention.
    h.clock.advance(std::time::Duration::from_secs(91 * 86_400));
    let JobDone::Complete(jobs) = p.gc().await.unwrap() else { panic!("defer") };
    assert!(matches!(h.repos.version(h.org, h.key(0)).await, Err(mage_core::ports::repos::RepoStoreError::NotFound)));
    assert!(h.git.pins(&h.mirror()).is_empty());
    assert!(matches!(h.artifacts.head(&BlobKey::parse(&bundle.key).unwrap()).await, Err(mage_core::ports::BlobError::NotFound)));
    assert!(h.knowledge.manifest_all(h.org, &h.key(0), &PageReq::first(10)).await.unwrap().items.is_empty());
    // Blob contents no manifest references are swept after the grace period.
    assert!(matches!(
        h.artifacts.head(&BlobKey::blob_content(h.org, oid(MAIN_GO))).await,
        Err(mage_core::ports::BlobError::NotFound)
    ));
    assert_eq!(jobs[0].kind, JobKind::IndexGc);
    assert_eq!(jobs[0].run_after, Some(h.clock.now() + super::gc::INTERVAL));
}
