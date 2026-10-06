//! Composition root of the runner role (`mage start runner`; README "Inside a role crate",
//! runs.md "Startup and recovery", executor.md "Host adapter", harness.md).
//!
//! Opens the SQLite `RunnerState`, the v1 host executor, the HTTP `ServerLink` (bootstrap token
//! only until the credential file holds a runner id), the harness adapters whose binaries exist
//! on this host, Playwright, the Run MCP host, the Verify and SCIP steps, then runs the lease loop
//! (recovery + register + lease) until `shutdown`, draining running tasks for
//! `daemon.drain_grace_secs`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use mage_core::ports::{Clock, IdGen, SystemClock, UuidV7Gen};
use tokio_util::sync::CancellationToken;

use crate::Config;
use crate::adapters::driven::exec_host::{HostDeps, HostExecutor};
use crate::adapters::driven::harness_claude::ClaudeHarness;
use crate::adapters::driven::harness_omp::OmpHarness;
use crate::adapters::driven::link::{Credentials, HttpServerLink};
use crate::adapters::driven::playwright::PlaywrightRunner;
use crate::adapters::driven::runner_state::SqliteRunnerState;
use crate::adapters::driving::lease_loop::LeaseLoop;
use crate::adapters::driving::mcp::RunMcpServer;
use crate::app::runner::daemon::Daemon;
use crate::app::runner::scip::ScipStep;
use crate::app::runner::task::RunnerDeps;
use crate::app::runner::verify::VerifyRunner;
use crate::config::{POSTGRES_PASSWORD_ENV, SecretRef};
use crate::error::AppError;
use crate::ports::browser::BrowserRunner;
use crate::ports::executor::Executor;
use crate::ports::harness::Harness;
use crate::ports::link::ServerLink;
use crate::ports::run_mcp::RunMcpHost;
use crate::ports::runner_state::RunnerState;

/// Why the runner could not start or stopped.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("config: {0}")]
    Config(String),
    #[error("runner state: {0}")]
    State(String),
    #[error("executor: {0}")]
    Executor(String),
    #[error("server link: {0}")]
    Link(String),
    /// The lease loop ended: revoked, rejected, or startup (recovery / register) failed.
    #[error("runner stopped: {0}")]
    Stopped(#[from] AppError),
}

/// Runs the runner until `shutdown` is cancelled (or the server revokes it).
pub async fn run(config: Config, shutdown: CancellationToken) -> Result<(), RunError> {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let ids: Arc<dyn IdGen> = Arc::new(UuidV7Gen);

    let mut executor_config = config.executor.clone();
    executor_config.postgres.password = match &config.postgres_password {
        Some(source) => Some(source.resolve().map_err(|e| RunError::Config(format!("runner.postgres.password: {e}")))?),
        None => std::env::var(POSTGRES_PASSWORD_ENV).ok().filter(|v| !v.is_empty()),
    };
    let isolation = executor_config.isolation;

    if let Some(dir) = config.state_file.parent() {
        std::fs::create_dir_all(dir).map_err(|e| RunError::State(format!("create {}: {e}", dir.display())))?;
    }
    let state: Arc<dyn RunnerState> =
        Arc::new(SqliteRunnerState::open(&config.state_file, clock.clone()).map_err(|e| RunError::State(format!("{e:?}")))?);
    let host = HostExecutor::open(executor_config, HostDeps { state: state.clone(), clock: clock.clone(), ids: ids.clone() })
        .await
        .map_err(|e| RunError::Executor(e.to_string()))?;
    let executor: Arc<dyn Executor> = host;

    let mut link_config = config.link.clone();
    link_config.bootstrap_token = bootstrap_token(&config.link.credential_file, config.bootstrap_token.as_ref())?;
    let http_link = HttpServerLink::new(link_config, env!("CARGO_PKG_VERSION").to_owned(), clock.clone(), ids.clone())
        .map_err(|e| RunError::Link(e.to_string()))?;
    let link: Arc<dyn ServerLink> = Arc::new(http_link);

    let mut harnesses: Vec<Arc<dyn Harness>> = Vec::new();
    match installed(&config.harness.omp.binary) {
        Some(path) => {
            tracing::info!(binary = %path.display(), "omp harness installed");
            harnesses.push(Arc::new(OmpHarness::new(config.harness.clone(), config.models.clone(), isolation)));
        }
        None => tracing::info!(binary = %config.harness.omp.binary, "omp harness not installed: binary not found"),
    }
    match installed(&config.harness.claude.binary) {
        Some(path) => {
            tracing::info!(binary = %path.display(), "claude harness installed");
            harnesses.push(Arc::new(ClaudeHarness::new(config.harness.clone(), isolation)));
        }
        None => tracing::info!(binary = %config.harness.claude.binary, "claude harness not installed: binary not found"),
    }
    if harnesses.is_empty() {
        tracing::warn!("no harness binary found: this runner only takes Verify runs");
    }

    let browser: Arc<dyn BrowserRunner> = Arc::new(PlaywrightRunner::new(config.playwright.clone()));
    let mcp_host: Arc<dyn RunMcpHost> = Arc::new(RunMcpServer::new(state.clone(), clock.clone(), config.run_mcp));
    let verify = Arc::new(VerifyRunner::new(executor.clone(), browser.clone(), link.clone(), clock.clone(), config.verify.clone()));
    let scip = Arc::new(ScipStep::new(executor.clone(), link.clone(), config.scip.clone()));
    let deps = Arc::new(RunnerDeps {
        link: link.clone(),
        executor: executor.clone(),
        state,
        harnesses,
        mcp_host,
        browser,
        verify,
        scip,
        clock,
        ids: ids.clone(),
        tools: config.tools.clone(),
        config: config.task.clone(),
    });
    let daemon = Daemon::new(deps, config.daemon.clone());

    tracing::info!(
        server = %config.link.server_url,
        name = %config.link.name,
        ?isolation,
        data_dir = %config.executor.data_dir.display(),
        "runner started"
    );
    LeaseLoop::new(daemon, link, executor, ids).run(shutdown).await?;
    tracing::info!("runner stopped");
    Ok(())
}

/// The bootstrap token while the credential file holds no runner id. Once registered, the token
/// is spent: a `file:` bootstrap token left on disk is deleted (server.md "Runner credential
/// lifecycle") and nothing is resolved.
fn bootstrap_token(credential_file: &Path, source: Option<&SecretRef>) -> Result<Option<String>, RunError> {
    let credentials = Credentials::load_or_create(credential_file)
        .map_err(|e| RunError::Link(format!("credential file {}: {e}", credential_file.display())))?;
    if credentials.runner_id().is_none() {
        return source.map(|s| s.resolve().map_err(|e| RunError::Config(format!("runner.link.bootstrap_token: {e}")))).transpose();
    }
    if let Some(SecretRef::File(path)) = source {
        match std::fs::remove_file(path) {
            Ok(()) => tracing::info!(path = %path.display(), "spent bootstrap token file deleted"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(path = %path.display(), error = %e, "could not delete the spent bootstrap token file"),
        }
    }
    Ok(None)
}

/// `binary` as a file on this host: a path as given, a bare name through `PATH`.
fn installed(binary: &str) -> Option<PathBuf> {
    if binary.contains('/') {
        let path = PathBuf::from(binary);
        return path.is_file().then_some(path);
    }
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths).map(|dir| dir.join(binary)).find(|candidate| candidate.is_file())
}
