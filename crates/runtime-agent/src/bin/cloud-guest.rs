//! `cloud-guest`: PID 1 inside every microVM. Reads the config drive, mounts what the process
//! needs, runs it (restarting with backoff), reaps zombies, and serves the control protocol
//! over vsock (Linux) and optionally a Unix socket; as PID 1 on Linux it also serves exec
//! sessions on vsock.
//!
//! Subcommands, run inside a guest by the host: `exec` serves one exec session on
//! stdin/stdout (the Docker backend's `docker exec -i <c> cloud-guest exec`); `files` and `file`
//! print a workspace's file tree or one file as JSON (non-zero exit and a message on stderr
//! otherwise: [`guest::EXIT_BAD_PATH`], [`guest::EXIT_NOT_FOUND`], 1).

#![forbid(unsafe_code)]

use std::io::Write as _;
use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand};
use cloud_runtime::guest::exec::{self, ExecContext};
use cloud_runtime::guest::process::{Backoff, ReapMode, Reaper, RunSpec, Runner, RunnerConfig};
use cloud_runtime::guest::{self, GuestConfig, control, init};
use cloud_workdir::WorkdirError;
use serde::Serialize;

#[derive(Parser, Debug)]
#[command(name = "cloud-guest", about = "aho cloud guest init")]
struct Args {
    #[command(subcommand)]
    command: Option<Sub>,
    /// Config drive: the raw device or a file.
    #[arg(long, env = guest::CONFIG_DRIVE_ENV, default_value = guest::CONFIG_DEVICE)]
    config: PathBuf,
    /// vsock port of the control protocol (Linux only).
    #[arg(long, default_value_t = guest::CONTROL_PORT)]
    vsock_port: u32,
    /// vsock port of the exec server (Linux, PID 1 only).
    #[arg(long, default_value_t = guest::EXEC_PORT)]
    exec_vsock_port: u32,
    /// Also serve the control protocol on this Unix socket.
    #[arg(long)]
    control_socket: Option<PathBuf>,
    /// Root of the image filesystem, for `/etc/passwd`, `/etc/group` and `resolv.conf`.
    #[arg(long, default_value = "/")]
    root: PathBuf,
    /// Seconds between SIGTERM and SIGKILL when restarting the process.
    #[arg(long, default_value_t = 10)]
    stop_grace_secs: u64,
    /// Seconds between SIGHUP and SIGKILL when an exec session's host goes away.
    #[arg(long, default_value_t = 2)]
    hangup_grace_secs: u64,
}

#[derive(Subcommand, Debug)]
enum Sub {
    /// Serve one exec session on stdin/stdout, as this process's user and environment.
    Exec,
    /// Print the file tree under `--root` (`cloud_api::FileTree` JSON).
    Files {
        #[arg(long)]
        root: PathBuf,
        #[arg(long, default_value_t = 5000)]
        max_entries: usize,
    },
    /// Print one file under `--root` (`cloud_api::FileContent` JSON).
    File {
        #[arg(long)]
        root: PathBuf,
        /// Relative to `--root`, `/`-separated.
        #[arg(long)]
        path: String,
        /// Most text bytes returned; longer files come back truncated.
        #[arg(long, default_value_t = 1 << 20)]
        max_bytes: u64,
    },
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

fn main() {
    let pid1 = std::process::id() == 1;
    tracing_subscriber::fmt().with_writer(std::io::stderr).with_ansi(false).init();
    let args = Args::parse();
    let grace = Duration::from_secs(args.hangup_grace_secs);
    match &args.command {
        Some(Sub::Files { root, max_entries }) => std::process::exit(print_json(cloud_workdir::list_files(root, *max_entries))),
        Some(Sub::File { root, path, max_bytes }) => std::process::exit(print_json(cloud_workdir::read_file(root, path, *max_bytes))),
        Some(Sub::Exec) => std::process::exit(
            match runtime().and_then(|rt| {
                let served = rt.block_on(exec_stdio(grace));
                // Dropping the runtime would wait for the blocking stdin read, which never ends
                // while the host keeps the stream open.
                rt.shutdown_background();
                served
            }) {
                Ok(()) => 0,
                Err(e) => {
                    tracing::error!(error = %e, "exec session failed");
                    1
                }
            },
        ),
        None => {}
    }
    if let Err(e) = runtime().and_then(|rt| rt.block_on(run(args, pid1))) {
        tracing::error!(error = %e, "cloud-guest failed");
        fatal(pid1);
    }
}

fn runtime() -> Result<tokio::runtime::Runtime, BoxError> {
    Ok(tokio::runtime::Builder::new_multi_thread().enable_all().build()?)
}

/// Prints `result` as JSON; the exit status.
fn print_json<T: Serialize>(result: Result<T, WorkdirError>) -> i32 {
    let value = match result {
        Ok(value) => value,
        Err(e) => {
            eprintln!("{e}");
            return match e {
                WorkdirError::BadPath(_) | WorkdirError::Private(_) | WorkdirError::Escapes(_) | WorkdirError::NotAFile(_) => {
                    guest::EXIT_BAD_PATH
                }
                WorkdirError::NotFound(_) => guest::EXIT_NOT_FOUND,
                _ => 1,
            };
        }
    };
    let written = serde_json::to_vec(&value).map_err(std::io::Error::other).and_then(|json| {
        let mut out = std::io::stdout().lock();
        out.write_all(&json)?;
        out.flush()
    });
    match written {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("writing the result: {e}");
            1
        }
    }
}

