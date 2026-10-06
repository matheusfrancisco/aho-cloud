//! Composition root of the indexer role (`mage start indexer`; README "Inside a role crate",
//! knowledge.md "Use cases" and "Text search, blame, and diff on Cloud Run").
//!
//! Builds the Postgres stores, the blob store, the GitHub read-app `SourceHost`, the git CLI,
//! tree-sitter, SCIP and ripgrep adapters, wires the `index.*` use cases into the job worker
//! (woken by `LISTEN mage_jobs`), and serves `/internal/v1` on `indexer.internal_listen` until
//! `shutdown` fires. The worker gets `workers.shutdown_grace_secs` to finish in-flight jobs.

use std::sync::Arc;

use mage_core::domain::JobLane;
use mage_core::ports::{ArtifactStore, Clock, IdGen, JobQueue, KnowledgeStore, RepoStore, SourceHost, SystemClock, UuidV7Gen};
use mage_infra::blob::ObjectBlobs;
use mage_infra::github::{AppCredentials, GitHubClient, GitHubSourceHost};
use mage_infra::pg::jobs::spawn_wakeup_listener;
use mage_infra::pg::pool::{SchemaStatus, check_schema, connect};
use mage_infra::pg::{PgJobQueue, PgKnowledgeStore, PgRepoStore};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::Config;
use crate::adapters::driven::git::GitCli;
use crate::adapters::driven::scip::ScipFileReader;
use crate::adapters::driven::search::RipgrepSearch;
use crate::adapters::driven::treesitter::TreeSitterParser;
use crate::adapters::driving::http::auth::{HttpKeySource, ServiceAuth};
use crate::adapters::driving::http::router;
use crate::adapters::driving::worker::{IndexHandlers, IndexWorker};
use crate::app::index::appmap::IndexAppMap;
use crate::app::index::history::IndexHistory;
use crate::app::index::ingest_scip::IngestScip;
use crate::app::index::{IndexDeps, IndexPipeline};
use crate::app::live::LiveService;
use crate::ports::git::GitRepoReader;
use crate::ports::parser::Parser;

/// Why the indexer could not start or stopped early.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("config: {0}")]
    Config(String),
    #[error("database: {0}")]
    Database(String),
    #[error("blob store: {0}")]
    Blob(String),
    #[error("github: {0}")]
    GitHub(String),
    #[error("git: {0}")]
    Git(String),
    #[error("parser: {0}")]
    Parser(String),
    #[error("internal api: {0}")]
    Http(String),
}

