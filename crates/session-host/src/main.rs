//! `cloud-session-host`: serves the internal workspace route and the rpc WebSocket bridge.

#![forbid(unsafe_code)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use cloud_session_host::spawn::{SERVICE_TOKEN_ENV, SESSION_SECRET_ENV};
use cloud_session_host::{CommandTemplate, Config, Limits, SessionHost, TokenKey, TokioSpawner};

/// Runs `ahoc` per workspace and bridges aho rpc (NDJSON over stdio) to WebSockets.
// No `Debug`: the struct holds the service token and signing secret.
#[derive(Parser)]
#[command(name = "cloud-session-host", version)]
struct Cli {
    /// Address to listen on.
    #[arg(long, env = "AHO_CLOUD_SESSION_LISTEN", default_value = "127.0.0.1:8790")]
    listen: SocketAddr,
    /// Directory holding `workspaces/<id>/{workspace.json,project/}`.
    #[arg(long, env = "AHO_CLOUD_SESSION_DATA")]
    data_root: PathBuf,
    /// Base URL browsers reach this service at (`wss://…`); `rpc_url` = `<base>/rpc/<workspace>`.
    #[arg(long, env = "AHO_CLOUD_SESSION_PUBLIC_URL")]
    public_url: String,
    /// Preview URL template; `{project}` is replaced by the project slug.
    #[arg(long, env = "AHO_CLOUD_PREVIEW_URL")]
    preview_url: String,
    /// Bearer token the control plane presents on internal routes.
    #[arg(long, env = SERVICE_TOKEN_ENV, hide_env_values = true)]
    service_token: String,
    /// HMAC secret for rpc tokens (at least 32 bytes).
    #[arg(long, env = SESSION_SECRET_ENV, hide_env_values = true)]
    token_secret: String,
    /// Lifetime of minted rpc tokens.
    #[arg(long, default_value_t = 600)]
    token_ttl_secs: u64,
    /// Program run per connection.
    #[arg(long, default_value = "ahoc")]
    command: PathBuf,
    /// Argument template (repeatable); `{dir}` is the workspace project directory.
    #[arg(long = "arg", allow_hyphen_values = true, default_values = ["--project", "{dir}", "rpc"])]
    args: Vec<String>,
    /// Appended when the workspace has a session id (repeatable); `{session}` is the id.
    #[arg(long = "session-arg", allow_hyphen_values = true, default_values = ["--session", "{session}"])]
    session_args: Vec<String>,
    /// `KEY=VALUE` passed as `--set KEY=VALUE` before the arguments, e.g. `seams.world=vm`.
    #[arg(long = "set", value_name = "KEY=VALUE")]
    sets: Vec<String>,
    /// Open rpc connections across all workspaces.
    #[arg(long, default_value_t = Limits::DEFAULT.max_connections)]
    max_connections: usize,
    /// Open rpc connections per workspace.
    #[arg(long, default_value_t = Limits::DEFAULT.max_per_workspace)]
    max_per_workspace: usize,
    /// Close a connection after this many seconds without traffic.
    #[arg(long, default_value_t = Limits::DEFAULT.idle_timeout.as_secs())]
    idle_timeout_secs: u64,
    /// Seconds a child gets to exit after its stdin closes before it is killed.
    #[arg(long, default_value_t = Limits::DEFAULT.kill_grace.as_secs())]
    kill_grace_secs: u64,
    /// Largest client message, in bytes.
    #[arg(long, default_value_t = Limits::DEFAULT.max_frame_bytes)]
    max_frame_bytes: usize,
    /// Largest child output line, in bytes.
    #[arg(long, default_value_t = Limits::DEFAULT.max_line_bytes)]
    max_line_bytes: usize,
}

impl Cli {
    fn config(self) -> Result<Config, String> {
        Ok(Config {
            data_root: self.data_root,
            public_url: self.public_url,
            preview_url: self.preview_url,
            service_token: self.service_token,
            token_key: TokenKey::new(self.token_secret.as_bytes()).map_err(|e| e.to_string())?,
            token_ttl: Duration::from_secs(self.token_ttl_secs),
            command: CommandTemplate { program: self.command, args: self.args, session_args: self.session_args, sets: self.sets },
            limits: Limits {
                max_connections: self.max_connections,
                max_per_workspace: self.max_per_workspace,
                idle_timeout: Duration::from_secs(self.idle_timeout_secs),
                kill_grace: Duration::from_secs(self.kill_grace_secs),
                max_frame_bytes: self.max_frame_bytes,
                max_line_bytes: self.max_line_bytes,
            },
        })
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let term = async {
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(mut s) => {
                    s.recv().await;
                }
                Err(e) => {
                    tracing::warn!("SIGTERM handler unavailable: {e}");
                    std::future::pending::<()>().await;
                }
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            () = term => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

async fn run(cli: Cli) -> Result<(), String> {
    let listen = cli.listen;
    let host = SessionHost::new(cli.config()?, Arc::new(TokioSpawner)).map_err(|e| e.to_string())?;
    let listener = tokio::net::TcpListener::bind(listen).await.map_err(|e| format!("listen {listen}: {e}"))?;
    tracing::info!(%listen, "cloud-session-host listening");
    host.serve(listener, shutdown_signal()).await.map_err(|e| e.to_string())
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    match run(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("cloud-session-host: {e}");
            ExitCode::FAILURE
        }
    }
}
