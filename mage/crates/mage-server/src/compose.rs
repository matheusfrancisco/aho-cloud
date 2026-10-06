//! Composition root of the server role (README "Inside a role crate"): builds every driven
//! adapter from [`Config`], wires the use cases in dependency order, registers the job handlers,
//! merges the HTTP routers and serves until the shutdown token is cancelled (server.md
//! "Shutdown"). Also the operator entry points `mage-app` exposes as CLI commands.
//!
//! Start order: pools + schema check → vault → platform keys → signer ring → stores and
//! provider adapters → knowledge → triage and fix hooks → `RunManager` → triage / fix / intake
//! use cases → job worker → routers → intake seed, reaper → bind. [`Server::serve`] then starts
//! the background loops (org feed and job wakeup listeners, key ring and webhook secret reloads,
//! worker) and serves; on shutdown `/ready` fails first, streams and long-polls end through the
//! shared token, the worker drains, and the pools close.

use std::collections::BTreeSet;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use bytes::Bytes;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use url::Url;

use mage_core::domain::{JobId, JobKind, JobLane, MemberRole, Org, OrgId, RepoPath, TicketId, TriageEvalId, UserId};
use mage_core::ports::jobs::{JobQueue, JobRecord};
use mage_core::ports::{ArtifactStore, Clock, IdGen, KnowledgeStore, Page, PageRequest, RepoStore, SourceHost, SystemClock, UuidV7Gen};
use mage_core::wire::runner::CredentialModeKind;
use mage_infra::blob::{BlobBackend, ObjectBlobs};
use mage_infra::github::{AppCredentials, GitHubClient, GitHubSourceHost};
use mage_infra::pg::jobs::spawn_wakeup_listener;
use mage_infra::pg::pool::{SchemaStatus, check_schema, connect};
use mage_infra::pg::{PgJobQueue, PgKnowledgeStore, PgPools, PgRepoStore};

use crate::adapters::driven::github::admin::{GithubAdmin, GithubAdminConfig};
use crate::adapters::driven::github::deployments::GithubDeploymentResolver;
use crate::adapters::driven::github::pull_requests::GitHubPullRequests;
use crate::adapters::driven::github::webhook::GithubDecoder;
use crate::adapters::driven::identity::GitHubIdentity;
use crate::adapters::driven::indexer_link::HttpIndexerLink;
use crate::adapters::driven::linear::{LinearDecoder, LinearTracker};
use crate::adapters::driven::llm_upstream::{AdcTokenSource, AnthropicApi, RoutedUpstream, VertexUpstream};
use crate::adapters::driven::pg::deployments_manual::PgDeploymentResolver;
use crate::adapters::driven::pg::{
    PgAuditLog, PgChangeFeed, PgOrgStore, PgOutboundLog, PgRunSignals, PgRunStore, PgSecretStore, PgTicketStore, PgTokenStore, PgTriageEvalStore,
    PgWebhookInbox,
};
use crate::adapters::driven::secretscan::RegexSecretScanner;
use crate::adapters::driven::signer::{Ed25519TokenSigner, RELOAD_EVERY};
use crate::adapters::driven::slack::{SlackDecoder, SlackNotifier};
use crate::adapters::driving::http::api_fix::{self, FixApiState};
use crate::adapters::driving::http::api_orgs::OrgsApiState;
use crate::adapters::driving::http::api_repos::{self, ReposState};
use crate::adapters::driving::http::api_runs::{self, RunsApiState};
use crate::adapters::driving::http::api_tickets::{self, TicketsHttpState};
use crate::adapters::driving::http::api_triage::{self, TriageHttpState};
use crate::adapters::driving::http::assets::Assets as UiBundle;
use crate::adapters::driving::http::auth::{AuthState, SessionGuard};
use crate::adapters::driving::http::feed_sse::{FeedState, SseTimings};
use crate::adapters::driving::http::health::{FnProbe, HealthState, PgReady, ReadyProbe};
use crate::adapters::driving::http::hooks::{self, HookSecrets, HooksState, SECRET_REFRESH_EVERY};
use crate::adapters::driving::http::llm::{self, GatewayState};
use crate::adapters::driving::http::mcp::{self, KnowledgeMcpState};
use crate::adapters::driving::http::run_sse::{self, RunSseState};
use crate::adapters::driving::http::runner::{self, RunnerApiState};
use crate::adapters::driving::http::shell::{AreaRouters, PlatformHttp, RateLimits, origin_of, router};
use crate::adapters::driving::jobs::fix::FixJobs;
use crate::adapters::driving::jobs::intake::{IntakeChains, IntakeJobs};
use crate::adapters::driving::jobs::platform::{PlatformJobs, platform_tickers};
use crate::adapters::driving::jobs::runs::RunJobs;
use crate::adapters::driving::jobs::triage::TriageJobs;
use crate::adapters::driving::worker::Worker;
use crate::app::auth::keys::{EnsurePlatformKeys, RewrapSecrets, RotateSigningKey, RotationPolicy};
use crate::app::auth::operator::{IssueRunnerBootstrap, PutSecret};
use crate::app::auth::tokens::TokenService;
use crate::app::auth::{Login, LoginConfig, RequestMeta, Sessions};
use crate::app::fix::media::MediaCache;
use crate::app::fix::{AcceptFixResult, ApplyFixOutcome, FixDeps, LateRunManager};
use crate::app::gateway::{Forward, GatewayDeps, GatewayMode};
use crate::app::intake::deliver::ProcessDelivery;
use crate::app::intake::github::GithubIngest;
use crate::app::intake::lifecycle::{BindIntegration, IntegrationEvents, seed};
use crate::app::intake::linear_issue::{IngestLinearIssue, TrackerIdentityCache};
use crate::app::intake::notify::Notify;
use crate::app::intake::reconcile::Reconcile;
use crate::app::intake::target::{DeploymentChain, TargetResolver};
use crate::app::intake::tickets::TicketsApi;
use crate::app::intake::{AcceptDelivery, IntakeMode};
use crate::app::knowledge_query::eval::{EvalOptions, EvalReport as KnowledgeEvalReport, KnowledgeEval};
use crate::app::knowledge_query::{AuthorizeKnowledge, Retriever, RetrieverDeps, Service, ServiceDeps, SourceReader, indexer_token_minter};
use crate::app::orgs::{Activity, CreateOrg, Directory, Members, OrgOwner};
use crate::app::repos::ReposService;
use crate::app::runs::{RunDeps, RunHooks, RunManager};
use crate::app::settings::{Integrations, ModelSettingsService, OrgSettingsService};
use crate::app::triage::{
    ApplyTriageDeps, ApplyTriageOutcome, EvalReport as TriageEvalReporter, EvidenceGate, LabelTriageEval, RequestRetriage, StartEvalCase,
    StartEvalCaseDeps, StartTriage, StartTriageDeps, StartTriageEval, StartTriageEvalRequest, TriageEvalReport,
};
use crate::config::Config;
use crate::error::AppError;
use crate::ports::audit::{Actor, AuditLog};
use crate::ports::auth::{IdentityProvider, TokenSigner};
use crate::ports::chat::ChatNotifier;
use crate::ports::deployments::DeploymentResolver;
use crate::ports::feed::ChangeFeed;
use crate::ports::hooks::{WebhookDecoder, WebhookSource};
use crate::ports::inbox::WebhookInbox;
use crate::ports::indexer_link::IndexerLink;
use crate::ports::llm::ModelUpstream;
use crate::ports::org::OrgStore;
use crate::ports::outbound::OutboundLog;
use crate::ports::pull_requests::PullRequests;
use crate::ports::runs::{RunSignals, RunStore};
use crate::ports::secrets::SecretStore;
use crate::ports::secretscan::SecretScanner;
use crate::ports::source_host_admin::SourceHostAdmin;
use crate::ports::ticket::TicketStore;
use crate::ports::tokens::{RunnerScope, TokenStore};
use crate::ports::tracker::TicketTracker;
use crate::ports::triage_eval::{GroundTruth, TriageEval, TriageEvalStore};
use crate::ports::vault::{SecretBytes, SecretMeta, SecretName, SecretScope, SecretVault, VaultError, names};

