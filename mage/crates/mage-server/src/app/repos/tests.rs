#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use mage_core::domain::{
    Budget, CommitSha, CustomerId, DeploymentSource, ExternalRef, InstallationId, Integration, IntegrationKind,
    IntegrationStatus, JobPayload, OrgId, RecipeStatus, RepoAccess, RepoId, RepoVersionKey, RunId, RunKind, RunSubject,
    VersionReason,
};
use mage_core::ports::jobs::JobState;
use mage_core::ports::source_host::{AccountKind, InstallationInfo, RemoteRepo, RepoCoords, RepoSelection};
use mage_core::ports::{Clock, IdGen, RepoStore};
use mage_core::testing::{FakeClock, FakeSourceHost, MemJobQueue, MemRepoStore, SeqIdGen, SourceHostWorld, WorldInstallation, WorldRepo};
use mage_core::wire::api::CustomerInput;
use uuid::Uuid;

use super::ReposService;
use crate::error::AppError;
use crate::ports::org::OrgStore;
use crate::ports::runs::{NewRun, RunStore, RunWrite};
use crate::domain::runs::RunSpec;
use crate::testing::runs::{sample_run, sample_spec};
use crate::testing::{MemChangeFeed, MemOrgStore, MemRunStore};

const INST: InstallationId = InstallationId(7);
pub(crate) const WEB: u64 = 101;
const ARCHIVED: u64 = 102;
const API: u64 = 103;

fn sha(n: u8) -> CommitSha {
    CommitSha::from_bytes([n; 20])
}

fn org(n: u128) -> OrgId {
    OrgId::from_uuid(Uuid::from_u128(n))
}

fn world_repo(external_id: u64, name: &str, archived: bool, head: CommitSha) -> WorldRepo {
    WorldRepo {
        remote: RemoteRepo {
            external_id,
            coords: RepoCoords { owner: "acme".into(), name: name.into() },
            default_branch: "main".into(),
            private: true,
            archived,
        },
        branches: BTreeMap::from([("main".to_owned(), head), ("feature".to_owned(), sha(9))]),
        tags: BTreeMap::from([("v1.0.0".to_owned(), sha(3))]),
        commits: vec![sha(4)],
        pull_requests: vec![],
    }
}

/// Org 1 with an active GitHub integration on installation [`INST`] (repos `web`, `api`, and an
/// archived `old`); the driving adapter tests reuse it.
pub(crate) struct Fixture {
    pub(crate) svc: Arc<ReposService>,
    pub(crate) repos: Arc<MemRepoStore>,
    orgs: Arc<MemOrgStore>,
    runs: Arc<MemRunStore>,
    jobs: Arc<MemJobQueue>,
    clock: Arc<FakeClock>,
    ids: Arc<SeqIdGen>,
    pub(crate) org: OrgId,
}

impl Fixture {
    pub(crate) async fn new() -> Self {
        let clock = Arc::new(FakeClock::new());
        let ids = Arc::new(SeqIdGen::new());
        let jobs = Arc::new(MemJobQueue::new(clock.clone(), ids.clone()));
        let repos = Arc::new(MemRepoStore::new(clock.clone(), jobs.clone()));
        let feed = Arc::new(MemChangeFeed::new(clock.clone()));
        let orgs = Arc::new(MemOrgStore::new(clock.clone(), jobs.clone(), feed));
        let runs = Arc::new(MemRunStore::new(clock.clone(), jobs.clone()));
        let world = SourceHostWorld {
            installations: vec![WorldInstallation {
                info: InstallationInfo {
                    id: INST,
                    account_login: "acme".into(),
                    account_kind: AccountKind::Organization,
                    permissions: BTreeMap::new(),
                    repository_selection: RepoSelection::All,
                    suspended: false,
                },
                repos: vec![world_repo(WEB, "web", false, sha(1)), world_repo(ARCHIVED, "old", true, sha(2)), world_repo(API, "api", false, sha(5))],
            }],
        };
        let source = Arc::new(FakeSourceHost::new(clock.clone(), world));
        let org = org(1);
        orgs.upsert_integration(
            &Integration { org_id: org, kind: IntegrationKind::GitHub, external_id: INST.0.to_string(), status: IntegrationStatus::Active },
            &[],
        )
        .await
        .unwrap();
        let svc = Arc::new(ReposService::new(repos.clone(), orgs.clone(), runs.clone(), source, jobs.clone(), clock.clone(), ids.clone()));
        Self { svc, repos, orgs, runs, jobs, clock, ids, org }
    }

