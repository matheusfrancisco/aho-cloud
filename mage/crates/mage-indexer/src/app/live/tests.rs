use std::sync::Arc;

use async_trait::async_trait;
use mage_core::domain::{FileClass, Language};
use mage_core::testing::FakeClock;
use mage_core::wire::indexer::{LogQuery, PatternKind, TextQuery, TreeDiffOptions};

use super::*;
use crate::ports::git::FetchWants;
use crate::ports::search::SearchError;
use crate::testing::MemTextSearch;
use crate::testing::git::{FixtureCommit, FixtureRepo, GitMethod, MemGitRepo, git_blob_oid, test_fetch_spec};
use crate::testing::world::{World, WorldFile, world_org, world_path};

const URL: &str = "https://github.com/acme/app.git";

const FILES: &[(&str, &str, FileClass)] = &[
    ("a.go", "needle a\n", FileClass::Source),
    ("gen/x.pb.go", "needle 1\nneedle 2\nneedle 3\n", FileClass::Generated),
    ("vendor/v.go", "needle v\n", FileClass::Vendored),
    ("z.go", "needle z\nline 2\nline 3\n", FileClass::Source),
];

struct Env {
    w: World,
    git: Arc<MemGitRepo>,
    search: Arc<MemTextSearch>,
    data: tempfile::TempDir,
    shas: Vec<CommitSha>,
    repo: RepoId,
}

impl Env {
    /// Three commits of `FILES` (the second changes z.go, the third adds c.go), fetched, each
    /// indexed with Files Ready.
    async fn new() -> Env {
        let w = World::new();
        let git = Arc::new(MemGitRepo::new());
        let tree: Vec<(&str, &str)> = FILES.iter().map(|(p, c, _)| (*p, *c)).collect();
        let mut tree2 = tree.clone();
        tree2[3].1 = "needle z\nline 2 changed\nline 3\n";
        let mut tree3 = tree2.clone();
        tree3.push(("c.go", "package c\n"));
        let shas = git.add_remote(URL, &FixtureRepo::linear("main", vec![
            FixtureCommit::new("first", FakeClock::DEFAULT_START, &tree),
            FixtureCommit::new("second", FakeClock::DEFAULT_START, &tree2),
            FixtureCommit::new("third", FakeClock::DEFAULT_START, &tree3),
        ]));
        let repo = w.repo("app").await;
        let mirror = MirrorRef::new(world_org(), repo.id);
        git.fetch(&mirror, &test_fetch_spec(URL), &FetchWants { all_heads_and_tags: true, shas: Vec::new() }).await.unwrap_or_else(|e| panic!("{e}"));
        for sha in &shas {
            let key = w.version(repo.id, *sha, &[Layer::Files]).await;
            let files: Vec<WorldFile<'_>> = FILES
                .iter()
                .map(|(path, content, class)| WorldFile { path, blob: git_blob_oid(content.as_bytes()), lang: Some(Language::Go), class: *class, lines: 3 })
                .collect();
            w.manifest(&key, &files).await;
        }
        let data = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        Env { w, git, search: Arc::new(MemTextSearch::new()), data, shas, repo: repo.id }
    }

    fn live(&self, config: LiveConfig) -> LiveService {
        LiveService::new(self.w.repos.clone(), self.w.knowledge.clone(), self.git.clone(), self.search.clone(), &config)
    }

    fn config(&self) -> LiveConfig {
        LiveConfig::new(self.data.path())
    }

    /// Materializes the checkout and registers its files with the text-search fake.
    async fn checkout(&self, live: &LiveService, sha: CommitSha) -> CheckoutLease {
        let lease = live.checkout(world_org(), self.repo, sha).await.unwrap_or_else(|e| panic!("{e}"));
        let files = FILES
            .iter()
            .map(|(path, _, _)| {
                let bytes = std::fs::read(lease.path().join(path)).unwrap_or_else(|e| panic!("{path}: {e}"));
                (world_path(path), bytes)
            })
            .collect();
        self.search.put_checkout(lease.path(), files);
        lease
    }

    fn request(&self, sha: CommitSha, max_hits: u32, include_generated: bool) -> SearchRequest {
        let mut query = TextQuery::new("needle", PatternKind::Literal);
        query.max_hits = max_hits;
        SearchRequest { org_id: world_org(), repo_id: self.repo, sha, query, include_generated }
    }

