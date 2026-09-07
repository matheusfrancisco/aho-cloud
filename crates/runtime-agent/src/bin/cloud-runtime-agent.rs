//! `cloud-runtime-agent`: the per-host daemon. Serves the internal deployment API, runs the
//! reconcile loop over Firecracker microVMs, and the egress proxy guests must use.

#![forbid(unsafe_code)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use cloud_runtime::egress::{EgressPolicies, EgressProxy, SystemDialer};
use cloud_runtime::firecracker::{Firecracker, FirecrackerConfig};
use cloud_runtime::image::{ImagePrograms, ImageStoreConfig};
use cloud_runtime::net::{Allocator, Ipv4Cidr};
use cloud_runtime::{Supervisor, SupervisorConfig, api};

#[derive(Parser, Debug)]
#[command(name = "cloud-runtime-agent", about = "aho cloud runtime agent (Firecracker microVMs)")]
struct Args {
    /// Internal API listen address.
    #[arg(long, env = "AHO_CLOUD_RUNTIME_LISTEN", default_value = "0.0.0.0:7400")]
    listen: SocketAddr,
    /// Shared service token the control plane presents as `Authorization: Bearer`.
    #[arg(long, env = "AHO_CLOUD_SERVICE_TOKEN", hide_env_values = true)]
    service_token: String,
    #[arg(long, env = "AHO_CLOUD_RUNTIME_STATE", default_value = "/var/lib/aho-cloud/runtime")]
    state_dir: PathBuf,
    /// Image cache (defaults to `<state-dir>/images`).
    #[arg(long)]
    image_dir: Option<PathBuf>,
    /// cgroup v2 directory the agent owns; one child per replica.
    #[arg(long, default_value = "/sys/fs/cgroup/aho-cloud")]
    cgroup_root: PathBuf,
    /// Guest /24; `.1` is the host gateway.
    #[arg(long, default_value = "10.77.0.0/24")]
    subnet: Ipv4Cidr,
    /// Networks allowed to open connections to guests, e.g. remote edge nodes (repeatable).
    #[arg(long = "inbound-source")]
    inbound_sources: Vec<Ipv4Cidr>,
    /// Registry repository per project; `{project}` is substituted.
    #[arg(long, env = "AHO_CLOUD_IMAGE_REPO", default_value = "localhost:5000/aho-cloud/{project}")]
    image_repo: String,
    #[arg(long, default_value = "/var/lib/aho-cloud/vmlinux")]
    kernel: PathBuf,
    /// The `cloud-guest` binary (static, for the guest architecture) placed in every rootfs.
    #[arg(long, default_value = "/usr/lib/aho-cloud/cloud-guest")]
    guest_binary: PathBuf,
    #[arg(long, default_value = "firecracker")]
    firecracker: PathBuf,
    #[arg(long, default_value = "skopeo")]
    skopeo: PathBuf,
    #[arg(long, default_value = "umoci")]
    umoci: PathBuf,
    #[arg(long, default_value = "mkfs.ext4")]
    mkfs_ext4: PathBuf,
    #[arg(long, default_value = "ip")]
    ip: PathBuf,
    #[arg(long, default_value = "nft")]
    nft: PathBuf,
    #[arg(long, default_value_t = 1)]
    vcpus: u8,
    #[arg(long, default_value_t = 512)]
    mem_mib: u32,
    #[arg(long, default_value_t = 2048)]
    rootfs_mib: u32,
    #[arg(long, default_value_t = 64)]
    memory_overhead_mib: u32,
    #[arg(long, default_value_t = 64)]
    pids_max: u32,
    /// Egress proxy port on the gateway address.
    #[arg(long, default_value_t = 3128)]
    proxy_port: u16,
    /// DNS resolver port on the gateway address (the resolver itself is host setup).
    #[arg(long, default_value_t = 53)]
    dns_port: u16,
    #[arg(long, default_value_t = 16)]
    max_replicas: u16,
    #[arg(long, default_value_t = 10)]
    reconcile_interval_secs: u64,
    #[arg(long, default_value_t = 2000)]
    health_timeout_ms: u64,
    #[arg(long, default_value_t = 3)]
    unhealthy_threshold: u32,
    #[arg(long, default_value_t = 60)]
    startup_grace_secs: u64,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let args = Args::parse();
    if args.service_token.is_empty() {
        return Err("AHO_CLOUD_SERVICE_TOKEN must not be empty".into());
    }
    let allocator = Allocator::new(args.subnet)?;
    let gateway = allocator.gateway();

    let backend = Firecracker::new(FirecrackerConfig {
        images: ImageStoreConfig {
            dir: args.image_dir.clone().unwrap_or_else(|| args.state_dir.join("images")),
            programs: ImagePrograms { skopeo: args.skopeo, umoci: args.umoci, mkfs_ext4: args.mkfs_ext4 },
            repo_template: args.image_repo,
            guest_binary: args.guest_binary,
        },
        state_dir: args.state_dir,
        firecracker: args.firecracker,
        ip: args.ip,
        nft: args.nft,
        kernel: args.kernel,
        cgroup_root: args.cgroup_root,
        vcpus: args.vcpus,
        mem_mib: args.mem_mib,
        rootfs_mib: args.rootfs_mib,
        memory_overhead_mib: args.memory_overhead_mib,
        pids_max: args.pids_max,
        gateway,
        proxy_port: args.proxy_port,
        dns_port: args.dns_port,
        inbound_sources: args.inbound_sources,
    });
    backend.prepare_host().await?;

    let egress = EgressPolicies::default();
    let proxy =
        Arc::new(EgressProxy::new(egress.clone(), SystemDialer { guest_subnet: args.subnet, connect_timeout: Duration::from_secs(10) }));
    let proxy_listener = tokio::net::TcpListener::bind((gateway, args.proxy_port)).await?;
    tracing::info!(addr = %proxy_listener.local_addr()?, "egress proxy listening");

    let supervisor = Supervisor::new(
        backend,
        allocator,
        egress,
        SupervisorConfig {
            proxy_port: args.proxy_port,
            max_replicas: args.max_replicas,
            health_timeout: Duration::from_millis(args.health_timeout_ms),
            unhealthy_threshold: args.unhealthy_threshold,
            startup_grace: Duration::from_secs(args.startup_grace_secs),
        },
    );
    let app = api::router(supervisor.clone(), &args.service_token);
    let api_listener = tokio::net::TcpListener::bind(args.listen).await?;
    tracing::info!(addr = %api_listener.local_addr()?, "runtime API listening");

    let interval = Duration::from_secs(args.reconcile_interval_secs.max(1));
    tokio::select! {
        r = axum::serve(api_listener, app) => r?,
        r = proxy.serve(proxy_listener) => r?,
        () = supervisor.run(interval) => {}
        r = tokio::signal::ctrl_c() => {
            r?;
            tracing::info!("shutting down");
        }
    }
    Ok(())
}