    /// Active `index.version` jobs as `(repo, sha, reason)`.
    pub(crate) fn index_jobs(&self) -> Vec<(RepoId, CommitSha, VersionReason)> {
        self.jobs
            .jobs()
            .into_iter()
            .filter(|j| j.state == JobState::Queued)
            .filter_map(|j| match j.payload {
                JobPayload::IndexVersion { repo_id, sha, reason, .. } => Some((repo_id, sha, reason)),
                _ => None,
            })
            .collect()
    }

    async fn reasons(&self, repo: RepoId, sha: CommitSha) -> Vec<VersionReason> {
        self.repos.version(self.org, RepoVersionKey::new(repo, sha)).await.unwrap().reasons.into_iter().collect()
    }
}

#[tokio::test]
async fn connect_stores_repo_and_enqueues_default_head_index_once() {
    let f = Fixture::new().await;
    let repo = f.svc.connect(f.org, WEB).await.unwrap();
    assert_eq!((repo.org_id, repo.external_id, repo.access, repo.installation_id), (f.org, WEB, RepoAccess::Granted, INST));
    assert_eq!((repo.owner.as_str(), repo.name.as_str(), repo.default_branch.as_str()), ("acme", "web", "main"));
    assert_eq!(f.index_jobs(), vec![(repo.id, sha(1), VersionReason::DefaultHead)]);
    assert_eq!(f.repos.default_head(f.org, repo.id).await.unwrap(), Some(sha(1)));

    // Connecting again is idempotent: same id, the queued job is deduplicated.
    let again = f.svc.connect(f.org, WEB).await.unwrap();
    assert_eq!(again.id, repo.id);
    assert_eq!(f.index_jobs().len(), 1);
}

#[tokio::test]
async fn connect_rejects_repos_outside_the_installation_and_archived_ones() {
    let f = Fixture::new().await;
    assert!(matches!(f.svc.connect(f.org, 999).await, Err(AppError::Invalid(_))));
    assert!(matches!(f.svc.connect(f.org, ARCHIVED).await, Err(AppError::Invalid(_))));
    // Another org has no GitHub integration: the installation's repos are not its repos.
    assert!(matches!(f.svc.connect(org(2), WEB).await, Err(AppError::Invalid(_))));
    // A revoked integration no longer grants its repos.
    f.orgs
        .set_integration_status(f.org, IntegrationKind::GitHub, &INST.0.to_string(), IntegrationStatus::Revoked, &[])
        .await
        .unwrap();
    assert!(matches!(f.svc.connect(f.org, WEB).await, Err(AppError::Invalid(_))));
    assert!(f.repos.list_repos(f.org).await.unwrap().is_empty());
    assert!(f.index_jobs().is_empty());
}

#[tokio::test]
async fn installation_repos_lists_unconnected_repos_and_disconnect_reopens_them() {
    let f = Fixture::new().await;
    let names = |list: mage_core::wire::api::InstallationRepoList| list.repositories.into_iter().map(|r| r.name).collect::<Vec<_>>();
    assert_eq!(names(f.svc.installation_repos(f.org).await.unwrap()), ["api", "web"], "archived repos are left out");

    let web = f.svc.connect(f.org, WEB).await.unwrap();
    assert_eq!(names(f.svc.installation_repos(f.org).await.unwrap()), ["api"]);

    f.svc.disconnect(f.org, web.id).await.unwrap();
    assert_eq!(f.repos.repo(f.org, web.id).await.unwrap().access, RepoAccess::Lost, "rows stay, access is lost");
    assert_eq!(names(f.svc.installation_repos(f.org).await.unwrap()), ["api", "web"]);

    let reconnected = f.svc.connect(f.org, WEB).await.unwrap();
    assert_eq!((reconnected.id, reconnected.access), (web.id, RepoAccess::Granted), "a reconnect keeps the repo id");
}