    fn materializations(&self) -> usize {
        self.git.calls().iter().filter(|(m, _)| *m == GitMethod::Materialize).count()
    }
}

fn positions(hits: &TextHits) -> Vec<(&str, u32)> {
    hits.hits.iter().map(|h| (h.path.as_str(), h.line)).collect()
}

#[tokio::test]
async fn generated_and_vendored_hits_are_dropped_and_the_page_refilled() {
    let e = Env::new().await;
    let live = e.live(e.config());
    let head = e.shas[1];
    drop(e.checkout(&live, head).await);

    let all = live.search(&e.request(head, 200, true)).await.unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(all.hits.len(), 6);
    let page = live.search(&e.request(head, 2, false)).await.unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(positions(&page), [("a.go", 1), ("z.go", 1)], "refilled past the generated and vendored hits");
    assert_eq!((page.next, page.truncated), (None, None));
    let page = live.search(&e.request(head, 1, false)).await.unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(positions(&page), [("a.go", 1)]);
    assert_eq!(page.truncated, Some(Truncation::Limit));
    assert_eq!(page.next, Some((world_path("a.go"), 1)));
    assert_eq!(e.materializations(), 1, "the checkout is reused");
}

#[tokio::test]
async fn checkouts_are_cached_evicted_and_survive_restarts() {
    let e = Env::new().await;
    let mut config = e.config();
    config.checkout_cache_bytes = 1;
    let live = e.live(config.clone());
    let first = live.checkout(world_org(), e.repo, e.shas[0]).await.unwrap_or_else(|err| panic!("{err}"));
    let first_dir = first.path().to_path_buf();
    // Over the cap, but in use: materializing another sha cannot evict it.
    let second = live.checkout(world_org(), e.repo, e.shas[1]).await.unwrap_or_else(|err| panic!("{err}"));
    assert!(first_dir.is_dir());
    drop(first);
    drop(second);
    let again = live.checkout(world_org(), e.repo, e.shas[1]).await.unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(e.materializations(), 2, "a cached checkout is reused");
    drop(again);
    // A new checkout over the cap evicts the idle ones, least recently used first.
    let third = live.checkout(world_org(), e.repo, e.shas[2]).await.unwrap_or_else(|err| panic!("{err}"));
    assert!(!first_dir.exists(), "evicted");
    assert!(third.path().join("c.go").is_file());
    drop(third);
    let back = live.checkout(world_org(), e.repo, e.shas[0]).await.unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(e.materializations(), 4, "the evicted checkout is materialized again");
    drop(back);
    let health = live.health().await;
    assert!(health.ok);
    assert_eq!(health.mirrors, 1);
    assert!(health.checkouts_bytes > 0);

    // A new process registers the checkouts on disk instead of materializing again.
    config.checkout_cache_bytes = 1 << 30;
    let restarted = e.live(config);
    assert_eq!(restarted.health().await.checkouts_bytes, health.checkouts_bytes);
    let lease = restarted.checkout(world_org(), e.repo, e.shas[0]).await.unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(e.materializations(), 4);
    drop(lease);
}

#[tokio::test]
async fn concurrent_searches_materialize_once() {
    let e = Env::new().await;
    let live = Arc::new(e.live(e.config()));
    let head = e.shas[1];
    let (a, b) = tokio::join!(live.checkout(world_org(), e.repo, head), live.checkout(world_org(), e.repo, head));
    let (a, b) = (a.unwrap_or_else(|err| panic!("{err}")), b.unwrap_or_else(|err| panic!("{err}")));
    assert_eq!(a.path(), b.path());
    assert_eq!(e.materializations(), 1);
}

/// Holds every search until released.
struct Gate {
    inner: MemTextSearch,
    open: tokio::sync::Semaphore,
}

#[async_trait]
impl TextSearch for Gate {
    async fn search(&self, checkout: &Checkout, q: &TextQuery) -> Result<TextHits, SearchError> {
        let _pass = self.open.acquire().await.map_err(|e| SearchError::Unavailable(e.to_string()))?;
        self.inner.search(checkout, q).await
    }
}

