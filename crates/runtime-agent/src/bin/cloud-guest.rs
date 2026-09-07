//! `cloud-guest`: PID 1 inside every microVM. Reads the config drive, mounts what the process
//! needs, runs it (restarting with backoff), reaps zombies, and serves the control protocol
//! over vsock (Linux) and optionally a Unix socket.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use cloud_runtime::guest::process::{Backoff, ReapMode, Reaper, RunSpec, Runner, RunnerConfig};
use cloud_runtime::guest::{self, GuestConfig, control, init};

#[derive(Parser, Debug)]
#[command(name = "cloud-guest", about = "aho cloud guest init")]
struct Args {
    /// Config drive: the raw device or a file.
    #[arg(long, env = guest::CONFIG_DRIVE_ENV, default_value = guest::CONFIG_DEVICE)]
    config: PathBuf,
    /// vsock port of the control protocol (Linux only).
    #[arg(long, default_value_t = guest::CONTROL_PORT)]
    vsock_port: u32,
    /// Also serve the control protocol on this Unix socket.
    #[arg(long)]
    control_socket: Option<PathBuf>,
    /// Root of the image filesystem, for `/etc/passwd`, `/etc/group` and `resolv.conf`.
    #[arg(long, default_value = "/")]
    root: PathBuf,
    /// Seconds between SIGTERM and SIGKILL when restarting the process.
    #[arg(long, default_value_t = 10)]
    stop_grace_secs: u64,
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

fn main() {
    let pid1 = std::process::id() == 1;
    tracing_subscriber::fmt().with_writer(std::io::stderr).with_ansi(false).init();
    let args = Args::parse();
    let result = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(BoxError::from)
        .and_then(|rt| rt.block_on(run(args, pid1)));
    if let Err(e) = result {
        tracing::error!(error = %e, "cloud-guest failed");
        fatal(pid1);
    }
}

/// PID 1 must never exit (the kernel would panic): power the VM off so the host notices.
fn fatal(pid1: bool) -> ! {
    #[cfg(target_os = "linux")]
    if pid1 {
        init::power_off();
    }
    let _ = pid1;
    std::process::exit(1)
}

async fn run(args: Args, pid1: bool) -> Result<(), BoxError> {
    #[cfg(target_os = "linux")]
    if pid1 {
        init::mount_early()?;
    }
    let bytes = tokio::fs::read(&args.config).await.map_err(|e| format!("{}: {e}", args.config.display()))?;
    let cfg = GuestConfig::from_drive(&bytes)?;
    if pid1 {
        init::write_resolv_conf(&args.root.join("etc/resolv.conf"), &cfg.dns)?;
        #[cfg(target_os = "linux")]
        if let Some(volume) = &cfg.volume {
            init::mount_volume(volume)?;
        }
    }
    let user = cfg.image.user.as_deref().map(|u| init::resolve_user(u, &args.root)).transpose()?;
    let spec = RunSpec { argv: cfg.argv()?, env: cfg.process_env(), cwd: cfg.image.working_dir.as_ref().map(PathBuf::from), user };
    tracing::info!(process = %cfg.process, argv = ?spec.argv, "starting");
    let reaper = Reaper::start(if pid1 { ReapMode::All } else { ReapMode::Own })?;
    let runner =
        Runner::start(reaper, RunnerConfig { spec, backoff: Backoff::default(), stop_grace: Duration::from_secs(args.stop_grace_secs) });

    if let Some(path) = &args.control_socket {
        let _ = std::fs::remove_file(path);
        let listener = tokio::net::UnixListener::bind(path)?;
        let runner = runner.clone();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let runner = runner.clone();
                        tokio::spawn(async move { control::serve(stream, &runner).await });
                    }
                    Err(e) => tracing::warn!(error = %e, "control socket accept failed"),
                }
            }
        });
    }

    #[cfg(target_os = "linux")]
    {
        let runner = runner.clone();
        let port = args.vsock_port;
        tokio::spawn(async move {
            if let Err(e) = serve_vsock(port, runner).await {
                tracing::error!(error = %e, "vsock control server stopped");
            }
        });
    }
    #[cfg(not(target_os = "linux"))]
    let _ = args.vsock_port;

    if pid1 {
        std::future::pending::<()>().await;
    }
    tokio::signal::ctrl_c().await?;
    runner.stop().await;
    Ok(())
}

#[cfg(target_os = "linux")]
async fn serve_vsock(port: u32, runner: Runner) -> std::io::Result<()> {
    let addr = tokio_vsock::VsockAddr::new(tokio_vsock::VMADDR_CID_ANY, port);
    let listener = tokio_vsock::VsockListener::bind(addr)?;
    tracing::info!(port, "control protocol on vsock");
    loop {
        let (stream, _) = listener.accept().await?;
        let runner = runner.clone();
        tokio::spawn(async move { control::serve(stream, &runner).await });
    }
}