#[tokio::test]
async fn another_orgs_repo_is_not_found() {
    let f = Fixture::new().await;
    let repo = f.svc.connect(f.org, WEB).await.unwrap();
    let other = org(2);
    assert!(matches!(f.svc.repo_detail(other, repo.id).await, Err(AppError::NotFound(_))));
    assert!(matches!(f.svc.disconnect(other, repo.id).await, Err(AppError::NotFound(_))));
    assert!(matches!(f.svc.index_rev(other, repo.id, "main").await, Err(AppError::NotFound(_))));
    assert!(matches!(f.svc.recipe_history(other, repo.id).await, Err(AppError::NotFound(_))));
    assert!(matches!(f.svc.save_recipe(other, repo.id, "version = 1\n".into()).await, Err(AppError::NotFound(_))));
    assert!(matches!(f.svc.list_deployments(other, repo.id).await, Err(AppError::NotFound(_))));
    assert!(matches!(f.svc.set_deployment(other, repo.id, "prod", sha(1)).await, Err(AppError::NotFound(_))));
    assert!(f.svc.list_repos(other).await.unwrap().repos.is_empty());
    assert_eq!(f.repos.repo(f.org, repo.id).await.unwrap().access, RepoAccess::Granted);
}

#[tokio::test]
async fn index_rev_picks_the_reason_from_the_ref() {
    let f = Fixture::new().await;
    let repo = f.svc.connect(f.org, API).await.unwrap();

    let tag = f.svc.index_rev(f.org, repo.id, "refs/tags/v1.0.0").await.unwrap();
    assert_eq!(f.reasons(repo.id, sha(3)).await, [VersionReason::Release("v1.0.0".into())]);
    assert!(f.index_jobs().contains(&(repo.id, sha(3), VersionReason::Release("v1.0.0".into()))));
    // Same sha again: the reason is idempotent and the job deduplicated.
    assert_eq!(f.svc.index_rev(f.org, repo.id, "v1.0.0").await.unwrap(), tag);

    // The default branch while its head job is queued: deduplicated onto the connect job.
    let head = f.svc.index_rev(f.org, repo.id, "main").await.unwrap();
    assert_eq!(f.index_jobs().iter().filter(|(_, s, _)| *s == sha(5)).count(), 1);
    assert_ne!(head, tag);

    for rev in ["feature", "0404040404040404040404040404040404040404", "nope"] {
        assert!(matches!(f.svc.index_rev(f.org, repo.id, rev).await, Err(AppError::Invalid(_))), "{rev}");
    }
    assert!(matches!(f.svc.index_rev(f.org, repo.id, " ").await, Err(AppError::Invalid(_))));

    f.svc.disconnect(f.org, repo.id).await.unwrap();
    assert!(matches!(f.svc.index_rev(f.org, repo.id, "main").await, Err(AppError::Invalid(_))));
}

const RECIPE: &str = "version = 1\n# comment kept\n[env]\nNODE_ENV = \"development\"\n";

