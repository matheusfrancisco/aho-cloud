//! Test wiring for the intake use cases: every port faked, one org with Linear (API key) and
//! Slack bound, one granted repo whose GitHub world is scripted.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use mage_core::domain::{
    CommitSha, InstallationId, Integration, IntegrationKind, IntegrationStatus, JobKind, Org, OrgId, Repo, RepoAccess, RepoId, UserId,
};
use mage_core::ports::jobs::{JobRecord, JobState};
use mage_core::ports::source_host::{AccountKind, InstallationInfo, RemoteRepo, RepoCoords, RepoSelection};
use mage_core::ports::RepoStore;
use mage_core::testing::{FakeClock, FakeSourceHost, MemArtifactStore, MemJobQueue, MemRepoStore, SeqIdGen, SourceHostWorld, WorldInstallation, WorldRepo};
use parking_lot::Mutex;
use time::OffsetDateTime;
use time::macros::datetime;
use url::Url;
use uuid::Uuid;

use crate::app::fix::media::MediaCache;
use crate::app::intake::config::IntakeMode;
use crate::app::intake::deliver::ProcessDelivery;
use crate::app::intake::github::GithubIngest;
use crate::app::intake::lifecycle::{BindIntegration, ConnectConfig, IntegrationEvents};
use crate::app::intake::linear_issue::{IngestLinearIssue, TrackerIdentityCache};
use crate::app::intake::notify::Notify;
use crate::app::intake::reconcile::Reconcile;
use crate::app::intake::target::{DeploymentChain, TargetResolver};
use crate::app::intake::tickets::TicketsApi;
use crate::domain::intake::{IntakeConfig, IntegrationSettings, LinearAuthMode};
use crate::ports::hooks::{DecodeError, HookEvent, WebhookDecoder, WebhookSource};
use crate::ports::org::{OrgStore, RetentionPolicy};
use crate::ports::tracker::{TrackerIdentity, TrackerIssue, TrackerStateKind};
use crate::testing::{
    FakeChat, FakeDeploymentResolver, FakeSourceHostAdmin, FakeTracker, MemChangeFeed, MemOrgStore, MemOutboundLog, MemRunStore,
    MemSecretVault, MemTicketStore, MemWebhookInbox,
};

pub const T0: OffsetDateTime = datetime!(2026-10-01 12:00 UTC);
pub const WORKSPACE: &str = "ws-1";
pub const TEAM: &str = "T1";
pub const INSTALLATION: InstallationId = InstallationId(7);
pub const MAGE_USER: &str = "mage-user";

pub fn sha(byte: u8) -> CommitSha {
    CommitSha::from_bytes([byte; 20])
}

/// Default branch head in the GitHub world.
pub const HEAD: u8 = 0x11;
/// Other commits the repo knows (directive targets).
pub const OTHER: u8 = 0x22;

/// `body` → event; anything else is a decode error.
pub struct ScriptedDecoder {
    pub source: WebhookSource,
    pub events: Mutex<HashMap<Vec<u8>, HookEvent>>,
}

impl ScriptedDecoder {
    pub fn new(source: WebhookSource) -> Self {
        Self { source, events: Mutex::new(HashMap::new()) }
    }
    pub fn script(&self, body: &[u8], event: HookEvent) {
        self.events.lock().insert(body.to_vec(), event);
    }
}

impl WebhookDecoder for ScriptedDecoder {
    fn source(&self) -> WebhookSource {
        self.source
    }
    fn decode(&self, _event: &str, body: &[u8]) -> Result<HookEvent, DecodeError> {
        self.events.lock().get(body).cloned().ok_or_else(|| DecodeError("unexpected payload shape".into()))
    }
}

pub struct Kit {
    pub clock: Arc<FakeClock>,
    pub ids: Arc<SeqIdGen>,
    pub jobs: Arc<MemJobQueue>,
    pub orgs: Arc<MemOrgStore>,
    pub tickets: Arc<MemTicketStore>,
    pub repos: Arc<MemRepoStore>,
    pub inbox: Arc<MemWebhookInbox>,
    pub outbound: Arc<MemOutboundLog>,
    pub tracker: Arc<FakeTracker>,
    pub chat: Arc<FakeChat>,
    pub source: Arc<FakeSourceHost>,
    pub admin: Arc<FakeSourceHostAdmin>,
    pub deployments: Arc<FakeDeploymentResolver>,
    pub runs: Arc<MemRunStore>,
    pub artifacts: Arc<MemArtifactStore>,
    pub vault: Arc<MemSecretVault>,
    pub identities: Arc<TrackerIdentityCache>,
    pub decoders: Vec<Arc<ScriptedDecoder>>,
    pub org: OrgId,
    pub repo: Repo,
}

pub fn remote_repo() -> RemoteRepo {
    RemoteRepo {
        external_id: 100,
        coords: RepoCoords { owner: "acme".into(), name: "shop".into() },
        default_branch: "main".into(),
        private: true,
        archived: false,
    }
}