/// Runs the indexer until `shutdown` is cancelled.
pub async fn run(config: Config, shutdown: CancellationToken) -> Result<(), RunError> {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let ids: Arc<dyn IdGen> = Arc::new(UuidV7Gen);

    let issuer = Url::parse(&config.internal_auth.issuer).map_err(|e| RunError::Config(format!("indexer.internal_auth.issuer: {e}")))?;
    let keys_url = config.internal_auth.keys_url().map_err(|e| RunError::Config(format!("indexer.internal_auth.keys_url: {e}")))?;

    let pools = connect(&config.database).await.map_err(|e| RunError::Database(e.to_string()))?;
    match check_schema(&pools.platform).await.map_err(|e| RunError::Database(e.to_string()))? {
        SchemaStatus::Current => {}
        SchemaStatus::Ahead { unknown } => tracing::warn!(?unknown, "database has migrations this binary does not know"),
    }
    let jobs: Arc<dyn JobQueue> = Arc::new(PgJobQueue::new(pools.clone(), clock.clone(), ids.clone()));
    let repos: Arc<dyn RepoStore> = Arc::new(PgRepoStore::new(pools.clone(), clock.clone(), ids.clone()));
    let knowledge: Arc<dyn KnowledgeStore> = Arc::new(PgKnowledgeStore::new(pools.clone(), clock.clone(), ids.clone()));

    let artifacts: Arc<dyn ArtifactStore> =
        Arc::new(ObjectBlobs::from_config(&config.blob, &issuer, clock.clone()).await.map_err(|e| RunError::Blob(e.to_string()))?);

    let pem = config.github.app_private_key.resolve().await.map_err(|e| RunError::GitHub(format!("indexer.github.app_private_key: {e}")))?;
    let credentials = AppCredentials { client_id: config.github.client_id.clone(), private_key_pem: pem.into_inner() };
    let client = GitHubClient::new(&config.github.api, credentials, clock.clone()).map_err(|e| RunError::GitHub(e.to_string()))?;
    let source: Arc<dyn SourceHost> = Arc::new(GitHubSourceHost::new(client));

    let git: Arc<dyn GitRepoReader> = Arc::new(GitCli::new(config.git_config()).map_err(|e| RunError::Git(e.to_string()))?);
    let parser: Arc<dyn Parser> = Arc::new(TreeSitterParser::new(config.treesitter.clone()).map_err(|e| RunError::Parser(e.to_string()))?);

    let live = Arc::new(LiveService::new(repos.clone(), knowledge.clone(), git.clone(), Arc::new(RipgrepSearch::new()), &config.live_config()));
    let deps = IndexDeps {
        repos: repos.clone(),
        jobs: jobs.clone(),
        knowledge: knowledge.clone(),
        artifacts: artifacts.clone(),
        source: source.clone(),
        git: git.clone(),
        parser: parser.clone(),
        clock: clock.clone(),
        ids: ids.clone(),
    };
    let pipeline = Arc::new(IndexPipeline::new(deps, config.index_config()).with_checkouts(live.clone()));
    let app_map = Arc::new(IndexAppMap::new(repos.clone(), knowledge.clone(), git.clone(), parser, clock.clone()));
    let history = Arc::new(IndexHistory::new(repos.clone(), knowledge.clone(), git, source, clock.clone()));
    let ingest_scip = Arc::new(IngestScip::new(repos, knowledge, artifacts, Arc::new(ScipFileReader::new()), clock.clone(), &config.data_dir));
    let handlers = Arc::new(IndexHandlers::new(pipeline, app_map, history, ingest_scip));

    let key_source = HttpKeySource::new(keys_url).map_err(|e| RunError::Config(e.to_string()))?;
    let auth = Arc::new(ServiceAuth::new(config.internal_auth.issuer.trim_end_matches('/'), Arc::new(key_source), clock));
    let listener = tokio::net::TcpListener::bind(config.internal_listen)
        .await
        .map_err(|e| RunError::Http(format!("bind {}: {e}", config.internal_listen)))?;

    // Everything below stops on `shutdown`, or early when the internal API fails.
    let stop = shutdown.child_token();
    let wake = Arc::new(Notify::new());
    let wakeups = spawn_wakeup_listener(pools.platform.clone(), JobLane::Indexer, wake.clone(), stop.clone());
    let worker_name = config.worker_name.clone().or_else(|| std::env::var("HOSTNAME").ok()).unwrap_or_else(|| "indexer".to_owned());
    let worker_id = format!("{worker_name}-{}", &ids.uuid_v4().simple().to_string()[..8]);
    let worker = Arc::new(IndexWorker::new(jobs, handlers, worker_id, config.workers.clone()).with_wakeup(wake));
    let worker_task = tokio::spawn(worker.run(stop.clone()));

    tracing::info!(listen = %config.internal_listen, data_dir = %config.data_dir.display(), "indexer started");
    let served = axum::serve(listener, router(live, auth)).with_graceful_shutdown(stop.clone().cancelled_owned()).await;
    if let Err(err) = &served {
        tracing::error!(error = %err, "internal api stopped; shutting the indexer down");
    }
    stop.cancel();
    if let Err(err) = worker_task.await {
        tracing::error!(error = %err, "indexer worker task failed");
    }
    if let Err(err) = wakeups.await {
        tracing::warn!(error = %err, "job wakeup listener task failed");
    }
    pools.close().await;
    tracing::info!("indexer stopped");
    served.map_err(|e| RunError::Http(e.to_string()))
}