#[tokio::test]
async fn save_recipe_stores_a_draft_and_rejects_secrets() {
    let f = Fixture::new().await;
    let repo = f.svc.connect(f.org, WEB).await.unwrap();

    let saved = f.svc.save_recipe(f.org, repo.id, RECIPE.into()).await.unwrap();
    assert_eq!(saved.recipe.status, RecipeStatus::Draft);
    assert_eq!(saved.recipe.body, RECIPE, "the original text is kept");
    assert!(saved.report.is_valid());
    let history = f.svc.recipe_history(f.org, repo.id).await.unwrap();
    assert_eq!(history.current.as_ref().map(|r| r.hash), Some(saved.recipe.hash));
    assert!(history.history.is_empty());

    // Same canonical form (different spelling): same hash, nothing new stored.
    let respelled = f.svc.save_recipe(f.org, repo.id, "version = 1\n[env]\nNODE_ENV = 'development'\n".into()).await.unwrap();
    assert_eq!(respelled.recipe.hash, saved.recipe.hash);

    let live = "version = 1\n[env]\nSTRIPE_API_KEY = \"sk_live_0123456789abcdef\"\n";
    match f.svc.save_recipe(f.org, repo.id, live.into()).await {
        Err(AppError::Invalid(msg)) => {
            assert!(msg.contains("env.STRIPE_API_KEY") && msg.contains("LiveSecret"), "{msg}");
            assert!(!msg.contains("sk_live_0123"), "the secret is never echoed: {msg}");
        }
        other => panic!("expected a secret rejection, got {other:?}"),
    }
    assert!(matches!(f.svc.save_recipe(f.org, repo.id, "version = 1\nbogus = true\n".into()).await, Err(AppError::Invalid(_))));

    f.clock.advance(std::time::Duration::from_secs(1));
    let second = f.svc.save_recipe(f.org, repo.id, "version = 1\n".into()).await.unwrap();
    let history = f.svc.recipe_history(f.org, repo.id).await.unwrap();
    assert_eq!(history.current.map(|r| r.hash), Some(second.recipe.hash), "latest draft is current");
    assert_eq!(history.history.into_iter().map(|r| r.hash).collect::<Vec<_>>(), [saved.recipe.hash]);
}

#[tokio::test]
async fn manual_deployment_mapping_records_history_and_indexes_the_sha() {
    let f = Fixture::new().await;
    let repo = f.svc.connect(f.org, WEB).await.unwrap();

    let first = f.svc.set_deployment(f.org, repo.id, "prod", sha(3)).await.unwrap();
    assert_eq!((first.source, first.sha, first.environment.as_str()), (DeploymentSource::Manual, sha(3), "prod"));
    assert_eq!(f.reasons(repo.id, sha(3)).await, [VersionReason::Deployed("prod".into())]);
    assert!(f.index_jobs().contains(&(repo.id, sha(3), VersionReason::Deployed("prod".into()))));

    f.clock.advance(std::time::Duration::from_secs(60));
    f.svc.set_deployment(f.org, repo.id, "staging", sha(4)).await.unwrap();
    f.clock.advance(std::time::Duration::from_secs(60));
    let moved = f.svc.set_deployment(f.org, repo.id, "prod", sha(4)).await.unwrap();
    assert!(f.reasons(repo.id, sha(3)).await.is_empty(), "Deployed(prod) moved to the new sha");

    let list = f.svc.list_deployments(f.org, repo.id).await.unwrap().deployments;
    assert_eq!(list.iter().map(|d| (d.environment.as_str(), d.sha)).collect::<Vec<_>>(), [("prod", sha(4)), ("staging", sha(4))]);
    assert_eq!(list[0], moved);
    assert_eq!(f.svc.repo_detail(f.org, repo.id).await.unwrap().deployments, list);

    assert!(matches!(f.svc.set_deployment(f.org, repo.id, "prod", sha(0xee)).await, Err(AppError::Invalid(_))), "not a commit");
    assert!(matches!(f.svc.set_deployment(f.org, repo.id, "", sha(4)).await, Err(AppError::Invalid(_))));
}

