//! `cloud-control-plane`: the aho cloud API server.

#![forbid(unsafe_code)]

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use cloud_control_plane::blobs::FsBlobStore;
use cloud_control_plane::clock::SystemClock;
use cloud_control_plane::ledgers::Ledgers;
use cloud_control_plane::ports::{HttpBuilder, HttpRuntime, HttpSessionHost, ServiceClient};
use cloud_control_plane::store::{MemStore, PgStore, Store};
use cloud_control_plane::{AppState, Config, MasterKey, Services, router};
use cloud_domains::{EdgeTarget, HickoryResolver};

/// The aho cloud control plane.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Args {
    /// Address to listen on.
    #[arg(long, env = "AHO_CLOUD_BIND", default_value = "127.0.0.1:8080")]
    bind: SocketAddr,
    /// Postgres connection URL.
    #[arg(long, env = "DATABASE_URL", required_unless_present = "ephemeral")]
    database_url: Option<String>,
    /// Development only: identities, orgs, secrets and ledgers live in memory and vanish on
    /// exit (uploads still go under --data-dir). Conflicts with --database-url.
    #[arg(long, conflicts_with = "database_url")]
    ephemeral: bool,
    /// Directory for ledgers (`ledgers/<project>.jsonl`) and uploads (`blobs/`).
    #[arg(long, env = "AHO_CLOUD_DATA_DIR")]
    data_dir: PathBuf,
    /// 32-byte master key: 64 hex digits or base64url.
    #[arg(long, env = "AHO_CLOUD_MASTER_KEY", hide_env_values = true)]
    master_key: String,
    /// Shared token for internal routes and on-call automation.
    #[arg(long, env = "AHO_CLOUD_SERVICE_TOKEN", hide_env_values = true)]
    service_token: String,
    /// Base URL sibling services reach this server at.
    #[arg(long, env = "AHO_CLOUD_PUBLIC_URL")]
    public_url: String,
    /// WebAuthn relying-party id (the console's registrable domain).
    #[arg(long, env = "AHO_CLOUD_RP_ID")]
    rp_id: String,
    /// Exact console origin, e.g. `https://console.aho.cloud`.
    #[arg(long, env = "AHO_CLOUD_RP_ORIGIN")]
    rp_origin: String,
    #[arg(long, env = "AHO_CLOUD_RP_NAME", default_value = "aho cloud")]
    rp_name: String,
    #[arg(long, env = "AHO_CLOUD_BUILDER_URL")]
    builder_url: String,
    #[arg(long, env = "AHO_CLOUD_RUNTIME_URL")]
    runtime_url: String,
    #[arg(long, env = "AHO_CLOUD_SESSION_HOST_URL")]
    session_host_url: String,
    /// CNAME target customer subdomains point at.
    #[arg(long, env = "AHO_CLOUD_EDGE_CNAME")]
    edge_cname: String,
    /// Edge IPv4 addresses for apex domains (comma-separated).
    #[arg(long, env = "AHO_CLOUD_EDGE_IPV4", value_delimiter = ',')]
    edge_ipv4: Vec<Ipv4Addr>,
    #[arg(long, env = "AHO_CLOUD_EDGE_IPV6", value_delimiter = ',')]
    edge_ipv6: Vec<Ipv6Addr>,
    /// Nameservers for NS-delegated subzones (comma-separated).
    #[arg(long, env = "AHO_CLOUD_EDGE_NAMESERVERS", value_delimiter = ',')]
    edge_nameservers: Vec<String>,
    /// Preview hostnames are `<project>.<preview-domain>`.
    #[arg(long, env = "AHO_CLOUD_PREVIEW_DOMAIN", default_value = "preview.aho.cloud")]
    preview_domain: String,
    #[arg(long, env = "AHO_CLOUD_DOMAIN_GRACE_SECS", default_value_t = 7 * 24 * 3600)]
    domain_grace_secs: u64,
    /// How often every domain binding is re-checked.
    #[arg(long, env = "AHO_CLOUD_DOMAIN_RECHECK_SECS", default_value_t = 300)]
    domain_recheck_secs: u64,
    /// Largest archive or session-log upload, in MiB.
    #[arg(long, env = "AHO_CLOUD_MAX_UPLOAD_MIB", default_value_t = 256)]
    max_upload_mib: usize,
    /// Timeout for one build request.
    #[arg(long, env = "AHO_CLOUD_BUILD_TIMEOUT_SECS", default_value_t = 3600)]
    build_timeout_secs: u64,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args = Args::parse();
    let master_key = MasterKey::parse(&args.master_key).ok_or("master key must be 32 bytes as 64 hex digits or base64url")?;
    if args.service_token.len() < 32 {
        return Err("service token must be at least 32 characters".into());
    }
    let mut cfg = Config::new(master_key, args.service_token.clone(), args.rp_id, args.rp_origin);
    cfg.public_url = args.public_url;
    cfg.rp_name = args.rp_name;
    cfg.edge_target = EdgeTarget { cname: args.edge_cname, ipv4: args.edge_ipv4, ipv6: args.edge_ipv6, nameservers: args.edge_nameservers };
    cfg.preview_domain = args.preview_domain;
    cfg.domain_grace_secs = args.domain_grace_secs;
    cfg.max_upload_bytes = args.max_upload_mib * 1024 * 1024;

    let (store, ledgers): (Arc<dyn Store>, Ledgers) = match &args.database_url {
        Some(url) => (Arc::new(PgStore::connect(url).await?), Ledgers::on_disk(args.data_dir.join("ledgers"))),
        None => {
            tracing::warn!("--ephemeral: nothing but uploads survives a restart");
            (Arc::new(MemStore::new()), Ledgers::memory())
        }
    };
    let token = &args.service_token;
    let services = Services {
        builder: Arc::new(HttpBuilder(ServiceClient::new(
            "builder",
            &args.builder_url,
            token,
            Duration::from_secs(args.build_timeout_secs),
        )?)),
        runtime: Arc::new(HttpRuntime(ServiceClient::new("runtime", &args.runtime_url, token, Duration::from_secs(60))?)),
        session_host: Arc::new(HttpSessionHost(ServiceClient::new(
            "session-host",
            &args.session_host_url,
            token,
            Duration::from_secs(60),
        )?)),
        resolver: Arc::new(HickoryResolver::system()?),
    };
    let state =
        AppState::new(cfg, store, Arc::new(FsBlobStore::new(args.data_dir.join("blobs"))), ledgers, services, Arc::new(SystemClock))
            .await?;

    let recheck = state.clone();
    let every = Duration::from_secs(args.domain_recheck_secs.max(30));
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(every);
        loop {
            tick.tick().await;
            match recheck.recheck_domains().await {
                Ok(0) => {}
                Ok(n) => tracing::info!(transitions = n, "domain bindings changed"),
                Err(error) => tracing::warn!(%error, "domain recheck failed"),
            }
        }
    });

    let listener = tokio::net::TcpListener::bind(args.bind).await?;
    tracing::info!(addr = %listener.local_addr()?, "control plane listening");
    axum::serve(listener, router(state.clone()))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    state.idle().await;
    Ok(())
}
