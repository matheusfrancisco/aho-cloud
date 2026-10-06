//! `FakeSourceHost`: a scripted `SourceHost` serving a [`SourceHostWorld`]. Tests change the
//! world, queue failures per method, and inspect the call log.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;
use url::Url;

use crate::domain::ids::{CommitSha, InstallationId};
use crate::ports::clock::Clock;
use crate::ports::source_host::{
    AccessToken, FETCH_USERNAME, FetchSpec, InstallationInfo, InstallationToken, MAX_PR_PAGE, PrListQuery, PrPage, PrSort, PrState,
    RemoteRepo, RepoCoords, SortDirection, SourceHost, SourceHostError, TokenScope,
};
use crate::testing::contract::{SourceHostWorld, WorldInstallation, WorldRepo};

/// Installation tokens live one hour, like GitHub's.
const TOKEN_TTL: Duration = Duration::from_secs(60 * 60);

/// Shortest sha prefix `resolve_ref` accepts (git's minimum abbreviation).
const MIN_SHA_PREFIX: usize = 4;

/// A `SourceHost` method, for scripted failures and the call log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SourceHostMethod {
    Installation,
    InstallationRepos,
    Repo,
    ResolveRef,
    InstallationToken,
    FetchSpec,
    ListPullRequests,
}

/// One recorded call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceHostCall {
    pub method: SourceHostMethod,
    pub inst: InstallationId,
    pub repo: Option<RepoCoords>,
    /// `resolve_ref`'s ref.
    pub git_ref: Option<String>,
}

pub struct FakeSourceHost {
    clock: Arc<dyn Clock>,
    state: Mutex<State>,
}

struct State {
    world: SourceHostWorld,
    failures: BTreeMap<SourceHostMethod, VecDeque<SourceHostError>>,
    calls: Vec<SourceHostCall>,
    tokens_issued: u64,
}

impl FakeSourceHost {
    pub fn new(clock: Arc<dyn Clock>, world: SourceHostWorld) -> Self {
        Self { clock, state: Mutex::new(State { world, failures: BTreeMap::new(), calls: Vec::new(), tokens_issued: 0 }) }
    }

    /// Change the world (new pushes, renames, suspensions) between calls.
    pub fn update_world(&self, change: impl FnOnce(&mut SourceHostWorld)) {
        change(&mut self.state.lock().world);
    }

    /// The next call of `method` fails with `error` (queued per method, first in first out).
    pub fn fail_next(&self, method: SourceHostMethod, error: SourceHostError) {
        self.state.lock().failures.entry(method).or_default().push_back(error);
    }

    /// Every call so far, in order.
    pub fn calls(&self) -> Vec<SourceHostCall> {
        self.state.lock().calls.clone()
    }

    /// Records the call and pops a scripted failure for it.
    fn enter(&self, method: SourceHostMethod, inst: InstallationId, repo: Option<&RepoCoords>, git_ref: Option<&str>) -> Result<(), SourceHostError> {
        let mut state = self.state.lock();
        state.calls.push(SourceHostCall { method, inst, repo: repo.cloned(), git_ref: git_ref.map(str::to_owned) });
        match state.failures.get_mut(&method).and_then(VecDeque::pop_front) {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn token(&self, state: &mut State, inst: InstallationId, scope: &TokenScope) -> Result<InstallationToken, SourceHostError> {
        let installation = usable(&state.world, inst)?;
        if let Some(names) = &scope.repositories {
            let unknown = names.iter().find(|name| !installation.repos.iter().any(|r| r.remote.coords.name.eq_ignore_ascii_case(name)));
            if let Some(name) = unknown {
                return Err(SourceHostError::Invalid(format!("repository {name} is not granted to installation {inst}")));
            }
        }
        for (permission, access) in &scope.permissions {
            let granted = installation.info.permissions.get(permission);
            if granted.is_none_or(|granted| granted < access) {
                return Err(SourceHostError::Invalid(format!("permission {permission}: {access:?} exceeds the installation's grant")));
            }
        }
        let permissions = if scope.permissions.is_empty() { installation.info.permissions.clone() } else { scope.permissions.clone() };
        state.tokens_issued += 1;
        Ok(InstallationToken {
            token: AccessToken::new(format!("ghs_fake_{inst}_{}", state.tokens_issued)),
            expires_at: self.clock.now() + TOKEN_TTL,
            permissions,
            repositories: scope.repositories.clone(),
        })
    }
}

fn installation(world: &SourceHostWorld, inst: InstallationId) -> Result<&WorldInstallation, SourceHostError> {
    world.installation(inst).ok_or(SourceHostError::NotFound)
}

/// An installation that can mint tokens (not suspended).
fn usable(world: &SourceHostWorld, inst: InstallationId) -> Result<&WorldInstallation, SourceHostError> {
    let installation = installation(world, inst)?;
    if installation.info.suspended { Err(SourceHostError::Unauthorized) } else { Ok(installation) }
}

fn repo<'a>(world: &'a SourceHostWorld, inst: InstallationId, coords: &RepoCoords) -> Result<&'a WorldRepo, SourceHostError> {
    usable(world, inst)?.repo(coords).ok_or(SourceHostError::NotFound)
}