#[tokio::test]
async fn repo_list_and_detail_show_head_recipe_and_last_learn() {
    let f = Fixture::new().await;
    let repo = f.svc.connect(f.org, WEB).await.unwrap();
    f.clock.advance(std::time::Duration::from_secs(1));
    f.svc.index_rev(f.org, repo.id, "v1.0.0").await.unwrap();
    let recipe = f.svc.save_recipe(f.org, repo.id, RECIPE.into()).await.unwrap().recipe;
    for _ in 0..2 {
        f.clock.advance(std::time::Duration::from_secs(1));
        let id = (f.ids.as_ref() as &dyn IdGen).new_id::<RunId>();
        let subject = RunSubject::RepoVersion(RepoVersionKey::new(repo.id, sha(1)));
        let budget = Budget { max_wall: std::time::Duration::from_secs(600), max_turns: 10, max_cost_micros: 1, max_gate_attempts: 1 };
        let run = sample_run(f.org, id, RunKind::Learn, subject, budget, f.clock.now());
        let spec = RunSpec { repo_id: Some(repo.id), sha: Some(sha(1)), ..sample_spec(RunKind::Learn) };
        let insert = NewRun { run, spec, create_key: None, source_ready: true, affinity: None };
        f.runs.commit(f.org, RunWrite { inserts: vec![insert], ..Default::default() }).await.unwrap();
    }
    let newest = f.runs.list(f.org, &crate::ports::runs::RunQuery { newest_first: true, ..Default::default() }).await.unwrap()[0].run.id;

    let list = f.svc.list_repos(f.org).await.unwrap().repos;
    assert_eq!(list.len(), 1);
    let summary = &list[0];
    assert_eq!(summary.head.as_ref().map(|h| (h.sha, h.reasons.clone())), Some((sha(1), vec![VersionReason::DefaultHead])));
    assert_eq!(summary.recipe.as_ref().map(|r| (r.hash, r.status.clone())), Some((recipe.hash, RecipeStatus::Draft)));
    assert_eq!(summary.last_learn.as_ref().map(|r| r.id), Some(newest));

    let detail = f.svc.repo_detail(f.org, repo.id).await.unwrap();
    assert_eq!(detail.versions.iter().map(|v| v.sha).collect::<Vec<_>>(), [sha(3), sha(1)], "newest first");
    assert_eq!(detail.recipe.map(|r| r.hash), Some(recipe.hash));
    assert_eq!(detail.last_learn.map(|r| r.id), Some(newest));
}

#[tokio::test]
async fn customers_are_created_updated_and_listed_per_org() {
    let f = Fixture::new().await;
    let input = |name: &str, env: Option<&str>| CustomerInput {
        name: name.into(),
        external_refs: vec![ExternalRef::LinearLabel("acme".into()), ExternalRef::LinearLabel("acme".into())],
        environment: env.map(str::to_owned),
    };
    let created = f.svc.create_customer(f.org, input(" Acme ", Some("prod"))).await.unwrap();
    assert_eq!((created.name.as_str(), created.environment.as_deref()), ("Acme", Some("prod")));
    assert_eq!(created.external_refs, [ExternalRef::LinearLabel("acme".into())], "duplicate refs collapse");

    let updated = f.svc.update_customer(f.org, created.id, input("Acme Corp", Some(""))).await.unwrap();
    assert_eq!((updated.id, updated.name.as_str(), updated.environment.as_deref()), (created.id, "Acme Corp", None));
    assert_eq!(f.svc.list_customers(f.org).await.unwrap().customers, [updated]);

    assert!(matches!(f.svc.update_customer(org(2), created.id, input("x", None)).await, Err(AppError::NotFound(_))));
    let unknown = CustomerId::from_uuid(Uuid::from_u128(77));
    assert!(matches!(f.svc.update_customer(f.org, unknown, input("x", None)).await, Err(AppError::NotFound(_))));
    assert!(matches!(f.svc.create_customer(f.org, input("  ", None)).await, Err(AppError::Invalid(_))));
    assert!(f.svc.list_customers(org(2)).await.unwrap().customers.is_empty());
}