pub use crate::app::auth::operator::IssuedBootstrap as IssuedBootstrapToken;

/// Platform secret holding the Mage Fix App's PEM key (server.md "Secret inventory").
const GITHUB_FIX_APP_PRIVATE_KEY: &str = "github_fix/app_private_key";
/// Timeouts of the identity providers' HTTP client.
const IDENTITY_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const IDENTITY_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// Why the server could not start or stopped with an error.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("invalid server config: {0}")]
    Config(String),
    #[error("database: {0}")]
    Database(String),
    #[error("{0}")]
    Startup(String),
    #[error("cannot listen on {addr}: {source}")]
    Bind { addr: SocketAddr, source: std::io::Error },
    #[error("http server: {0}")]
    Serve(std::io::Error),
    #[error(transparent)]
    App(#[from] AppError),
}

fn startup(what: &str, err: impl std::fmt::Display) -> RunError {
    RunError::Startup(format!("{what}: {err}"))
}

/// The `mage/web-app` bundle the server serves on the public host.
#[derive(Debug, Clone)]
pub enum Assets {
    /// A built `dist/` directory, read into memory at start.
    Dir(PathBuf),
    /// Files embedded in the binary: (path relative to the bundle root, bytes); `index.html`
    /// is required.
    Embedded(Vec<(String, &'static [u8])>),
    /// API only: the UI paths answer 404.
    None,
}

impl Assets {
    fn bundle(self) -> Result<Option<UiBundle>, RunError> {
        let bundle = match self {
            Assets::Dir(dir) => UiBundle::from_dir(&dir),
            Assets::Embedded(files) => UiBundle::from_map(files.into_iter().map(|(path, bytes)| (path, Bytes::from_static(bytes)))),
            Assets::None => return Ok(None),
        };
        bundle.map(Some).map_err(|e| startup("web-app bundle", e))
    }
}

/// Start the server role and serve until `shutdown` is cancelled.
pub async fn run(config: Config, assets: Assets, shutdown: CancellationToken) -> Result<(), RunError> {
    Server::build(config, assets, shutdown).await?.serve().await
}

// ---------------------------------------------------------------------------------------------
// Stores
// ---------------------------------------------------------------------------------------------

/// The Postgres-backed stores every entry point shares.
struct Stores {
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGen>,
    pools: PgPools,
    orgs: Arc<dyn OrgStore>,
    tickets: Arc<dyn TicketStore>,
    runs: Arc<dyn RunStore>,
    tokens: Arc<dyn TokenStore>,
    audit: Arc<dyn AuditLog>,
    secrets: Arc<dyn SecretStore>,
    inbox: Arc<dyn WebhookInbox>,
    outbound: Arc<dyn OutboundLog>,
    evals: Arc<dyn TriageEvalStore>,
    feed: PgChangeFeed,
    jobs: Arc<dyn JobQueue>,
    repos: Arc<dyn RepoStore>,
    knowledge: Arc<dyn KnowledgeStore>,
}

impl Stores {
    /// Connects both pools and refuses a database behind this binary.
    async fn open(config: &Config) -> Result<Stores, RunError> {
        let pools = connect(&config.database).await.map_err(|e| RunError::Database(e.to_string()))?;
        match check_schema(&pools.platform).await {
            Ok(SchemaStatus::Current) => {}
            Ok(SchemaStatus::Ahead { unknown }) => warn!(?unknown, "database has migrations this binary does not know"),
            Err(e) => {
                pools.close().await;
                return Err(RunError::Database(e.to_string()));
            }
        }
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let ids: Arc<dyn IdGen> = Arc::new(UuidV7Gen);
        let p = || pools.clone();
        Ok(Stores {
            orgs: Arc::new(PgOrgStore::new(p(), clock.clone(), ids.clone())),
            tickets: Arc::new(PgTicketStore::new(p(), clock.clone(), ids.clone())),
            runs: Arc::new(PgRunStore::new(p(), clock.clone(), ids.clone()).with_thresholds(config.runs.due_thresholds())),
            tokens: Arc::new(PgTokenStore::new(p(), clock.clone(), ids.clone())),
            audit: Arc::new(PgAuditLog::new(p(), clock.clone(), ids.clone())),
            secrets: Arc::new(PgSecretStore::new(p(), clock.clone(), ids.clone())),
            inbox: Arc::new(PgWebhookInbox::new(p(), clock.clone(), ids.clone())),
            outbound: Arc::new(PgOutboundLog::new(p(), clock.clone(), ids.clone())),
            evals: Arc::new(PgTriageEvalStore::new(p(), clock.clone(), ids.clone())),
            feed: PgChangeFeed::new(p(), clock.clone()),
            jobs: Arc::new(PgJobQueue::new(p(), clock.clone(), ids.clone())),
            repos: Arc::new(PgRepoStore::new(p(), clock.clone(), ids.clone())),
            knowledge: Arc::new(PgKnowledgeStore::new(p(), clock.clone(), ids.clone())),
            pools,
            clock,
            ids,
        })
    }

    fn vault(&self, config: &Config) -> Result<Arc<dyn SecretVault>, RunError> {
        config.vault.build(config.is_production(), self.secrets.clone(), self.clock.clone()).map_err(|e| startup("server.vault", e))
    }

    async fn org(&self, slug: &str) -> Result<Org, RunError> {
        self.orgs.org_by_slug(slug).await.map_err(AppError::from)?.ok_or_else(|| AppError::NotFound(format!("org {slug:?}")).into())
    }
}

/// The Active version of a Platform secret, `None` if it was never put.
async fn platform_secret(vault: &dyn SecretVault, name: &str) -> Result<Option<SecretBytes>, RunError> {
    let secret = SecretName::new(name).map_err(|e| startup(name, e))?;
    match vault.load(&SecretScope::Platform, &secret).await {
        Ok((_, value)) => Ok(Some(value)),
        Err(VaultError::NotFound) => Ok(None),
        Err(e) => Err(startup(&format!("platform secret {name}"), e)),
    }
}

/// A Platform secret the server cannot start without, as UTF-8.
async fn required_platform_secret(vault: &dyn SecretVault, name: &str) -> Result<String, RunError> {
    let value = platform_secret(vault, name)
        .await?
        .ok_or_else(|| RunError::Startup(format!("platform secret {name} is not set; put it with `mage admin secret put platform {name} --stdin`")))?;
    value.expose_str().map(str::to_owned).ok_or_else(|| RunError::Startup(format!("platform secret {name} is not UTF-8")))
}

async fn github_read_client(config: &Config, clock: Arc<dyn Clock>) -> Result<GitHubClient, RunError> {
    let pem = config.github.app_private_key.resolve().await.map_err(|e| startup("server.github.app_private_key", e))?;
    let app = AppCredentials { client_id: config.github.client_id.clone(), private_key_pem: pem.into_inner() };
    GitHubClient::new(&config.github.api, app, clock).map_err(|e| startup("GitHub read app", e))
}

fn identity_http() -> Result<reqwest::Client, RunError> {
    reqwest::Client::builder()
        .connect_timeout(IDENTITY_CONNECT_TIMEOUT)
        .timeout(IDENTITY_REQUEST_TIMEOUT)
        .build()
        .map_err(|e| startup("identity HTTP client", e))
}

/// GitHub login (and invite lookups) with the read app's OAuth client.
async fn github_identity(config: &Config, vault: &dyn SecretVault) -> Result<GitHubIdentity, RunError> {
    let secret = required_platform_secret(vault, names::GITHUB_OAUTH_CLIENT_SECRET).await?;
    Ok(config.login.github.build(&secret, identity_http()?))
}

/// Dev-only settings are refused in production.
fn check_environment(config: &Config) -> Result<(), RunError> {
    if config.is_production() {
        if config.gateway.mode == GatewayMode::HarnessLogin {
            return Err(RunError::Config("gateway.mode = \"harness-login\" is development only".into()));
        }
        if config.runs.credential_mode == CredentialModeKind::LocalLogin {
            return Err(RunError::Config("runs.credential_mode = \"local_login\" is development only".into()));
        }
    }
    Ok(())
}

/// Host names the Knowledge MCP accepts in `Host`; empty (rmcp's loopback-only default) when
/// the public URL is a loopback address.
fn public_hosts(url: &Url) -> Vec<String> {
    let Some(host) = url.host_str() else { return Vec::new() };
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if bare == "localhost" || bare.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback()) {
        return Vec::new();
    }
    let mut hosts = vec![host.to_owned()];
    if let Some(port) = url.port_or_known_default() {
        hosts.push(format!("{host}:{port}"));
    }
    hosts
}

// ---------------------------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------------------------

/// A built server: every adapter and use case wired, the listener bound, nothing serving yet.
pub struct Server {
    listener: TcpListener,
    app: Router,
    health: HealthState,
    shutdown: CancellationToken,
    pools: PgPools,
    feed: PgChangeFeed,
    signer: Arc<Ed25519TokenSigner>,
    hook_secrets: Option<Arc<HookSecrets>>,
    worker: Arc<Worker>,
}

impl Server {
    /// Builds everything and binds `config.bind`. Tasks started while building stop when
    /// `shutdown` is cancelled or the build fails.
    pub async fn build(config: Config, assets: Assets, shutdown: CancellationToken) -> Result<Server, RunError> {
        let shutdown = shutdown.child_token();
        let built = build(config, assets, shutdown.clone()).await;
        if built.is_err() {
            shutdown.cancel();
        }
        built
    }