pub fn world() -> SourceHostWorld {
    SourceHostWorld {
        installations: vec![WorldInstallation {
            info: InstallationInfo {
                id: INSTALLATION,
                account_login: "acme".into(),
                account_kind: AccountKind::Organization,
                permissions: BTreeMap::new(),
                repository_selection: RepoSelection::Selected,
                suspended: false,
            },
            repos: vec![WorldRepo {
                remote: remote_repo(),
                branches: BTreeMap::from([("main".to_owned(), sha(HEAD))]),
                tags: BTreeMap::from([("v1.0".to_owned(), sha(0x44))]),
                commits: vec![sha(OTHER), sha(0x33), sha(0x55), sha(0x66)],
                pull_requests: vec![],
            }],
        }],
    }
}

impl Kit {
    pub async fn new() -> Kit {
        let clock = Arc::new(FakeClock::at(T0));
        let ids = Arc::new(SeqIdGen::new());
        let jobs = Arc::new(MemJobQueue::new(clock.clone(), ids.clone()));
        let feed = Arc::new(MemChangeFeed::new(clock.clone()));
        let orgs = Arc::new(MemOrgStore::new(clock.clone(), jobs.clone(), feed.clone()));
        let tickets = Arc::new(MemTicketStore::new(clock.clone(), jobs.clone(), feed));
        let repos = Arc::new(MemRepoStore::new(clock.clone(), jobs.clone()));
        let inbox = Arc::new(MemWebhookInbox::new(clock.clone(), jobs.clone()));
        let outbound = Arc::new(MemOutboundLog::new(clock.clone(), ids.clone()));
        let tracker = Arc::new(FakeTracker::new());
        let chat = Arc::new(FakeChat::new(clock.clone()));
        let source = Arc::new(FakeSourceHost::new(clock.clone(), world()));
        let runs = Arc::new(MemRunStore::new(clock.clone(), jobs.clone()));
        let artifacts = Arc::new(MemArtifactStore::new(clock.clone()));
        let vault = Arc::new(MemSecretVault::new(clock.clone()));
        let identities = Arc::new(TrackerIdentityCache::new(tracker.clone(), clock.clone()));

        let org = OrgId::from_uuid(Uuid::from_u128(0xa0));
        let owner = UserId::from_uuid(Uuid::from_u128(0xa1));
        orgs.create_org(&Org { id: org, slug: "acme".into(), name: "Acme".into(), created_at: T0 }, owner, &RetentionPolicy::default()).await.unwrap();
        let mut cfg = IntakeConfig::default();
        cfg.linear.teams = vec!["ENG".into()];
        orgs.put_intake_config(org, &cfg).await.unwrap();
        for (kind, external, settings) in [
            (IntegrationKind::Linear, WORKSPACE, IntegrationSettings { linear_auth: Some(LinearAuthMode::ApiKey), ..Default::default() }),
            (IntegrationKind::Slack, TEAM, IntegrationSettings { slack_team_domain: Some("acme".into()), ..Default::default() }),
            (IntegrationKind::GitHub, "7", IntegrationSettings::default()),
        ] {
            let i = Integration { org_id: org, kind, external_id: external.into(), status: IntegrationStatus::Active };
            orgs.upsert_integration(&i, &[]).await.unwrap();
            orgs.set_integration_settings(org, kind, external, &settings).await.unwrap();
        }
        tracker.set_identity(org, TrackerIdentity { user_id: MAGE_USER.into(), app_id: None });

        let r = remote_repo();
        let repo = Repo {
            id: RepoId::from_uuid(Uuid::from_u128(0xb0)),
            org_id: org,
            external_id: r.external_id,
            owner: r.coords.owner,
            name: r.coords.name,
            default_branch: r.default_branch,
            installation_id: INSTALLATION,
            access: RepoAccess::Granted,
            created_at: T0,
        };
        let repo = repos.upsert_repo(&repo, &[]).await.unwrap();

        let decoders = [WebhookSource::GitHub, WebhookSource::GitHubFix, WebhookSource::Linear, WebhookSource::Slack]
            .into_iter()
            .map(|s| Arc::new(ScriptedDecoder::new(s)))
            .collect();
        Kit {
            clock,
            ids,
            jobs,
            orgs,
            tickets,
            repos,
            inbox,
            outbound,
            tracker,
            chat,
            source,
            admin: Arc::new(FakeSourceHostAdmin::new()),
            deployments: Arc::new(FakeDeploymentResolver::new()),
            runs,
            artifacts,
            vault,
            identities,
            decoders,
            org,
            repo,
        }
    }

    pub async fn config(&self, change: impl FnOnce(&mut IntakeConfig)) {
        let mut cfg = self.orgs.intake_config(self.org).await.unwrap();
        change(&mut cfg);
        self.orgs.put_intake_config(self.org, &cfg).await.unwrap();
    }

