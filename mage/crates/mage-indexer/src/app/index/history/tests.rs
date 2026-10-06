use std::collections::BTreeMap;
use std::sync::Arc;

use mage_core::domain::{CommitSha, IndexPriority, InstallationId, Layer, LayerState, RepoVersionKey, TreeOid};
use mage_core::ports::knowledge::{LinkLookup, TicketLink};
use mage_core::ports::source_host::{AccountKind, InstallationInfo, RemoteRepo, RepoSelection};
use mage_core::testing::{FakeClock, FakeSourceHost, SourceHostWorld, WorldInstallation, WorldPullRequest, WorldRepo};
use time::{Duration, OffsetDateTime};

use super::*;
use crate::ports::git::{FetchWants, MirrorRef};
use crate::testing::git::{FixtureCommit, FixtureRepo, MemGitRepo, test_fetch_spec};
use crate::testing::world::{World, world_org};

const URL: &str = "https://github.com/acme/app.git";

fn days_ago(n: i64) -> OffsetDateTime {
    FakeClock::DEFAULT_START - Duration::days(n)
}

fn info(subject: &str, body: &str) -> CommitInfo {
    CommitInfo {
        sha: CommitSha::from_bytes([1; 20]),
        tree: TreeOid::from_bytes([2; 20]),
        parents: Vec::new(),
        author_name: "Ada".into(),
        author_time: FakeClock::DEFAULT_START,
        committer_time: FakeClock::DEFAULT_START,
        subject: subject.into(),
        body: body.into(),
        files: None,
    }
}

fn ids(links: &[TicketLink]) -> Vec<(&str, LinkSource, Option<u32>)> {
    links.iter().map(|l| (l.identifier.as_str(), l.source, l.pr)).collect()
}

#[test]
fn merge_and_squash_subjects_name_the_pr() {
    assert_eq!(merge_subject("Merge pull request #12 from acme/eng-3-fix"), Some((12, "acme/eng-3-fix")));
    assert_eq!(merge_subject("Merge pull request #12"), None);
    assert_eq!(merge_subject("Merge branch 'main'"), None);
    assert_eq!(squash_number("Fix login redirect (#456)"), Some(456));
    assert_eq!(squash_number("Fix login redirect (#456) "), Some(456));
    assert_eq!(squash_number("Fix (#x)"), None);
    assert_eq!(squash_number("No number"), None);
}

#[test]
fn linear_ids_in_commits() {
    let links = commit_links(&info("ENG-1: fix login (#45)", "Refs OPS-2 and ENG-1.\nSee UTF-8 handling"));
    assert_eq!(ids(&links), [
        ("ENG-1", LinkSource::CommitSubject, Some(45)),
        ("OPS-2", LinkSource::CommitBody, Some(45)),
        ("ENG-1", LinkSource::CommitBody, Some(45)),
        ("UTF-8", LinkSource::CommitBody, Some(45)),
    ]);
    assert!(links.iter().all(|l| l.commit == Some(CommitSha::from_bytes([1; 20]))));
    let merge = commit_links(&info("Merge pull request #12 from acme/eng-3-fix-redirect", "ENG-3 Fix redirect"));
    assert_eq!(ids(&merge), [("ENG-3", LinkSource::CommitBody, Some(12)), ("ENG-3", LinkSource::MergeBranch, Some(12))]);
    assert!(commit_links(&info("chore: bump deps", "")).is_empty());
    assert!(commit_links(&info("eng-9 lowercase prose is not an id", "")).is_empty());
}

fn pr(number: u32, title: &str, head: &str, body: &str, merge: Option<CommitSha>, updated: OffsetDateTime) -> PullRequestInfo {
    PullRequestInfo {
        number,
        title: title.into(),
        body_head: body.into(),
        head_ref: head.into(),
        base_ref: "main".into(),
        merge_commit_sha: merge,
        merged_at: merge.map(|_| updated),
        updated_at: updated,
        url: format!("https://github.com/acme/app/pull/{number}"),
        author_login: "ada".into(),
    }
}