    /// The bound address (`bind` with port 0 picks a free port).
    pub fn local_addr(&self) -> Result<SocketAddr, RunError> {
        self.listener.local_addr().map_err(RunError::Serve)
    }

    /// Serves until the shutdown token is cancelled, then drains (server.md "Shutdown").
    pub async fn serve(self) -> Result<(), RunError> {
        let Server { listener, app, health, shutdown, pools, feed, signer, hook_secrets, worker } = self;
        let mut tasks: Vec<JoinHandle<()>> = Vec::new();
        tasks.push(tokio::spawn({
            let shutdown = shutdown.clone();
            async move { feed.listen(shutdown).await }
        }));
        tasks.push(signer.spawn_reload_loop(RELOAD_EVERY, shutdown.clone()));
        if let Some(secrets) = &hook_secrets {
            tasks.push(secrets.spawn_refresh(SECRET_REFRESH_EVERY, shutdown.clone()));
        }
        tasks.push(spawn_wakeup_listener(pools.platform.clone(), JobLane::Server, worker.wake_handle(), shutdown.clone()));
        tasks.push(tokio::spawn(worker.run(shutdown.clone())));

        info!(addr = ?listener.local_addr().ok(), "mage server listening");
        let drain = {
            let shutdown = shutdown.clone();
            async move {
                shutdown.cancelled().await;
                health.mark_draining();
                info!("shutting down: not ready, draining connections");
            }
        };
        let served = axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).with_graceful_shutdown(drain).await;
        // A serve error ends the role too: stop every background loop.
        shutdown.cancel();
        for task in tasks {
            if let Err(e) = task.await {
                warn!(error = %e, "background task ended abnormally");
            }
        }
        pools.close().await;
        info!("mage server stopped");
        served.map_err(RunError::Serve)
    }
}