fn resolve(repo: &WorldRepo, git_ref: &str) -> Result<CommitSha, SourceHostError> {
    let branch = git_ref
        .strip_prefix("refs/heads/")
        .or_else(|| git_ref.strip_prefix("heads/"))
        .unwrap_or(git_ref);
    if let Some(sha) = repo.branches.get(branch) {
        return Ok(*sha);
    }
    let tag = git_ref
        .strip_prefix("refs/tags/")
        .or_else(|| git_ref.strip_prefix("tags/"))
        .unwrap_or(git_ref);
    if let Some(sha) = repo.tags.get(tag) {
        return Ok(*sha);
    }
    let hex = git_ref.to_ascii_lowercase();
    if hex.len() < MIN_SHA_PREFIX || hex.len() > 40 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(SourceHostError::NotFound);
    }
    let matches: Vec<CommitSha> = repo.known_commits().into_iter().filter(|sha| sha.to_hex().starts_with(&hex)).collect();
    match matches.as_slice() {
        [] => Err(SourceHostError::NotFound),
        [sha] => Ok(*sha),
        _ => Err(SourceHostError::Invalid(format!("ambiguous sha prefix {git_ref}"))),
    }
}

#[async_trait]
impl SourceHost for FakeSourceHost {
    async fn installation(&self, inst: InstallationId) -> Result<InstallationInfo, SourceHostError> {
        self.enter(SourceHostMethod::Installation, inst, None, None)?;
        let state = self.state.lock();
        installation(&state.world, inst).map(|i| i.info.clone())
    }

    async fn installation_repos(&self, inst: InstallationId) -> Result<Vec<RemoteRepo>, SourceHostError> {
        self.enter(SourceHostMethod::InstallationRepos, inst, None, None)?;
        let state = self.state.lock();
        Ok(usable(&state.world, inst)?.repos.iter().map(|r| r.remote.clone()).collect())
    }

    async fn repo(&self, inst: InstallationId, coords: &RepoCoords) -> Result<RemoteRepo, SourceHostError> {
        self.enter(SourceHostMethod::Repo, inst, Some(coords), None)?;
        let state = self.state.lock();
        repo(&state.world, inst, coords).map(|r| r.remote.clone())
    }

    async fn resolve_ref(&self, inst: InstallationId, coords: &RepoCoords, git_ref: &str) -> Result<CommitSha, SourceHostError> {
        self.enter(SourceHostMethod::ResolveRef, inst, Some(coords), Some(git_ref))?;
        let state = self.state.lock();
        resolve(repo(&state.world, inst, coords)?, git_ref)
    }

    async fn installation_token(&self, inst: InstallationId, scope: &TokenScope) -> Result<InstallationToken, SourceHostError> {
        self.enter(SourceHostMethod::InstallationToken, inst, None, None)?;
        let mut state = self.state.lock();
        self.token(&mut state, inst, scope)
    }