#[test]
fn linear_ids_in_pull_requests() {
    let merge = CommitSha::from_bytes([3; 20]);
    let links = pr_links(&pr(12, "ENG-3 Fix redirect", "chico/eng-3-fix-redirect", "Also fixes ENG-4", Some(merge), days_ago(1)));
    assert_eq!(ids(&links), [
        ("ENG-3", LinkSource::PrTitle, Some(12)),
        ("ENG-3", LinkSource::PrBranch, Some(12)),
        ("ENG-4", LinkSource::PrBody, Some(12))
    ]);
    assert!(links.iter().all(|l| l.commit == Some(merge)));
}

struct Env {
    w: World,
    git: Arc<MemGitRepo>,
    host: Arc<FakeSourceHost>,
    uc: IndexHistory,
}

fn host_world(prs: Vec<WorldPullRequest>) -> SourceHostWorld {
    SourceHostWorld {
        installations: vec![WorldInstallation {
            info: InstallationInfo {
                id: InstallationId(7),
                account_login: "acme".into(),
                account_kind: AccountKind::Organization,
                permissions: BTreeMap::new(),
                repository_selection: RepoSelection::All,
                suspended: false,
            },
            repos: vec![WorldRepo {
                remote: RemoteRepo {
                    external_id: 1,
                    coords: RepoCoords { owner: "acme".into(), name: "app".into() },
                    default_branch: "main".into(),
                    private: true,
                    archived: false,
                },
                branches: BTreeMap::new(),
                tags: BTreeMap::new(),
                commits: Vec::new(),
                pull_requests: prs,
            }],
        }],
    }
}

fn commits() -> Vec<FixtureCommit> {
    vec![
        FixtureCommit::new("Initial commit", days_ago(30), &[("a.txt", "a")]),
        FixtureCommit::new("ENG-1: add login", days_ago(20), &[("a.txt", "b")]).with_body("Refs OPS-2"),
        FixtureCommit::new("Merge pull request #12 from acme/eng-3-fix-redirect", days_ago(10), &[("a.txt", "c")]),
        FixtureCommit::new("Fix flaky test (#13)", days_ago(5), &[("a.txt", "d")]),
    ]
}

impl Env {
    async fn new(prs: Vec<WorldPullRequest>) -> Env {
        let w = World::new();
        let git = Arc::new(MemGitRepo::new());
        let host = Arc::new(FakeSourceHost::new(w.clock.clone(), host_world(prs)));
        let uc = IndexHistory::new(w.repos.clone(), w.knowledge.clone(), git.clone(), host.clone(), w.clock.clone());
        Env { w, git, host, uc }
    }

    /// Pushes `commits` and indexes a version at the head.
    async fn push(&self, commits: Vec<FixtureCommit>) -> (Vec<CommitSha>, RepoVersionKey) {
        let repo = self.w.repo("app").await;
        let shas = self.git.add_remote(URL, &FixtureRepo::linear("main", commits));
        let mirror = MirrorRef::new(world_org(), repo.id);
        self.git.fetch(&mirror, &test_fetch_spec(URL), &FetchWants { all_heads_and_tags: true, shas: Vec::new() }).await.unwrap_or_else(|e| panic!("{e}"));
        let head = *shas.last().unwrap_or_else(|| panic!("no commits"));
        let key = self.w.version(repo.id, head, &[]).await;
        (shas, key)
    }

    async fn run(&self, key: RepoVersionKey) {
        let done = self.uc.run(world_org(), key.repo_id, key.sha, IndexPriority::Normal).await.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(done, JobDone::done());
    }

    async fn links(&self, key: RepoVersionKey, id: &str) -> Vec<TicketLink> {
        self.w.knowledge.ticket_links(world_org(), key.repo_id, &LinkLookup::Identifier(id.into())).await.unwrap_or_else(|e| panic!("{e}"))
    }
}

