//! `cloud-edge`: the public edge process. https on :443 (certificates from the domains
//! `CertStore`), ACME HTTP-01 + redirects on :80, sites polled from the control plane.

#![forbid(unsafe_code)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use cloud_domains::{CertStore, Http01Store};
use cloud_edge::{
    ClientCredentials, Components, Edge, GithubEndpoints, Http01, KeyStore, Listener, Mailer, OidcSettings, PollingDirectory, Settings,
    SmtpMailer, tls_server_config,
};
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "cloud-edge", about = "aho cloud edge: TLS, route modes, edge login, principal JWT")]
struct Args {
    /// https listener.
    #[arg(long, env = "AHO_CLOUD_EDGE_HTTPS", default_value = "0.0.0.0:443")]
    https: SocketAddr,
    /// Port-80 listener (ACME HTTP-01, redirects to https).
    #[arg(long, env = "AHO_CLOUD_EDGE_HTTP", default_value = "0.0.0.0:80")]
    http: SocketAddr,
    /// Optional plain-HTTP app listener (local development, private health checks); the app
    /// sees `X-Forwarded-Proto: http` on it.
    #[arg(long, env = "AHO_CLOUD_EDGE_PLAIN")]
    plain: Option<SocketAddr>,
    /// Control-plane base URL.
    #[arg(long, env = "AHO_CLOUD_CONTROL_PLANE")]
    control_plane: String,
    #[arg(long, env = "AHO_CLOUD_SERVICE_TOKEN", hide_env_values = true)]
    service_token: String,
    /// Principal signing keys (`<project>/<stage>.pem`).
    #[arg(long, env = "AHO_CLOUD_EDGE_KEYS")]
    keys_dir: PathBuf,
    /// Certificate directory shared with the domains issuer.
    #[arg(long, env = "AHO_CLOUD_CERTS")]
    certs_dir: PathBuf,
    /// HTTP-01 challenge directory shared with the domains issuer.
    #[arg(long, env = "AHO_CLOUD_HTTP01_DIR")]
    http01_dir: PathBuf,
    #[arg(long, env = "AHO_CLOUD_EDGE_POLL_SECS", default_value_t = 2)]
    poll_secs: u64,
    #[arg(long, env = "AHO_CLOUD_EDGE_CERT_RELOAD_SECS", default_value_t = 30)]
    cert_reload_secs: u64,
    /// SMTP URL for magic links (`smtps://user:pass@host:465`); without it magic links are off.
    #[arg(long, env = "AHO_CLOUD_SMTP_URL", hide_env_values = true)]
    smtp_url: Option<String>,
    #[arg(long, env = "AHO_CLOUD_MAIL_FROM", default_value = "aho cloud <login@aho.cloud>")]
    mail_from: String,
    #[arg(long, env = "AHO_CLOUD_EDGE_SESSION_TTL_SECS", default_value_t = 12 * 3600)]
    session_ttl_secs: u64,
    #[arg(long, env = "AHO_CLOUD_GOOGLE_CLIENT_ID")]
    google_client_id: Option<String>,
    #[arg(long, env = "AHO_CLOUD_GOOGLE_CLIENT_SECRET", hide_env_values = true)]
    google_client_secret: Option<String>,
    #[arg(long, env = "AHO_CLOUD_MICROSOFT_CLIENT_ID")]
    microsoft_client_id: Option<String>,
    #[arg(long, env = "AHO_CLOUD_MICROSOFT_CLIENT_SECRET", hide_env_values = true)]
    microsoft_client_secret: Option<String>,
    #[arg(long, env = "AHO_CLOUD_GITHUB_CLIENT_ID")]
    github_client_id: Option<String>,
    #[arg(long, env = "AHO_CLOUD_GITHUB_CLIENT_SECRET", hide_env_values = true)]
    github_client_secret: Option<String>,
}