#[tokio::test]
async fn excess_searches_are_unavailable() {
    let e = Env::new().await;
    let gate = Arc::new(Gate { inner: MemTextSearch::new(), open: tokio::sync::Semaphore::new(0) });
    let mut config = e.config();
    config.max_searches = 1;
    let live = Arc::new(LiveService::new(e.w.repos.clone(), e.w.knowledge.clone(), e.git.clone(), gate.clone(), &config));
    let head = e.shas[1];
    let lease = live.checkout(world_org(), e.repo, head).await.unwrap_or_else(|err| panic!("{err}"));
    gate.inner.put_checkout(lease.path(), vec![(world_path("a.go"), b"needle\n".to_vec())]);
    let request = e.request(head, 10, true);
    let held = {
        let (live, request) = (live.clone(), request.clone());
        tokio::spawn(async move { live.search(&request).await })
    };
    while live.searches.available_permits() > 0 {
        tokio::task::yield_now().await;
    }
    assert!(matches!(live.search(&request).await, Err(AppError::Unavailable(_))));
    gate.open.add_permits(1);
    let hits = held.await.unwrap_or_else(|err| panic!("{err}")).unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(hits.hits.len(), 1);
}

#[tokio::test]
async fn version_and_input_checks() {
    let e = Env::new().await;
    let live = e.live(e.config());
    let unknown = CommitSha::from_bytes([9; 20]);
    assert!(matches!(live.search(&e.request(unknown, 10, false)).await, Err(AppError::NotFound(_))));
    let not_ready = CommitSha::from_bytes([8; 20]);
    e.w.version(e.repo, not_ready, &[]).await;
    assert!(matches!(live.search(&e.request(not_ready, 10, false)).await, Err(AppError::Unavailable(_))));
    assert!(matches!(live.search(&e.request(e.shas[1], 0, false)).await, Err(AppError::Invalid(_))));

    let blame = |sha, start, end| BlameRequest { org_id: world_org(), repo_id: e.repo, sha, path: world_path("z.go"), start_line: start, end_line: end };
    let hunks = live.blame(&blame(e.shas[1], 1, 3)).await.unwrap_or_else(|err| panic!("{err}")).hunks;
    let owners: Vec<(u32, u32, CommitSha)> = hunks.iter().map(|h| (h.start, h.lines, h.commit)).collect();
    assert_eq!(owners, [(1, 1, e.shas[0]), (2, 1, e.shas[1]), (3, 1, e.shas[0])]);
    assert!(matches!(live.blame(&blame(e.shas[1], 1, 201)).await, Err(AppError::Invalid(_))));
    assert!(matches!(live.blame(&blame(unknown, 1, 2)).await, Err(AppError::NotFound(_))));

    let log = |include: Vec<CommitSha>, max| LogRequest {
        org_id: world_org(),
        repo_id: e.repo,
        query: LogQuery { include, exclude: Vec::new(), path_prefix: None, since: None, skip: 0, max, with_files: false },
    };
    let commits = live.log(&log(vec![e.shas[1]], 10)).await.unwrap_or_else(|err| panic!("{err}")).commits;
    assert_eq!(commits.iter().map(|c| c.sha).collect::<Vec<_>>(), [e.shas[1], e.shas[0]]);
    assert!(matches!(live.log(&log(vec![e.shas[1]], 201)).await, Err(AppError::Invalid(_))));
    assert!(matches!(live.log(&log(vec![e.shas[1]], 0)).await, Err(AppError::Invalid(_))));
    assert!(matches!(live.log(&log(vec![unknown], 10)).await, Err(AppError::NotFound(_))));

    let opts = |paths: usize| TreeDiffOptions {
        paths: (0..paths).map(|i| world_path(&format!("f{i}.go"))).collect(),
        hunks: true,
        context: 3,
        max_bytes_per_file: 8 * 1024,
        renames: false,
    };
    let diff = |from, to, paths| DiffRequest { org_id: world_org(), repo_id: e.repo, from, to, opts: opts(paths) };
    let files = live.diff(&diff(e.shas[0], e.shas[1], 0)).await.unwrap_or_else(|err| panic!("{err}")).files;
    assert_eq!(files.iter().map(|f| f.change.path.as_str()).collect::<Vec<_>>(), ["z.go"]);
    assert!(matches!(live.diff(&diff(e.shas[0], e.shas[1], 21)).await, Err(AppError::Invalid(_))));
    assert!(matches!(live.diff(&diff(e.shas[0], unknown, 0)).await, Err(AppError::NotFound(_))));
}