#[tokio::test]
async fn walks_commits_and_closed_prs_then_continues_from_the_cursor() {
    let e = Env::new(Vec::new()).await;
    let (shas, key) = e.push(commits()).await;
    e.host.update_world(|world| {
        world.installations[0].repos[0].pull_requests = vec![
            WorldPullRequest { info: pr(12, "ENG-3 Fix redirect", "eng-3-fix-redirect", "Also ENG-4", Some(shas[2]), days_ago(9)), open: false },
            WorldPullRequest { info: pr(13, "ENG-6 draft", "eng-6", "", None, days_ago(2)), open: true },
            WorldPullRequest { info: pr(14, "Bump deps", "deps", "", None, days_ago(1)), open: false },
        ];
    });
    e.run(key).await;

    let version = e.w.repos.version(world_org(), key).await.unwrap_or_else(|err| panic!("{err}"));
    assert!(version.layer(Layer::History).is_some_and(LayerState::is_ready));
    let stored = e.w.knowledge.commits(world_org(), key.repo_id, &shas).await.unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(stored.iter().map(|c| c.info.sha).collect::<Vec<_>>(), shas, "every commit of the backfill");
    assert_eq!(ids(&e.links(key, "ENG-1").await), [("ENG-1", LinkSource::CommitSubject, None)]);
    assert_eq!(ids(&e.links(key, "OPS-2").await), [("OPS-2", LinkSource::CommitBody, None)]);
    assert_eq!(ids(&e.links(key, "ENG-3").await), [
        ("ENG-3", LinkSource::MergeBranch, Some(12)),
        ("ENG-3", LinkSource::PrTitle, Some(12)),
        ("ENG-3", LinkSource::PrBranch, Some(12)),
    ]);
    assert_eq!(ids(&e.links(key, "ENG-4").await), [("ENG-4", LinkSource::PrBody, Some(12))]);
    assert!(e.links(key, "ENG-6").await.is_empty(), "open PRs are not read");
    let prs = e.w.knowledge.pull_requests_for_commits(world_org(), key.repo_id, &[shas[2]]).await.unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(prs.iter().map(|(_, pr)| pr.number).collect::<Vec<_>>(), [12]);
    let cursor = e.w.knowledge.history_cursor(world_org(), key.repo_id).await.unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(cursor.walked_heads, [key.sha]);
    assert_eq!(cursor.prs_updated_after, Some(days_ago(1)));

    // A later push: one new commit; the cursor keeps both heads.
    let mut more = commits();
    more.push(FixtureCommit::new("ENG-5 new feature", days_ago(1), &[("a.txt", "e")]));
    let (shas2, key2) = e.push(more).await;
    assert_eq!(shas2[..4], shas[..], "same history");
    e.run(key2).await;
    assert_eq!(ids(&e.links(key2, "ENG-5").await), [("ENG-5", LinkSource::CommitSubject, None)]);
    let cursor = e.w.knowledge.history_cursor(world_org(), key.repo_id).await.unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(cursor.walked_heads, [key.sha, key2.sha]);
    let pr_calls = e.host.calls().iter().filter(|c| c.method == mage_core::testing::SourceHostMethod::ListPullRequests).count();
    assert_eq!(pr_calls, 2, "one page per run");
}

#[tokio::test]
async fn rate_limited_pull_requests_retry_later() {
    let e = Env::new(Vec::new()).await;
    let (_, key) = e.push(commits()).await;
    let retry_at = FakeClock::DEFAULT_START + Duration::minutes(10);
    e.host.fail_next(mage_core::testing::SourceHostMethod::ListPullRequests, mage_core::ports::SourceHostError::RateLimited { retry_at });
    let err = e.uc.run(world_org(), key.repo_id, key.sha, IndexPriority::Normal).await;
    assert!(matches!(err, Err(AppError::RateLimited { retry_at: at }) if at == retry_at), "{err:?}");
    let cursor = e.w.knowledge.history_cursor(world_org(), key.repo_id).await.unwrap_or_else(|err| panic!("{err}"));
    assert!(cursor.walked_heads.is_empty(), "the cursor moves only after a complete run");
    // The retry completes.
    e.run(key).await;
}