async fn build(config: Config, assets: Assets, shutdown: CancellationToken) -> Result<Server, RunError> {
    check_environment(&config)?;
    let ui = assets.bundle()?;
    let production = config.is_production();
    let public_url = config.public_url.clone();
    let policy = config.run_policy();
    let triage_config = config.triage_config();

    // Storage, vault, keys.
    let s = Stores::open(&config).await?;
    let (clock, ids) = (s.clock.clone(), s.ids.clone());
    let vault = s.vault(&config)?;
    EnsurePlatformKeys::new(vault.clone(), Actor::System).run().await?;
    let signer = Arc::new(Ed25519TokenSigner::new(vault.clone(), config.public_base()));
    signer.reload().await.map_err(|e| startup("signing key ring", e))?;
    let token_signer: Arc<dyn TokenSigner> = signer.clone();
    let token_service = Arc::new(TokenService::new(token_signer.clone(), s.tokens.clone(), clock.clone(), ids.clone()));
    let feed: Arc<dyn ChangeFeed> = Arc::new(s.feed.clone());
    let blobs = Arc::new(ObjectBlobs::from_config(&config.blob, &public_url, clock.clone()).await.map_err(|e| startup("server.blob", e))?);
    let artifacts: Arc<dyn ArtifactStore> = blobs.clone();

    // GitHub: read app (SourceHost, admin, deployments, login) and the optional Fix App.
    let read = github_read_client(&config, clock.clone()).await?;
    let source: Arc<dyn SourceHost> = Arc::new(GitHubSourceHost::new(read.clone()));
    let fix_client = match &config.github_fix {
        Some(fix) => {
            let pem = required_platform_secret(vault.as_ref(), GITHUB_FIX_APP_PRIVATE_KEY).await?;
            let app = AppCredentials { client_id: fix.client_id.clone(), private_key_pem: pem };
            Some(GitHubClient::new(&config.github.api, app, clock.clone()).map_err(|e| startup("GitHub Fix App", e))?)
        }
        None => None,
    };
    let admin_config = GithubAdminConfig { read_client_id: config.github.client_id.clone(), fix_client_id: config.github_fix.as_ref().map(|f| f.client_id.clone()) };
    let source_admin: Arc<dyn SourceHostAdmin> = Arc::new(GithubAdmin::new(admin_config, read.clone(), fix_client.clone(), vault.clone()));
    let github_deployments: Arc<dyn DeploymentResolver> = Arc::new(GithubDeploymentResolver::new(read.clone()));
    // Every PullRequests call first resolves the org's active Fix App installation; without
    // the Fix App there is none, so calls end in `PermissionDenied` before the client is used.
    let pr_client = fix_client.clone().unwrap_or_else(|| read.clone());
    let pr_config = config.github_fix.as_ref().map(|f| f.pull_requests.clone()).unwrap_or_default();
    let pull_requests: Arc<dyn PullRequests> =
        Arc::new(GitHubPullRequests::new(pr_client, s.orgs.clone(), clock.clone(), pr_config).map_err(|e| startup("GitHub pull requests", e))?);
    let github_login: Arc<dyn IdentityProvider> = Arc::new(github_identity(&config, vault.as_ref()).await?);
    let mut providers: Vec<Arc<dyn IdentityProvider>> = vec![github_login.clone()];
    if let Some(google) = &config.login.google {
        let secret = required_platform_secret(vault.as_ref(), names::GOOGLE_OAUTH_CLIENT_SECRET).await?;
        providers.push(Arc::new(google.build(&secret, identity_http()?)));
    }

    // Linear, Slack, model upstream, secret scanner, indexer link.
    let tracker: Arc<dyn TicketTracker> =
        Arc::new(LinearTracker::new(&config.linear, vault.clone(), s.orgs.clone(), clock.clone()).map_err(|e| startup("server.linear", e))?);
    let chat: Arc<dyn ChatNotifier> =
        Arc::new(SlackNotifier::new(config.slack.clone(), vault.clone(), s.orgs.clone(), clock.clone()).map_err(|e| startup("server.slack", e))?);
    let anthropic = AnthropicApi::new(&config.gateway.anthropic_base_url).map_err(|e| startup("gateway.anthropic_base_url", e))?;
    let vertex = match &config.gateway.vertex {
        Some(_) => {
            let tokens = AdcTokenSource::from_environment().map_err(|e| startup("gateway.vertex credentials", e))?;
            Some(VertexUpstream::new(tokens).map_err(|e| startup("gateway.vertex", e))?)
        }
        None => None,
    };
    let upstream: Arc<dyn ModelUpstream> = Arc::new(RoutedUpstream::new(anthropic, vertex));
    let scanner: Arc<dyn SecretScanner> = Arc::new(RegexSecretScanner::new().map_err(|e| startup("secret scanner", e))?);
    let indexer: Arc<dyn IndexerLink> = Arc::new(
        HttpIndexerLink::new(config.knowledge.indexer_url.clone(), indexer_token_minter(token_service.clone()), config.knowledge.indexer_timeout())
            .map_err(|e| startup("server.knowledge.indexer_url", e))?,
    );

    // Knowledge query side.
    let reader = SourceReader::new(s.knowledge.clone(), artifacts.clone());
    let retriever = Arc::new(Retriever::new(RetrieverDeps {
        knowledge: s.knowledge.clone(),
        repos: s.repos.clone(),
        tickets: s.tickets.clone(),
        indexer: indexer.clone(),
        weights: config.knowledge.retrieval.clone(),
    }));
    let knowledge_service = Arc::new(Service::new(ServiceDeps {
        knowledge: s.knowledge.clone(),
        repos: s.repos.clone(),
        tickets: s.tickets.clone(),
        runs: s.runs.clone(),
        indexer: indexer.clone(),
        source: reader.clone(),
    }));
    let knowledge_auth = Arc::new(AuthorizeKnowledge::new(token_service.clone(), s.runs.clone(), clock.clone()));

    // Run kinds' hooks, then the run manager (fix binds it late: its hooks call back into it).
    let fix_deps = Arc::new(FixDeps {
        tickets: s.tickets.clone(),
        runs: s.runs.clone(),
        repos: s.repos.clone(),
        orgs: s.orgs.clone(),
        knowledge: s.knowledge.clone(),
        reader: reader.clone(),
        artifacts: artifacts.clone(),
        jobs: s.jobs.clone(),
        pull_requests,
        scanner,
        run_manager: LateRunManager::new(),
        clock: clock.clone(),
        ids: ids.clone(),
        config: config.fix.fix_config(public_url.clone()),
    });
    let shared_reader = Arc::new(reader.clone());
    let hooks = RunHooks {
        triage_gate: Arc::new(EvidenceGate::new(s.knowledge.clone(), shared_reader.clone())),
        triage_outcome: Arc::new(ApplyTriageOutcome::new(
            ApplyTriageDeps {
                tickets: s.tickets.clone(),
                runs: s.runs.clone(),
                knowledge: s.knowledge.clone(),
                reader: shared_reader,
                evals: s.evals.clone(),
                jobs: s.jobs.clone(),
                clock: clock.clone(),
                ids: ids.clone(),
            },
            triage_config.clone(),
        )),
        fix_gate: Arc::new(AcceptFixResult::new(fix_deps.clone())),
        fix_outcome: Arc::new(ApplyFixOutcome::new(fix_deps.clone())),
    };
    let signals: Arc<dyn RunSignals> = Arc::new(PgRunSignals::start(s.pools.platform.clone(), shutdown.clone()));
    let run_manager = RunManager::new(RunDeps {
        runs: s.runs.clone(),
        signals,
        tickets: s.tickets.clone(),
        orgs: s.orgs.clone(),
        repos: s.repos.clone(),
        artifacts: artifacts.clone(),
        jobs: s.jobs.clone(),
        token_store: s.tokens.clone(),
        signer: token_signer.clone(),
        audit: s.audit.clone(),
        clock: clock.clone(),
        ids: ids.clone(),
        policy,
        hooks,
        shutdown: shutdown.clone(),
    });
    fix_deps.run_manager.set(run_manager.clone());

    // Triage.
    let start_triage = Arc::new(StartTriage::new(
        StartTriageDeps {
            tickets: s.tickets.clone(),
            runs: s.runs.clone(),
            run_manager: run_manager.clone(),
            repos: s.repos.clone(),
            orgs: s.orgs.clone(),
            retriever: retriever.clone(),
            jobs: s.jobs.clone(),
            clock: clock.clone(),
            ids: ids.clone(),
        },
        triage_config.clone(),
    ));
    let eval_case = Arc::new(StartEvalCase::new(
        StartEvalCaseDeps {
            tickets: s.tickets.clone(),
            runs: s.runs.clone(),
            run_manager: run_manager.clone(),
            repos: s.repos.clone(),
            orgs: s.orgs.clone(),
            retriever,
            evals: s.evals.clone(),
            jobs: s.jobs.clone(),
            clock: clock.clone(),
            ids: ids.clone(),
        },
        triage_config.clone(),
    ));
    let retriage = Arc::new(RequestRetriage::new(s.tickets.clone(), s.runs.clone(), s.orgs.clone(), s.jobs.clone(), clock.clone(), triage_config));

    // Intake.
    let github_ingest = Arc::new(GithubIngest::new(s.orgs.clone(), s.repos.clone(), s.tickets.clone(), source.clone(), s.jobs.clone(), clock.clone()));
    let lifecycle = Arc::new(IntegrationEvents::new(
        s.orgs.clone(),
        s.repos.clone(),
        source.clone(),
        source_admin.clone(),
        vault.clone(),
        github_ingest.clone(),
        s.jobs.clone(),
        clock.clone(),
        ids.clone(),
    ));
    let identities = Arc::new(TrackerIdentityCache::new(tracker.clone(), clock.clone()));
    let decoders: Vec<Arc<dyn WebhookDecoder>> = vec![
        Arc::new(GithubDecoder::new(WebhookSource::GitHub)),
        Arc::new(GithubDecoder::new(WebhookSource::GitHubFix)),
        Arc::new(LinearDecoder::new()),
        Arc::new(SlackDecoder::new(false)),
    ];
    let deliver = Arc::new(ProcessDelivery::new(
        s.inbox.clone(),
        decoders,
        s.orgs.clone(),
        s.repos.clone(),
        s.jobs.clone(),
        identities.clone(),
        github_ingest.clone(),
        lifecycle.clone(),
        clock.clone(),
    ));
    let chain = Arc::new(DeploymentChain::new(vec![Arc::new(PgDeploymentResolver::new(s.repos.clone())), github_deployments.clone()]));
    let targets = Arc::new(TargetResolver::new(source.clone(), s.repos.clone(), chain));
    let ingest = Arc::new(IngestLinearIssue::new(
        s.orgs.clone(),
        s.tickets.clone(),
        s.repos.clone(),
        tracker.clone(),
        chat.clone(),
        s.outbound.clone(),
        targets,
        identities,
        clock.clone(),
        ids.clone(),
    ));
    let reconcile = Arc::new(Reconcile::new(
        s.orgs.clone(),
        s.repos.clone(),
        s.tickets.clone(),
        s.inbox.clone(),
        s.jobs.clone(),
        source.clone(),
        source_admin.clone(),
        tracker.clone(),
        github_deployments,
        github_ingest,
        lifecycle,
        clock.clone(),
        config.intake.mode,
        config.github_fix.is_some(),
    ));
    let notify = Arc::new(Notify::new(
        s.orgs.clone(),
        s.tickets.clone(),
        s.runs.clone(),
        s.repos.clone(),
        s.outbound.clone(),
        tracker.clone(),
        chat.clone(),
        artifacts.clone(),
        Arc::new(MediaCache::new(artifacts.clone())),
        clock.clone(),
        ids.clone(),
        public_url.clone(),
    ));
    let bind = Arc::new(BindIntegration::new(s.orgs.clone(), source_admin, tracker, chat, vault.clone(), clock.clone(), config.connect_config()));

    // Start-up seeding (idempotent on every instance) and the reaper chain.
    let seeded = seed(s.orgs.as_ref(), &config.intake, &config.github.installations).await?;
    run_manager.ensure_reaper().await?;

    // Job worker.
    let mut worker = Worker::new(s.jobs.clone(), clock.clone(), config.jobs.clone(), worker_id());
    let rotate = Arc::new(RotateSigningKey::new(vault.clone(), token_signer.clone(), s.audit.clone(), clock.clone(), RotationPolicy::default()));
    let handlers: [Arc<dyn crate::adapters::driving::worker::JobHandler>; 5] = [
        Arc::new(PlatformJobs::new(rotate, Arc::new(RewrapSecrets::new(vault.clone())), s.tokens.clone(), clock.clone())),
        Arc::new(IntakeJobs::new(deliver, ingest, reconcile.clone(), notify)),
        Arc::new(TriageJobs::new(start_triage, eval_case)),
        Arc::new(RunJobs::new(run_manager.clone())),
        Arc::new(FixJobs::new(fix_deps.clone())),
    ];
    for handler in handlers {
        worker.register(handler).map_err(|e| startup("job worker", e))?;
    }
    for ticker in platform_tickers() {
        worker.add_periodic(ticker);
    }
    worker.add_periodic(Arc::new(IntakeChains::new(reconcile, seeded)));

    // Platform HTTP.
    let sessions = Arc::new(Sessions::new(s.tokens.clone(), vault.clone(), clock.clone(), config.session));
    let login = Arc::new(Login::new(
        providers,
        s.tokens.clone(),
        s.orgs.clone(),
        sessions.clone(),
        s.audit.clone(),
        clock.clone(),
        ids.clone(),
        LoginConfig { public_url: public_url.clone() },
    ));
    let directory = Arc::new(Directory::new(s.orgs.clone(), s.tickets.clone(), s.runs.clone()));
    let orgs_api = OrgsApiState {
        sessions: sessions.clone(),
        login: login.clone(),
        directory: directory.clone(),
        create: Arc::new(CreateOrg::new(s.orgs.clone(), Some(github_login.clone()), s.audit.clone(), clock.clone(), ids.clone())),
        public_org_create: false,
        members: Arc::new(Members::new(s.orgs.clone(), github_login, s.audit.clone(), clock.clone(), ids.clone())),
        settings: Arc::new(OrgSettingsService::new(s.orgs.clone(), s.audit.clone(), config.harness, production)),
        model: Arc::new(ModelSettingsService::new(s.orgs.clone(), vault.clone(), upstream.clone(), s.audit.clone(), config.gateway.clone())),
        integrations: Arc::new(Integrations::new(s.orgs.clone(), s.tokens.clone(), bind, s.audit.clone(), clock.clone(), public_url.clone())),
        activity: Arc::new(Activity::new(s.audit.clone(), s.runs.clone())),
        features: Vec::new(),
    };
    let guard = SessionGuard { sessions: sessions.clone(), directory, public_origin: origin_of(&public_url) };
    let probes: Vec<Arc<dyn ReadyProbe>> = vec![
        Arc::new(PgReady::new(s.pools.platform.clone())),
        Arc::new(FnProbe::new("signing keys", {
            let signer = signer.clone();
            move || signer.is_ready()
        })),
        Arc::new(FnProbe::new("org feed", {
            let feed = s.feed.clone();
            move || feed.listening()
        })),
    ];
    let health = HealthState::new(probes);
    let platform = PlatformHttp {
        public_url: public_url.clone(),
        guard: guard.clone(),
        auth: AuthState { login, sessions: sessions.clone(), guard, clock: clock.clone() },
        orgs_api,
        feed: FeedState { feed: feed.clone(), sessions, shutdown: shutdown.clone(), timings: SseTimings::default() },
        health: health.clone(),
        signer: token_signer.clone(),
        ui,
        trace: None,
        limits: RateLimits::default(),
    };

    // Area routers.
    let mut areas = AreaRouters::default();
    areas.api.push(api_runs::router(RunsApiState::new(run_manager.clone())));
    areas.api.push(api_tickets::router(TicketsHttpState {
        tickets: Arc::new(TicketsApi::new(s.tickets.clone(), s.repos.clone(), s.orgs.clone(), s.runs.clone(), source.clone(), clock.clone())),
    }));
    areas.api.push(api_triage::router(TriageHttpState { retriage }));
    areas.api.push(api_repos::router(ReposState {
        service: Arc::new(ReposService::new(s.repos.clone(), s.orgs.clone(), s.runs.clone(), source, s.jobs.clone(), clock.clone(), ids.clone())),
    }));
    areas.api.push(api_fix::router(FixApiState::new(fix_deps)));
    areas.events.push(run_sse::router(RunSseState::new(run_manager.clone())));
    areas.token.push(runner::router(RunnerApiState::new(run_manager.clone())));
    areas.token.push(
        mcp::router(KnowledgeMcpState {
            auth: knowledge_auth,
            service: knowledge_service,
            limits: config.knowledge.mcp,
            allowed_hosts: public_hosts(&public_url),
            shutdown: shutdown.clone(),
        })
        .map_err(|e| startup("knowledge MCP", e))?,
    );
    let hook_secrets = match config.intake.mode {
        IntakeMode::Webhook => {
            let secrets = Arc::new(HookSecrets::load(vault.clone()).await.map_err(|e| startup("webhooks", e))?);
            let accept = Arc::new(AcceptDelivery::new(s.inbox.clone()));
            areas.token.push(hooks::router(HooksState::new(accept, secrets.clone(), clock.clone(), config.intake.linear_max_skew())));
            Some(secrets)
        }
        // No public URL: providers cannot push, intake polls.
        IntakeMode::Poll => None,
    };
    if config.blob.backend == BlobBackend::Local {
        areas.token.push(mage_infra::blob::local::dev_routes(blobs).map_err(|e| startup("dev blob routes", e))?);
    }
    let forward = Arc::new(Forward::new(
        GatewayDeps {
            tokens: token_service,
            runs: s.runs.clone(),
            orgs: s.orgs.clone(),
            vault,
            upstream,
            meter: Arc::new(run_manager),
            audit: s.audit.clone(),
            feed,
            clock,
        },
        config.gateway.clone(),
    )?);
    areas.nested.push(("/llm/anthropic", llm::router(GatewayState::new(forward, shutdown.clone()))));
    let app = router(platform, areas);

    let listener = TcpListener::bind(config.bind).await.map_err(|source| RunError::Bind { addr: config.bind, source })?;
    Ok(Server {
        listener,
        app,
        health,
        shutdown,
        pools: s.pools,
        feed: s.feed,
        signer,
        hook_secrets,
        worker: Arc::new(worker),
    })
}