async fn exec_stdio(grace: Duration) -> Result<(), BoxError> {
    let reaper = Reaper::start(ReapMode::Own)?;
    let conn = tokio::io::join(tokio::io::stdin(), tokio::io::stdout());
    exec::serve(conn, &reaper, &ExecContext::inherit(grace)).await?;
    Ok(())
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
    let user = cfg.image.user.as_deref().map(|u| init::resolve_user(u, &args.root)).transpose()?;
    if pid1 {
        init::write_resolv_conf(&args.root.join("etc/resolv.conf"), &cfg.dns)?;
        #[cfg(target_os = "linux")]
        if let Some(volume) = &cfg.volume {
            init::mount_volume(volume)?;
            if let Some(user) = user {
                init::claim_volume(std::path::Path::new(&volume.mount), user)?;
            }
        }
    }
    let spec = RunSpec { argv: cfg.argv()?, env: cfg.process_env(), cwd: cfg.image.working_dir.as_ref().map(PathBuf::from), user };
    tracing::info!(process = %cfg.process, argv = ?spec.argv, "starting");
    let reaper = Reaper::start(if pid1 { ReapMode::All } else { ReapMode::Own })?;
    // Exec sessions share the process's environment, directory and user.
    let exec_ctx = ExecContext {
        env: spec.env.clone(),
        cwd: spec.cwd.clone(),
        user: spec.user,
        hangup_grace: Duration::from_secs(args.hangup_grace_secs),
    };
    let runner = Runner::start(
        reaper.clone(),
        RunnerConfig { spec, backoff: Backoff::default(), stop_grace: Duration::from_secs(args.stop_grace_secs) },
    );

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
        if pid1 {
            let port = args.exec_vsock_port;
            tokio::spawn(async move {
                if let Err(e) = serve_exec_vsock(port, reaper, exec_ctx).await {
                    tracing::error!(error = %e, "vsock exec server stopped");
                }
            });
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (args.vsock_port, args.exec_vsock_port, exec_ctx);

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

#[cfg(target_os = "linux")]
async fn serve_exec_vsock(port: u32, reaper: std::sync::Arc<Reaper>, ctx: ExecContext) -> std::io::Result<()> {
    let addr = tokio_vsock::VsockAddr::new(tokio_vsock::VMADDR_CID_ANY, port);
    let listener = tokio_vsock::VsockListener::bind(addr)?;
    tracing::info!(port, "exec sessions on vsock");
    let ctx = std::sync::Arc::new(ctx);
    loop {
        let (stream, _) = listener.accept().await?;
        let (reaper, ctx) = (reaper.clone(), ctx.clone());
        tokio::spawn(async move {
            if let Err(e) = exec::serve(stream, &reaper, &ctx).await {
                tracing::warn!(error = %e, "exec session ended with an error");
            }
        });
    }
}
