//! `SourceHost` contract. A [`SourceHostWorld`] describes installations, repos, refs and pull
//! requests; `make` builds a host serving exactly that world (the fake loads it, the GitHub
//! adapter test serves it from recorded/mocked HTTP), and the suite checks the port's semantics.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use time::OffsetDateTime;

use crate::domain::ids::{CommitSha, InstallationId};
use crate::ports::clock::Clock;
use crate::ports::source_host::{
    Access, AccountKind, FETCH_USERNAME, InstallationInfo, MIN_TOKEN_VALIDITY, PrListQuery, PrSort, PrState, PullRequestInfo,
    RemoteRepo, RepoCoords, RepoSelection, SortDirection, SourceHost, SourceHostError, TokenScope,
};
use crate::testing::clock::FakeClock;

/// Everything a source host knows, as data.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SourceHostWorld {
    pub installations: Vec<WorldInstallation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorldInstallation {
    pub info: InstallationInfo,
    pub repos: Vec<WorldRepo>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorldRepo {
    pub remote: RemoteRepo,
    /// Branch name → head.
    pub branches: BTreeMap<String, CommitSha>,
    /// Tag name → commit (annotated tags already peeled).
    pub tags: BTreeMap<String, CommitSha>,
    /// Other known commits (resolvable by full sha or unique prefix).
    pub commits: Vec<CommitSha>,
    pub pull_requests: Vec<WorldPullRequest>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorldPullRequest {
    pub info: PullRequestInfo,
    pub open: bool,
}

impl SourceHostWorld {
    pub fn installation(&self, inst: InstallationId) -> Option<&WorldInstallation> {
        self.installations.iter().find(|i| i.info.id == inst)
    }
}

impl WorldInstallation {
    /// Case-insensitive owner/name lookup (GitHub names are case-insensitive).
    pub fn repo(&self, coords: &RepoCoords) -> Option<&WorldRepo> {
        self.repos.iter().find(|r| r.remote.coords.owner.eq_ignore_ascii_case(&coords.owner) && r.remote.coords.name.eq_ignore_ascii_case(&coords.name))
    }
}

impl WorldRepo {
    /// Every commit the repo knows: branch heads, tags and `commits`.
    pub fn known_commits(&self) -> BTreeSet<CommitSha> {
        self.branches.values().chain(self.tags.values()).chain(&self.commits).copied().collect()
    }
}

/// Runs every `SourceHost` scenario against a host built by `make` for the suite's world.
pub async fn contract_source_host<F, Fut>(make: F)
where
    F: Fn(SourceHostWorld, Arc<FakeClock>) -> Fut,
    Fut: Future<Output = Arc<dyn SourceHost>>,
{
    macro_rules! scenario {
        ($name:ident) => {{
            let clock = Arc::new(FakeClock::new());
            let world = world(clock.now());
            let host = make(world.clone(), clock.clone()).await;
            $name(&clock, &world, &*host).await;
        }};
    }
    scenario!(installations_and_repos);
    scenario!(resolve_ref_branches_tags_and_shas);
    scenario!(tokens_are_scoped_and_fresh);
    scenario!(fetch_spec_uses_a_repo_scoped_token);
    scenario!(pull_requests_filter_sort_and_page);
}

const ACTIVE: InstallationId = InstallationId(1001);
const SUSPENDED: InstallationId = InstallationId(1002);

fn sha(n: u8) -> CommitSha {
    CommitSha::from_bytes([n; 20])
}

fn coords(owner: &str, name: &str) -> RepoCoords {
    RepoCoords { owner: owner.into(), name: name.into() }
}

fn pr(number: u32, updated_at: OffsetDateTime, merged: bool, open: bool) -> WorldPullRequest {
    WorldPullRequest {
        info: PullRequestInfo {
            number,
            title: format!("PR {number}"),
            body_head: format!("Fixes ENG-{number}"),
            head_ref: format!("eng-{number}-fix"),
            base_ref: "main".into(),
            merge_commit_sha: merged.then(|| sha(100 + number as u8)),
            merged_at: merged.then_some(updated_at),
            updated_at,
            url: format!("https://github.com/acme/web/pull/{number}"),
            author_login: "dev".into(),
        },
        open,
    }
}

/// Two installations (one suspended); `acme/web` with branches, a tag, an extra commit whose
/// prefix is unique, two commits sharing a prefix, and six pull requests.
fn world(now: OffsetDateTime) -> SourceHostWorld {
    let permissions = BTreeMap::from([
        ("contents".to_owned(), Access::Read),
        ("metadata".to_owned(), Access::Read),
        ("pull_requests".to_owned(), Access::Read),
    ]);
    let mut twin_a = [0x5a; 20];
    twin_a[19] = 1;
    let mut twin_b = [0x5a; 20];
    twin_b[19] = 2;
    let at = |mins: u64| now - Duration::from_secs(60 * mins);
    let web = WorldRepo {
        remote: RemoteRepo { external_id: 501, coords: coords("Acme", "Web"), default_branch: "main".into(), private: true, archived: false },
        branches: BTreeMap::from([("main".to_owned(), sha(1)), ("feature/x".to_owned(), sha(2))]),
        tags: BTreeMap::from([("v1.0.0".to_owned(), sha(3))]),
        commits: vec![CommitSha::from_bytes([0xc4; 20]), CommitSha::from_bytes(twin_a), CommitSha::from_bytes(twin_b)],
        pull_requests: vec![
            pr(1, at(600), true, false),
            pr(2, at(500), false, false),
            pr(3, at(400), true, false),
            pr(4, at(300), false, true),
            pr(5, at(200), true, false),
            pr(6, at(100), true, false),
        ],
    };
    let api = WorldRepo {
        remote: RemoteRepo { external_id: 502, coords: coords("Acme", "api"), default_branch: "trunk".into(), private: true, archived: true },
        branches: BTreeMap::from([("trunk".to_owned(), sha(9))]),
        tags: BTreeMap::new(),
        commits: vec![],
        pull_requests: vec![],
    };
    let installation = |id, suspended, repos| WorldInstallation {
        info: InstallationInfo {
            id,
            account_login: "acme".into(),
            account_kind: AccountKind::Organization,
            permissions: permissions.clone(),
            repository_selection: RepoSelection::Selected,
            suspended,
        },
        repos,
    };
    SourceHostWorld { installations: vec![installation(ACTIVE, false, vec![web, api]), installation(SUSPENDED, true, vec![])] }
}

async fn installations_and_repos(_: &FakeClock, world: &SourceHostWorld, host: &dyn SourceHost) {
    let active = world.installation(ACTIVE).expect("world installation");
    assert_eq!(host.installation(ACTIVE).await, Ok(active.info.clone()));
    assert_eq!(host.installation(SUSPENDED).await.map(|i| i.suspended), Ok(true), "a suspended installation is still described");
    assert_eq!(host.installation(InstallationId(9)).await, Err(SourceHostError::NotFound));

    let mut repos = host.installation_repos(ACTIVE).await.unwrap();
    repos.sort_by_key(|r| r.external_id);
    assert_eq!(repos, active.repos.iter().map(|r| r.remote.clone()).collect::<Vec<_>>());
    assert_eq!(host.installation_repos(InstallationId(9)).await, Err(SourceHostError::NotFound));
    assert_eq!(host.installation_repos(SUSPENDED).await, Err(SourceHostError::Unauthorized), "suspended installations mint no tokens");

    let web = host.repo(ACTIVE, &coords("acme", "WEB")).await.unwrap();
    assert_eq!((web.external_id, web.coords.clone()), (501, coords("Acme", "Web")), "lookup is case-insensitive, coords canonical");
    assert_eq!(host.repo(ACTIVE, &coords("acme", "missing")).await, Err(SourceHostError::NotFound));
    assert_eq!(host.repo(InstallationId(9), &coords("acme", "web")).await, Err(SourceHostError::NotFound));
}

async fn resolve_ref_branches_tags_and_shas(_: &FakeClock, _: &SourceHostWorld, host: &dyn SourceHost) {
    let web = coords("acme", "web");
    let resolve = |r: &str| {
        let (web, r) = (web.clone(), r.to_owned());
        async move { host.resolve_ref(ACTIVE, &web, &r).await }
    };
    assert_eq!(resolve("main").await, Ok(sha(1)));
    assert_eq!(resolve("feature/x").await, Ok(sha(2)));
    assert_eq!(resolve("refs/heads/main").await, Ok(sha(1)));
    assert_eq!(resolve("v1.0.0").await, Ok(sha(3)), "tags resolve to the peeled commit");
    assert_eq!(resolve("refs/tags/v1.0.0").await, Ok(sha(3)));
    let full = CommitSha::from_bytes([0xc4; 20]);
    assert_eq!(host.resolve_ref(ACTIVE, &web, &full.to_hex()).await, Ok(full));
    assert_eq!(resolve("c4c4c4c").await, Ok(full), "a unique sha prefix");
    assert_eq!(resolve("0101010").await, Ok(sha(1)), "branch heads resolve by prefix too");
    assert!(matches!(resolve("5a5a5a5a").await, Err(SourceHostError::Invalid(_))), "an ambiguous prefix");
    assert_eq!(resolve("no-such-branch").await, Err(SourceHostError::NotFound));
    assert_eq!(resolve(&"e".repeat(40)).await, Err(SourceHostError::NotFound));
    assert_eq!(host.resolve_ref(ACTIVE, &coords("acme", "missing"), "main").await, Err(SourceHostError::NotFound));
}

async fn tokens_are_scoped_and_fresh(clock: &FakeClock, _: &SourceHostWorld, host: &dyn SourceHost) {
    let full = host.installation_token(ACTIVE, &TokenScope::default()).await.unwrap();
    assert!(full.expires_at >= clock.now() + MIN_TOKEN_VALIDITY, "a token has at least 5 minutes left");
    assert_eq!(full.repositories, None);
    assert_eq!(full.permissions.get("contents"), Some(&Access::Read), "empty permissions = the installation's set");
    assert!(!full.token.expose().is_empty());
    assert_eq!(format!("{:?}", full.token), "***");

    let scoped = host.installation_token(ACTIVE, &TokenScope::fetch("web")).await.unwrap();
    assert_eq!(scoped.repositories, Some(vec!["web".to_owned()]));
    assert_eq!(
        scoped.permissions,
        BTreeMap::from([("contents".to_owned(), Access::Read), ("pull_requests".to_owned(), Access::Read)])
    );

    let write = TokenScope { repositories: None, permissions: BTreeMap::from([("contents".to_owned(), Access::Write)]) };
    assert!(matches!(host.installation_token(ACTIVE, &write).await, Err(SourceHostError::Invalid(_))), "more than granted");
    let foreign = TokenScope { repositories: Some(vec!["other".into()]), permissions: BTreeMap::new() };
    assert!(matches!(host.installation_token(ACTIVE, &foreign).await, Err(SourceHostError::Invalid(_))), "repo outside the installation");
    assert_eq!(host.installation_token(SUSPENDED, &TokenScope::default()).await.map(|_| ()), Err(SourceHostError::Unauthorized));
    assert_eq!(host.installation_token(InstallationId(9), &TokenScope::default()).await.map(|_| ()), Err(SourceHostError::NotFound));
}

async fn fetch_spec_uses_a_repo_scoped_token(clock: &FakeClock, _: &SourceHostWorld, host: &dyn SourceHost) {
    let spec = host.fetch_spec(ACTIVE, &coords("acme", "web")).await.unwrap();
    assert_eq!(spec.url.as_str(), "https://github.com/Acme/Web.git");
    assert_eq!(spec.username, FETCH_USERNAME);
    assert_eq!(spec.token.repositories, Some(vec!["Web".to_owned()]), "scoped to the one repo");
    assert_eq!(spec.token.permissions.get("contents"), Some(&Access::Read));
    assert_eq!(spec.token.permissions.get("pull_requests"), Some(&Access::Read));
    assert!(spec.token.expires_at >= clock.now() + MIN_TOKEN_VALIDITY);
    assert_eq!(host.fetch_spec(ACTIVE, &coords("acme", "missing")).await.map(|_| ()), Err(SourceHostError::NotFound));
}

async fn pull_requests_filter_sort_and_page(clock: &FakeClock, _: &SourceHostWorld, host: &dyn SourceHost) {
    let web = coords("acme", "web");
    let query = |state, sort, direction, updated_since, per_page| PrListQuery { state, sort, direction, updated_since, page_token: None, per_page };
    let numbers = |items: &[PullRequestInfo]| items.iter().map(|p| p.number).collect::<Vec<_>>();

    let all = host.list_pull_requests(ACTIVE, &web, &query(PrState::All, PrSort::Created, SortDirection::Asc, None, 100)).await.unwrap();
    assert_eq!((numbers(&all.items), all.next), (vec![1, 2, 3, 4, 5, 6], None));
    let open = host.list_pull_requests(ACTIVE, &web, &query(PrState::Open, PrSort::Created, SortDirection::Desc, None, 100)).await.unwrap();
    assert_eq!(numbers(&open.items), vec![4]);

    let since = clock.now() - Duration::from_secs(60 * 400);
    let mut q = query(PrState::Closed, PrSort::Updated, SortDirection::Asc, Some(since), 2);
    let mut paged = Vec::new();
    loop {
        let page = host.list_pull_requests(ACTIVE, &web, &q).await.unwrap();
        assert!(page.items.len() <= 2);
        paged.extend(numbers(&page.items));
        match page.next {
            Some(token) => q.page_token = Some(token),
            None => break,
        }
    }
    assert_eq!(paged, vec![3, 5, 6], "closed, updated_at >= since (inclusive), oldest update first, across pages");
    let merged = all.items.iter().find(|p| p.number == 3).expect("pr 3");
    assert_eq!(merged.merge_commit_sha, Some(sha(103)));

    let bad = query(PrState::All, PrSort::Created, SortDirection::Asc, None, 0);
    assert!(matches!(host.list_pull_requests(ACTIVE, &web, &bad).await, Err(SourceHostError::Invalid(_))));
    let too_big = query(PrState::All, PrSort::Created, SortDirection::Asc, None, 101);
    assert!(matches!(host.list_pull_requests(ACTIVE, &web, &too_big).await, Err(SourceHostError::Invalid(_))));
    let missing = host.list_pull_requests(ACTIVE, &coords("acme", "missing"), &query(PrState::All, PrSort::Created, SortDirection::Asc, None, 10)).await;
    assert_eq!(missing.map(|_| ()), Err(SourceHostError::NotFound));
}