/// `jobs.leased_by`: host and process.
fn worker_id() -> String {
    let host = std::env::var("HOSTNAME").ok().filter(|h| !h.is_empty()).unwrap_or_else(|| "mage-server".to_owned());
    format!("{host}:{}", std::process::id())
}

// ---------------------------------------------------------------------------------------------
// Operator entry points (`mage …` CLI)
// ---------------------------------------------------------------------------------------------

/// `mage migrate`: applies every pending migration (no statement timeout).
pub async fn migrate(config: &Config) -> Result<(), RunError> {
    let mut database = config.database.clone();
    database.statement_timeout_ms = 0;
    let pools = connect(&database).await.map_err(|e| RunError::Database(e.to_string()))?;
    let migrated = mage_infra::pg::schema::migrate(&pools.platform).await.map_err(|e| RunError::Database(format!("migrate: {e}")));
    pools.close().await;
    migrated
}

/// `mage jobs dead list [--kind]`.
pub async fn jobs_dead_list(config: &Config, kind: Option<JobKind>, page: PageRequest) -> Result<Page<JobRecord>, RunError> {
    let s = Stores::open(config).await?;
    let listed = s.jobs.list_dead(kind, page).await.map_err(|e| RunError::Database(e.to_string()));
    s.pools.close().await;
    listed
}