#[derive(Debug, thiserror::Error)]
enum StartError {
    #[error("certificates: {0}")]
    Certs(#[from] cloud_domains::CertError),
    #[error("http-01 directory: {0}")]
    Http01(std::io::Error),
    #[error("keys: {0}")]
    Keys(#[from] cloud_edge::KeyError),
    #[error("directory: {0}")]
    Directory(#[from] cloud_edge::DirectoryError),
    #[error("mail: {0}")]
    Mail(#[from] cloud_edge::MailError),
    #[error("edge: {0}")]
    Edge(#[from] cloud_edge::EdgeError),
    #[error("tls: {0}")]
    Tls(#[from] rustls::Error),
    #[error("bind {0}: {1}")]
    Bind(SocketAddr, std::io::Error),
}

/// The domains crate's challenge store, shared with the issuer through a directory.
struct Challenges(Http01Store);

impl Http01 for Challenges {
    fn key_authorization(&self, token: &str) -> Option<String> {
        self.0.get(token)
    }
}

fn creds(id: Option<String>, secret: Option<String>) -> Option<ClientCredentials> {
    id.map(|client_id| ClientCredentials { client_id, client_secret: secret })
}

async fn bind(addr: SocketAddr) -> Result<TcpListener, StartError> {
    TcpListener::bind(addr).await.map_err(|e| StartError::Bind(addr, e))
}

async fn run(args: Args) -> Result<(), StartError> {
    let certs = Arc::new(CertStore::open(&args.certs_dir)?);
    let http01 = Http01Store::with_dir(&args.http01_dir).map_err(StartError::Http01)?;
    let keys = Arc::new(KeyStore::open(&args.keys_dir)?);
    let directory = Arc::new(PollingDirectory::new(&args.control_plane, args.service_token.clone())?);
    if let Err(e) = directory.poll().await {
        tracing::warn!(error = %e, "first site poll failed; unknown hosts get 421 until one succeeds");
    }
    let mailer: Option<Arc<dyn Mailer>> = match &args.smtp_url {
        Some(url) => Some(Arc::new(SmtpMailer::from_url(url, &args.mail_from)?)),
        None => None,
    };
    let settings = Settings {
        session_ttl_secs: args.session_ttl_secs,
        oidc: OidcSettings {
            google: creds(args.google_client_id, args.google_client_secret),
            microsoft: creds(args.microsoft_client_id, args.microsoft_client_secret),
            github: creds(args.github_client_id, args.github_client_secret),
            github_endpoints: GithubEndpoints::default(),
            ..OidcSettings::default()
        },
        ..Settings::default()
    };
    let edge = Edge::new(Components { directory: directory.clone(), keys, mailer, http01: Some(Arc::new(Challenges(http01))), settings })?;
    let tls = tls_server_config(certs.clone())?;

    let https = bind(args.https).await?;
    let http = bind(args.http).await?;
    let plain = match args.plain {
        Some(addr) => Some(bind(addr).await?),
        None => None,
    };
    let _poller = directory.spawn(Duration::from_secs(args.poll_secs.max(1)));
    let reload_every = Duration::from_secs(args.cert_reload_secs.max(1));
    let _reloader = tokio::spawn(async move {
        let mut tick = tokio::time::interval(reload_every);
        loop {
            tick.tick().await;
            if let Err(e) = certs.reload() {
                tracing::warn!(error = %e, "certificate reload failed; keeping loaded certificates");
            }
        }
    });
    tracing::info!(https = %args.https, http = %args.http, plain = ?args.plain, "edge listening");
    let plain_edge = edge.clone();
    tokio::select! {
        () = edge.serve(https, Listener::Tls(tls)) => {}
        () = edge.serve(http, Listener::Redirect) => {}
        () = async move {
            match plain {
                Some(l) => plain_edge.serve(l, Listener::Plain).await,
                None => std::future::pending().await,
            }
        } => {}
        _ = tokio::signal::ctrl_c() => tracing::info!("shutting down"),
    }
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt().with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))).init();
    match run(Args::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %e, "cloud-edge failed to start");
            ExitCode::FAILURE
        }
    }
}