    async fn fetch_spec(&self, inst: InstallationId, coords: &RepoCoords) -> Result<FetchSpec, SourceHostError> {
        self.enter(SourceHostMethod::FetchSpec, inst, Some(coords), None)?;
        let mut state = self.state.lock();
        let canonical = repo(&state.world, inst, coords)?.remote.coords.clone();
        let url = Url::parse(&format!("https://github.com/{}/{}.git", canonical.owner, canonical.name))
            .map_err(|e| SourceHostError::Invalid(format!("repo coords {canonical}: {e}")))?;
        let token = self.token(&mut state, inst, &TokenScope::fetch(&canonical.name))?;
        Ok(FetchSpec { url, username: FETCH_USERNAME.to_owned(), token })
    }

    async fn list_pull_requests(&self, inst: InstallationId, coords: &RepoCoords, query: &PrListQuery) -> Result<PrPage, SourceHostError> {
        self.enter(SourceHostMethod::ListPullRequests, inst, Some(coords), None)?;
        if query.per_page == 0 || query.per_page > MAX_PR_PAGE {
            return Err(SourceHostError::Invalid(format!("per_page {} outside 1..=100", query.per_page)));
        }
        let offset: usize = match &query.page_token {
            None => 0,
            Some(token) => token.parse().map_err(|_| SourceHostError::Invalid(format!("bad page token {token:?}")))?,
        };
        let state = self.state.lock();
        let world_repo = repo(&state.world, inst, coords)?;
        let mut prs: Vec<_> = world_repo
            .pull_requests
            .iter()
            .filter(|pr| match query.state {
                PrState::Open => pr.open,
                PrState::Closed => !pr.open,
                PrState::All => true,
            })
            .filter(|pr| query.updated_since.is_none_or(|since| pr.info.updated_at >= since))
            .map(|pr| pr.info.clone())
            .collect();
        match query.sort {
            PrSort::Created => prs.sort_by_key(|pr| pr.number),
            PrSort::Updated => prs.sort_by_key(|pr| (pr.updated_at, pr.number)),
        }
        if query.direction == SortDirection::Desc {
            prs.reverse();
        }
        let per_page = usize::from(query.per_page);
        let items: Vec<_> = prs.iter().skip(offset).take(per_page).cloned().collect();
        let end = offset + items.len();
        let next = (end < prs.len()).then(|| end.to_string());
        Ok(PrPage { items, next })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::source_host::{Access, AccountKind, RepoSelection};
    use crate::testing::{FakeClock, contract_source_host};

    #[tokio::test]
    async fn fake_source_host_passes_the_contract() {
        contract_source_host(|world, clock| async move { Arc::new(FakeSourceHost::new(clock, world)) as Arc<dyn SourceHost> }).await;
    }

    #[tokio::test]
    async fn scripted_failures_are_per_method_and_calls_are_logged() {
        let clock = Arc::new(FakeClock::new());
        let host = FakeSourceHost::new(clock, SourceHostWorld::default());
        let retry_at = FakeClock::DEFAULT_START;
        host.fail_next(SourceHostMethod::Installation, SourceHostError::RateLimited { retry_at });
        let inst = InstallationId(7);
        assert_eq!(host.installation_repos(inst).await, Err(SourceHostError::NotFound), "other methods are unaffected");
        assert_eq!(host.installation(inst).await, Err(SourceHostError::RateLimited { retry_at }));
        assert_eq!(host.installation(inst).await, Err(SourceHostError::NotFound), "a scripted failure fires once");
        let methods: Vec<SourceHostMethod> = host.calls().into_iter().map(|c| c.method).collect();
        assert_eq!(methods, vec![SourceHostMethod::InstallationRepos, SourceHostMethod::Installation, SourceHostMethod::Installation]);

        host.update_world(|world| {
            world.installations.push(WorldInstallation {
                info: InstallationInfo {
                    id: inst,
                    account_login: "acme".into(),
                    account_kind: AccountKind::User,
                    permissions: BTreeMap::from([("contents".to_owned(), Access::Read)]),
                    repository_selection: RepoSelection::All,
                    suspended: false,
                },
                repos: vec![],
            });
        });
        assert_eq!(host.installation(inst).await.map(|i| i.account_login), Ok("acme".to_owned()));
    }
}