/// `mage jobs dead retry <id>`.
pub async fn jobs_dead_retry(config: &Config, job: JobId) -> Result<(), RunError> {
    let s = Stores::open(config).await?;
    let retried = s.jobs.retry_dead(job).await.map_err(|e| RunError::Database(e.to_string()));
    s.pools.close().await;
    retried
}

/// `mage jobs dead discard <id>`.
pub async fn jobs_dead_discard(config: &Config, job: JobId) -> Result<(), RunError> {
    let s = Stores::open(config).await?;
    let discarded = s.jobs.discard_dead(job).await.map_err(|e| RunError::Database(e.to_string()));
    s.pools.close().await;
    discarded
}

/// `mage admin org create`: the org and its first Owner, a GitHub login resolved to its stable id
/// (server.md "Provisioning").
pub async fn admin_org_create(config: &Config, slug: &str, name: &str, owner_github_login: &str, operator: &str) -> Result<Org, RunError> {
    let s = Stores::open(config).await?;
    let created: Result<Org, RunError> = async {
        let vault = s.vault(config)?;
        let github: Arc<dyn IdentityProvider> = Arc::new(github_identity(config, vault.as_ref()).await?);
        let create = CreateOrg::new(s.orgs.clone(), Some(github), s.audit.clone(), s.clock.clone(), s.ids.clone());
        let actor = Actor::Operator { os_user: operator.to_owned() };
        Ok(create.create(slug, name, OrgOwner::GitHubLogin(owner_github_login.to_owned()), &actor, &RequestMeta::default()).await?)
    }
    .await;
    s.pools.close().await;
    created
}

