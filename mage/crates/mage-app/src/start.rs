//! `mage start`: runs the routed role(s) until SIGINT / SIGTERM (a second signal exits at once).
//! `all` runs the three roles as tasks sharing one shutdown token; the server binds first so the
//! runner and the indexer find it, and any role ending early stops the others.

use anyhow::{Context, Result, anyhow};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::assets;
use crate::roles::Roles;

pub async fn run(roles: Roles) -> Result<()> {
    let shutdown = CancellationToken::new();
    let signals = tokio::spawn(watch_signals(shutdown.clone()));
    let result = match roles {
        Roles::Server(config) => mage_server::run(*config, assets::bundle(), shutdown.clone()).await.context("server"),
        Roles::Indexer(config) => mage_indexer::run(*config, shutdown.clone()).await.context("indexer"),
        Roles::Runner(config) => mage_runner::run(*config, shutdown.clone()).await.context("runner"),
        Roles::All { server, indexer, runner } => all(*server, *indexer, *runner, &shutdown).await,
    };
    signals.abort();
    if let Err(err) = &result {
        tracing::error!(error = format!("{err:#}"), "mage stopped with an error");
    }
    result
}

async fn all(server: mage_server::Config, indexer: mage_indexer::Config, runner: mage_runner::Config, shutdown: &CancellationToken) -> Result<()> {
    tracing::warn!("mage start all: every role in one process (local development only)");
    let server = mage_server::compose::Server::build(server, assets::bundle(), shutdown.clone()).await.context("server")?;
    tracing::info!(addr = %server.local_addr().context("server")?, "server listening");
    let mut roles = JoinSet::new();
    roles.spawn(async move { ("server", server.serve().await.map_err(anyhow::Error::from)) });
    let token = shutdown.clone();
    roles.spawn(async move { ("indexer", mage_indexer::run(indexer, token).await.map_err(anyhow::Error::from)) });
    let token = shutdown.clone();
    roles.spawn(async move { ("runner", mage_runner::run(runner, token).await.map_err(anyhow::Error::from)) });

    let mut failures = Vec::new();
    while let Some(joined) = roles.join_next().await {
        let (role, result) = joined.unwrap_or_else(|panic| ("role task", Err(anyhow!("panicked: {panic}"))));
        if !shutdown.is_cancelled() {
            tracing::warn!(role, "role stopped before shutdown; stopping the others");
            shutdown.cancel();
        }
        match result {
            Ok(()) => tracing::info!(role, "role stopped"),
            Err(err) => {
                tracing::error!(role, error = format!("{err:#}"), "role failed");
                failures.push(format!("{role}: {err:#}"));
            }
        }
    }
    if failures.is_empty() { Ok(()) } else { Err(anyhow!(failures.join("; "))) }
}

async fn watch_signals(shutdown: CancellationToken) {
    signal().await;
    tracing::info!("shutdown requested: draining (signal again to exit now)");
    shutdown.cancel();
    signal().await;
    tracing::warn!("second signal: exiting without draining");
    std::process::exit(130);
}

/// SIGINT or SIGTERM.
async fn signal() {
    match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(mut term) => {
            tokio::select! {
                Ok(()) = tokio::signal::ctrl_c() => {}
                Some(()) = term.recv() => {}
                else => std::future::pending::<()>().await,
            }
        }
        Err(err) => {
            tracing::warn!(error = %err, "SIGTERM handler unavailable; only SIGINT stops mage");
            if tokio::signal::ctrl_c().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }
}