    /// An ENG issue in the Triage state, reported an hour before `T0`.
    pub fn issue(&self, id: &str) -> TrackerIssue {
        TrackerIssue {
            issue_id: id.into(),
            identifier: format!("ENG-{}", id.len()),
            url: Url::parse(&format!("https://linear.app/acme/issue/{id}")).unwrap(),
            workspace_id: WORKSPACE.into(),
            team_key: "ENG".into(),
            project: None,
            labels: vec![],
            state_name: "Triage".into(),
            state_kind: TrackerStateKind::Triage,
            title: "Invoice export fails".into(),
            description: "Export button shows `Something went wrong exporting`".into(),
            created_at: T0 - time::Duration::hours(1),
            updated_at: T0 - time::Duration::minutes(5),
            archived: false,
            comments: vec![],
            attachments: vec![],
            customer_ids: vec![],
        }
    }

    pub fn github(&self) -> Arc<GithubIngest> {
        Arc::new(GithubIngest::new(self.orgs.clone(), self.repos.clone(), self.tickets.clone(), self.source.clone(), self.jobs.clone(), self.clock.clone()))
    }

    pub fn targets(&self) -> Arc<TargetResolver> {
        let chain = Arc::new(DeploymentChain::new(vec![self.deployments.clone()]));
        Arc::new(TargetResolver::new(self.source.clone(), self.repos.clone(), chain))
    }

    pub fn ingest(&self) -> IngestLinearIssue {
        IngestLinearIssue::new(
            self.orgs.clone(),
            self.tickets.clone(),
            self.repos.clone(),
            self.tracker.clone(),
            self.chat.clone(),
            self.outbound.clone(),
            self.targets(),
            self.identities.clone(),
            self.clock.clone(),
            self.ids.clone(),
        )
    }

    pub fn lifecycle(&self) -> Arc<IntegrationEvents> {
        Arc::new(IntegrationEvents::new(
            self.orgs.clone(),
            self.repos.clone(),
            self.source.clone(),
            self.admin.clone(),
            self.vault.clone(),
            self.github(),
            self.jobs.clone(),
            self.clock.clone(),
            self.ids.clone(),
        ))
    }

    pub fn deliver(&self) -> ProcessDelivery {
        ProcessDelivery::new(
            self.inbox.clone(),
            self.decoders.iter().map(|d| d.clone() as Arc<dyn WebhookDecoder>).collect(),
            self.orgs.clone(),
            self.repos.clone(),
            self.jobs.clone(),
            self.identities.clone(),
            self.github(),
            self.lifecycle(),
            self.clock.clone(),
        )
    }

    pub fn decoder(&self, source: WebhookSource) -> &ScriptedDecoder {
        self.decoders.iter().find(|d| d.source == source).expect("decoder per source")
    }

    pub fn reconcile(&self, fix_app: bool) -> Reconcile {
        Reconcile::new(
            self.orgs.clone(),
            self.repos.clone(),
            self.tickets.clone(),
            self.inbox.clone(),
            self.jobs.clone(),
            self.source.clone(),
            self.admin.clone(),
            self.tracker.clone(),
            self.deployments.clone(),
            self.github(),
            self.lifecycle(),
            self.clock.clone(),
            IntakeMode::Webhook,
            fix_app,
        )
    }

    pub fn bind(&self) -> BindIntegration {
        BindIntegration::new(
            self.orgs.clone(),
            self.admin.clone(),
            self.tracker.clone(),
            self.chat.clone(),
            self.vault.clone(),
            self.clock.clone(),
            ConnectConfig {
                linear_client_id: Some("lin-client".into()),
                slack_client_id: Some("slack-client".into()),
                github_fix_app_slug: Some("ahomage-fix".into()),
                ..ConnectConfig::default()
            },
        )
    }

    pub fn notify(&self) -> Notify {
        Notify::new(
            self.orgs.clone(),
            self.tickets.clone(),
            self.runs.clone(),
            self.repos.clone(),
            self.outbound.clone(),
            self.tracker.clone(),
            self.chat.clone(),
            self.artifacts.clone(),
            Arc::new(MediaCache::new(self.artifacts.clone())),
            self.clock.clone(),
            self.ids.clone(),
            Url::parse("https://mage.example/").unwrap(),
        )
    }

    pub fn tickets_api(&self) -> TicketsApi {
        TicketsApi::new(self.tickets.clone(), self.repos.clone(), self.orgs.clone(), self.runs.clone(), self.source.clone(), self.clock.clone())
    }

    /// Active (queued) jobs of `kind`, oldest first.
    pub fn queued(&self, kind: JobKind) -> Vec<JobRecord> {
        self.jobs.jobs().into_iter().filter(|j| j.kind == kind && j.state == JobState::Queued).collect()
    }

    /// Drop every queued job (so a later step's enqueues are observable on their own).
    pub async fn drain(&self) {
        for j in self.jobs.jobs().into_iter().filter(|j| j.state == JobState::Queued) {
            mage_core::ports::JobQueue::cancel(self.jobs.as_ref(), j.id).await.unwrap();
        }
    }
}