/// `mage admin secret put <platform|org:slug> <name>`: a new Active version.
pub async fn admin_secret_put(config: &Config, scope: &str, name: &str, value: Vec<u8>, operator: &str) -> Result<SecretMeta, RunError> {
    let s = Stores::open(config).await?;
    let put: Result<SecretMeta, RunError> = async {
        let scope = match scope.strip_prefix("org:") {
            Some(slug) => SecretScope::Org(s.org(slug).await?.id),
            None if scope == "platform" => SecretScope::Platform,
            None => return Err(AppError::Invalid(format!("secret scope must be `platform` or `org:<slug>`, got {scope:?}")).into()),
        };
        let put = PutSecret::new(s.vault(config)?, s.audit.clone());
        Ok(put.put(scope, name, SecretBytes::new(value), &Actor::Operator { os_user: operator.to_owned() }).await?)
    }
    .await;
    s.pools.close().await;
    put
}

/// `mage admin runner issue --name <name> [--scope org,…]`: a one-time bootstrap token for a
/// runner that leases runs of `org_slugs` (empty: every org).
pub async fn runner_bootstrap_token_create(config: &Config, name: &str, org_slugs: &[String], operator: &str) -> Result<IssuedBootstrapToken, RunError> {
    let s = Stores::open(config).await?;
    let issued: Result<IssuedBootstrapToken, RunError> = async {
        let scope = if org_slugs.is_empty() {
            RunnerScope::Shared
        } else {
            let mut orgs: Vec<OrgId> = Vec::with_capacity(org_slugs.len());
            for slug in org_slugs {
                orgs.push(s.org(slug).await?.id);
            }
            RunnerScope::Orgs(orgs)
        };
        let issue = IssueRunnerBootstrap::new(s.tokens.clone(), s.audit.clone(), s.clock.clone());
        Ok(issue.issue(name, scope, &Actor::Operator { os_user: operator.to_owned() }).await?)
    }
    .await;
    s.pools.close().await;
    issued
}

