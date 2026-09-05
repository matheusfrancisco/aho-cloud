//! `cloud-builder`: serves the internal build route for the control plane.

#![forbid(unsafe_code)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use cloud_builder::{Builder, Config, SERVICE_TOKEN_ENV, router};

/// Builds release images (BuildKit, pack) for the aho cloud control plane.
#[derive(Debug, Parser)]
#[command(name = "cloud-builder", version)]
struct Args {
    /// Address to serve the internal API on.
    #[arg(long, env = "AHO_CLOUD_BUILDER_LISTEN", default_value = "127.0.0.1:7420")]
    listen: SocketAddr,
    /// Shared internal service token (required on requests, sent when fetching archives).
    #[arg(long, env = SERVICE_TOKEN_ENV, hide_env_values = true)]
    service_token: String,
    /// Repository prefix images are pushed under, e.g. `registry.internal:5000/aho-cloud`.
    #[arg(long, env = "AHO_CLOUD_REGISTRY")]
    registry: String,
    /// Scratch directory for sources being built.
    #[arg(long, env = "AHO_CLOUD_BUILDER_WORK_DIR")]
    work_dir: PathBuf,
    #[arg(long, env = "AHO_CLOUD_BUILDER_GIT", default_value = "git")]
    git: PathBuf,
    #[arg(long, env = "AHO_CLOUD_BUILDER_BUILDCTL", default_value = "buildctl")]
    buildctl: PathBuf,
    #[arg(long, env = "AHO_CLOUD_BUILDER_PACK", default_value = "pack")]
    pack: PathBuf,
    /// BuildKit daemon address (`buildctl --addr`); unset uses buildctl's default.
    #[arg(long, env = "BUILDKIT_HOST")]
    buildkit_addr: Option<String>,
    /// Builds running at once.
    #[arg(long, env = "AHO_CLOUD_BUILDER_CONCURRENCY", default_value_t = 2)]
    concurrency: usize,
    /// Wall-clock limit per build, in seconds.
    #[arg(long, env = "AHO_CLOUD_BUILDER_TIMEOUT_SECS", default_value_t = 1800)]
    timeout_secs: u64,
    /// Bytes of tool output returned per build (the tail).
    #[arg(long, env = "AHO_CLOUD_BUILDER_LOG_LIMIT", default_value_t = 256 * 1024)]
    log_limit: usize,
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt().with_writer(std::io::stderr).init();
    let args = Args::parse();
    let config = Config {
        git: args.git,
        buildctl: args.buildctl,
        pack: args.pack,
        buildkit_addr: args.buildkit_addr,
        registry: args.registry,
        work_dir: args.work_dir,
        concurrency: args.concurrency,
        timeout: Duration::from_secs(args.timeout_secs),
        log_limit: args.log_limit,
        service_token: args.service_token,
    };
    let builder = match Builder::new(config) {
        Ok(b) => Arc::new(b),
        Err(e) => {
            tracing::error!("config: {e}");
            return ExitCode::from(2);
        }
    };
    let listener = match tokio::net::TcpListener::bind(args.listen).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("listen on {}: {e}", args.listen);
            return ExitCode::FAILURE;
        }
    };
    tracing::info!("cloud-builder listening on {}", args.listen);
    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    match axum::serve(listener, router(builder)).with_graceful_shutdown(shutdown).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("serve: {e}");
            ExitCode::FAILURE
        }
    }
}