/// The org's first Owner: the recorded author of operator-started evals.
async fn org_owner(orgs: &dyn OrgStore, org: OrgId) -> Result<UserId, RunError> {
    let mut page = PageRequest::first(PageRequest::MAX_LIMIT);
    loop {
        let members = orgs.members(org, page.clone()).await.map_err(AppError::from)?;
        if let Some((user, _)) = members.items.iter().find(|(_, m)| m.role == MemberRole::Owner) {
            return Ok(user.id);
        }
        match members.next {
            Some(next) => page = page.after(next),
            None => return Err(AppError::Invalid("org has no owner".into()).into()),
        }
    }
}

/// `mage eval triage run`: a `TriageEval` over the org's labeled tickets; the server's job
/// worker runs its cases.
#[allow(clippy::too_many_arguments)]
pub async fn eval_triage_run(
    config: &Config,
    org_slug: &str,
    corpus: &str,
    variant_toml: &str,
    repeat: u8,
    limit: Option<u32>,
    concurrency: Option<u16>,
) -> Result<TriageEval, RunError> {
    let s = Stores::open(config).await?;
    let started: Result<TriageEval, RunError> = async {
        let org = s.org(org_slug).await?.id;
        let created_by = org_owner(s.orgs.as_ref(), org).await?;
        let start = StartTriageEval::new(s.tickets.clone(), s.runs.clone(), s.evals.clone(), s.jobs.clone());
        let req = StartTriageEvalRequest { corpus: corpus.to_owned(), variant_toml: variant_toml.to_owned(), repeat, limit, concurrency, created_by };
        Ok(start.run(org, req).await?)
    }
    .await;
    s.pools.close().await;
    started
}

/// `mage eval triage label --auto`: ground truth from fix PRs and confirmed causes.
pub async fn eval_triage_label_auto(config: &Config, org_slug: &str) -> Result<Vec<GroundTruth>, RunError> {
    let s = Stores::open(config).await?;
    let labeled: Result<Vec<GroundTruth>, RunError> = async {
        let org = s.org(org_slug).await?.id;
        Ok(LabelTriageEval::new(s.tickets.clone(), s.knowledge.clone(), s.evals.clone()).auto(org).await?)
    }
    .await;
    s.pools.close().await;
    labeled
}

/// `mage eval triage label <ticket> <paths…>`: a manual (`Strong`) label.
pub async fn eval_triage_label_manual(config: &Config, org_slug: &str, ticket: TicketId, paths: BTreeSet<RepoPath>) -> Result<GroundTruth, RunError> {
    let s = Stores::open(config).await?;
    let labeled: Result<GroundTruth, RunError> = async {
        let org = s.org(org_slug).await?.id;
        Ok(LabelTriageEval::new(s.tickets.clone(), s.knowledge.clone(), s.evals.clone()).put_manual(org, ticket, paths).await?)
    }
    .await;
    s.pools.close().await;
    labeled
}

/// `mage eval triage report <eval>`.
pub async fn eval_triage_report(config: &Config, org_slug: &str, eval: TriageEvalId) -> Result<TriageEvalReport, RunError> {
    let s = Stores::open(config).await?;
    let report: Result<TriageEvalReport, RunError> = async {
        let org = s.org(org_slug).await?.id;
        Ok(TriageEvalReporter::new(s.runs.clone(), s.evals.clone()).run(org, eval).await?)
    }
    .await;
    s.pools.close().await;
    report
}

/// `mage knowledge eval`: replays the org's resolved tickets through the retriever
/// (knowledge.md "Eval"). Indexer calls are signed with the server's key ring.
pub async fn knowledge_eval(config: &Config, org_slug: &str, opts: EvalOptions) -> Result<KnowledgeEvalReport, RunError> {
    let s = Stores::open(config).await?;
    let report: Result<KnowledgeEvalReport, RunError> = async {
        let org = s.org(org_slug).await?.id;
        let signer = Arc::new(Ed25519TokenSigner::new(s.vault(config)?, config.public_base()));
        signer.reload().await.map_err(|e| startup("signing key ring", e))?;
        if !signer.is_ready() {
            return Err(RunError::Startup("no active signing key; start the server once to create it".into()));
        }
        let tokens = Arc::new(TokenService::new(signer, s.tokens.clone(), s.clock.clone(), s.ids.clone()));
        let indexer: Arc<dyn IndexerLink> = Arc::new(
            HttpIndexerLink::new(config.knowledge.indexer_url.clone(), indexer_token_minter(tokens), config.knowledge.indexer_timeout())
                .map_err(|e| startup("server.knowledge.indexer_url", e))?,
        );
        let retriever = Arc::new(Retriever::new(RetrieverDeps {
            knowledge: s.knowledge.clone(),
            repos: s.repos.clone(),
            tickets: s.tickets.clone(),
            indexer,
            weights: config.knowledge.retrieval.clone(),
        }));
        let eval = KnowledgeEval::new(retriever, s.knowledge.clone(), s.repos.clone(), s.tickets.clone());
        Ok(eval.run(org, &opts).await?)
    }
    .await;
    s.pools.close().await;
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_hosts_are_explicit_except_for_loopback() {
        let hosts = |u: &str| public_hosts(&Url::parse(u).unwrap_or_else(|e| panic!("{u}: {e}")));
        assert!(hosts("http://localhost:8080").is_empty());
        assert!(hosts("http://127.0.0.1:8080").is_empty());
        assert!(hosts("http://[::1]:8080").is_empty());
        assert_eq!(hosts("https://mage.example.com"), vec!["mage.example.com".to_owned(), "mage.example.com:443".to_owned()]);
        assert_eq!(hosts("http://10.0.0.5:8080"), vec!["10.0.0.5".to_owned(), "10.0.0.5:8080".to_owned()]);
    }
}
