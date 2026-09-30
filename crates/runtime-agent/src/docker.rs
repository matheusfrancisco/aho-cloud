//! A development backend: each replica is a Docker container instead of a Firecracker microVM,
//! for local stacks on machines without KVM (macOS).
//!
//! It is not an isolation boundary. The egress allowlist is not enforced (no proxy, no
//! nftables), resource ceilings are Docker's `--memory`/`--cpus`/`--pids-limit`, and
//! containers share the Docker daemon's kernel. Everything the supervisor relies on still
//! holds: images run pinned by digest, secrets reach the process only through an env file
//! (never argv), volumes persist per `(project, stage, process, index)`, the replica's port is
//! published on [`DockerConfig::publish_ip`] and reported as its endpoint, and command health
//! checks and restarts speak the same control protocol as `cloud-guest`, answered on the host
//! with `docker exec` and `docker restart`.

use std::collections::{BTreeMap, HashMap};
use std::io::Write as _;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use futures::future::BoxFuture;
use parking_lot::Mutex;

use crate::exec;
use crate::guest::control::{self, Control, ProcessState};
use crate::image::{digest_hex, pinned_ref};
use crate::vmm::{GuestConn, ReplicaSpec, Started, Vmm, VmmError};

/// Label on every container this backend owns; [`Docker::prepare`] removes stale ones.
pub const MANAGED_LABEL: &str = "aho-cloud.managed";

/// Variables the microVM path points at the host egress proxy. No proxy runs next to Docker,
/// so passing them would break every outbound call.
const PROXY_ENV: [&str; 6] = ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy", "NO_PROXY", "no_proxy"];

/// Grace `docker restart` gives the process before killing it, in seconds.
const RESTART_GRACE_SECS: &str = "10";

#[derive(Clone, Debug)]
pub struct DockerConfig {
    /// The `docker` CLI.
    pub docker: PathBuf,
    /// Registry repository per project as the Docker daemon reaches it; `{project}` is substituted.
    pub repo_template: String,
    /// Holds one env file per replica (`env/<id>.env`, mode 0600).
    pub state_dir: PathBuf,
    /// Host address replica ports are published on.
    pub publish_ip: Ipv4Addr,
    /// Docker network replicas join; `None` uses the daemon's default bridge.
    pub network: Option<String>,
    pub mem_mib: u32,
    pub cpus: u8,
    pub pids_max: u32,
}

/// The Docker backend. See the module docs for what it does not enforce.
pub struct Docker {
    cfg: DockerConfig,
    /// Replica id → container port → published host address.
    published: Mutex<HashMap<String, HashMap<u16, SocketAddr>>>,
}

impl Docker {
    pub fn new(cfg: DockerConfig) -> Self {
        Docker { cfg, published: Mutex::new(HashMap::new()) }
    }

    /// Creates the env-file directory and removes containers left by an earlier agent run:
    /// the supervisor's state is in memory, so they are nobody's replicas any more.
    pub async fn prepare(&self) -> Result<(), VmmError> {
        let dir = self.env_dir();
        std::fs::create_dir_all(&dir).map_err(|e| VmmError::Io(dir.display().to_string(), e))?;
        restrict(&dir, 0o700)?;
        let stale = exec::run(&self.cfg.docker, ["ps", "-aq", "--filter", &format!("label={MANAGED_LABEL}=1")]).await?;
        let ids: Vec<&str> = stale.split_whitespace().collect();
        if !ids.is_empty() {
            tracing::info!(count = ids.len(), "removing containers from an earlier run");
            exec::run(&self.cfg.docker, ["rm", "-f"].into_iter().chain(ids.iter().copied())).await?;
        }
        Ok(())
    }

    fn env_dir(&self) -> PathBuf {
        self.cfg.state_dir.join("env")
    }

    fn env_file(&self, id: &str) -> PathBuf {
        self.env_dir().join(format!("{id}.env"))
    }

    /// `docker run` arguments for `spec`, reading its environment from `env_file`.
    pub fn run_args(&self, spec: &ReplicaSpec, env_file: &Path) -> Result<Vec<String>, VmmError> {
        digest_hex(&spec.image_digest)?;
        let mut args: Vec<String> = vec!["run".into(), "-d".into(), "--name".into(), spec.id.clone()];
        for (key, value) in [
            (MANAGED_LABEL, "1"),
            ("aho-cloud.project", spec.project.as_str()),
            ("aho-cloud.stage", spec.stage.as_str()),
            ("aho-cloud.process", spec.process.as_str()),
            ("aho-cloud.release", spec.release.as_str()),
        ] {
            args.push("--label".into());
            args.push(format!("{key}={value}"));
        }
        args.extend([
            "--env-file".into(),
            env_file.display().to_string(),
            "--memory".into(),
            format!("{}m", self.cfg.mem_mib),
            "--cpus".into(),
            self.cfg.cpus.to_string(),
            "--pids-limit".into(),
            self.cfg.pids_max.to_string(),
            // The supervisor replaces exited replicas; Docker must not restart them behind it.
            "--restart".into(),
            "no".into(),
        ]);
        if let Some(network) = &self.cfg.network {
            args.extend(["--network".into(), network.clone()]);
        }
        if let Some(port) = spec.guest.port {
            args.extend(["-p".into(), format!("{}::{port}", self.cfg.publish_ip)]);
        }
        if let Some(volume) = &spec.guest.volume {
            args.extend(["-v".into(), format!("{}:{}", volume_name(spec), volume.mount)]);
        }
        let image = pinned_ref(&self.cfg.repo_template, &spec.project, &spec.image_digest);
        match &spec.guest.command {
            Some(argv) => {
                let (program, rest) = argv.split_first().ok_or_else(|| VmmError::Docker("empty command".into()))?;
                args.extend(["--entrypoint".into(), program.clone(), image]);
                args.extend(rest.iter().cloned());
            }
            None => args.push(image),
        }
        Ok(args)
    }

    /// The published host address of `port` on container `id`, from `docker port`.
    async fn published_addr(&self, id: &str, port: u16) -> Result<SocketAddr, VmmError> {
        let out = exec::run(&self.cfg.docker, ["port", id, &format!("{port}/tcp")]).await?;
        out.lines()
            .filter_map(|l| l.trim().parse::<SocketAddr>().ok())
            .find(|a| a.ip() == std::net::IpAddr::V4(self.cfg.publish_ip))
            .ok_or_else(|| VmmError::Docker(format!("{id}: port {port} is not published on {}: {out:?}", self.cfg.publish_ip)))
    }

    async fn remove(&self, id: &str) -> Result<(), VmmError> {
        match exec::run(&self.cfg.docker, ["rm", "-f", id]).await {
            Ok(_) => Ok(()),
            Err(e) if e.detail.contains("No such container") => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

/// `aho-cloud-<project>-<stage>-<process>-<index>`: the same disk across restarts and releases.
pub fn volume_name(spec: &ReplicaSpec) -> String {
    format!("aho-cloud-{}-{}-{}-{}", spec.project, spec.stage.as_str(), spec.process, spec.index)
}

/// The replica's environment without the egress-proxy variables, as `KEY=value` lines.
/// Refuses what an env file cannot carry exactly.
pub fn env_file_contents(env: &BTreeMap<String, String>) -> Result<String, VmmError> {
    let mut out = String::new();
    for (key, value) in env.iter().filter(|(k, _)| !PROXY_ENV.contains(&k.as_str())) {
        let valid_key = key.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid_key {
            return Err(VmmError::Docker(format!("environment name `{key}` cannot go in an env file")));
        }
        if value.contains(['\n', '\r', '\0']) {
            return Err(VmmError::Docker(format!("the value of `{key}` has a line break; env files cannot carry it")));
        }
        out.push_str(key);
        out.push('=');
        out.push_str(value);
        out.push('\n');
    }
    Ok(out)
}

fn write_private(path: &Path, contents: &str) -> Result<(), VmmError> {
    let io = |e| VmmError::Io(path.display().to_string(), e);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path).map_err(io)?;
    file.write_all(contents.as_bytes()).map_err(io)?;
    restrict(path, 0o600)
}

fn restrict(path: &Path, mode: u32) -> Result<(), VmmError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(|e| VmmError::Io(path.display().to_string(), e))?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

impl Vmm for Docker {
    fn start(&self, spec: ReplicaSpec) -> BoxFuture<'_, Result<Started, VmmError>> {
        Box::pin(async move {
            let env_file = self.env_file(&spec.id);
            let args = self.run_args(&spec, &env_file)?;
            write_private(&env_file, &env_file_contents(&spec.guest.env)?)?;
            let image = pinned_ref(&self.cfg.repo_template, &spec.project, &spec.image_digest);
            let started = async {
                exec::run(&self.cfg.docker, ["pull", "-q", image.as_str()]).await?;
                exec::run(&self.cfg.docker, &args).await?;
                if let Some(port) = spec.guest.port {
                    let addr = self.published_addr(&spec.id, port).await?;
                    self.published.lock().entry(spec.id.clone()).or_default().insert(port, addr);
                }
                Ok::<_, VmmError>(())
            }
            .await;
            if let Err(e) = started {
                // Leave nothing behind a failed start: no container, no secrets on disk.
                if let Err(cleanup) = self.stop(&spec.id).await {
                    tracing::warn!(id = %spec.id, error = %cleanup, "cleanup after a failed start");
                }
                return Err(e);
            }
            Ok(Started { pid: None })
        })
    }

    fn stop<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), VmmError>> {
        Box::pin(async move {
            self.remove(id).await?;
            self.published.lock().remove(id);
            let env_file = self.env_file(id);
            match std::fs::remove_file(&env_file) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(VmmError::Io(env_file.display().to_string(), e)),
            }
        })
    }

    fn running<'a>(&'a self, id: &'a str) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            exec::run(&self.cfg.docker, ["inspect", "-f", "{{.State.Running}}", id]).await.is_ok_and(|out| out.trim() == "true")
        })
    }

    fn connect<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<GuestConn, VmmError>> {
        Box::pin(async move {
            let (host, guest) = tokio::io::duplex(control::MAX_LINE * 2);
            let answer = ContainerControl { docker: self.cfg.docker.clone(), id: id.to_owned() };
            tokio::spawn(async move {
                if let Err(e) = control::serve(guest, &answer).await {
                    tracing::debug!(id = %answer.id, error = %e, "container control connection ended");
                }
            });
            Ok(Box::new(host) as GuestConn)
        })
    }

    fn endpoint(&self, id: &str, ip: Ipv4Addr, port: u16) -> SocketAddr {
        self.published.lock().get(id).and_then(|ports| ports.get(&port).copied()).unwrap_or_else(|| SocketAddr::new(ip.into(), port))
    }
}

/// The guest side of the control protocol for one container, answered from the host.
struct ContainerControl {
    docker: PathBuf,
    id: String,
}

impl Control for ContainerControl {
    fn health(&self, argv: Vec<String>, timeout: Duration) -> BoxFuture<'_, Result<i32, String>> {
        Box::pin(async move {
            let status = tokio::process::Command::new(&self.docker)
                .arg("exec")
                .arg(&self.id)
                .args(&argv)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .status();
            match tokio::time::timeout(timeout, status).await {
                Ok(Ok(status)) => status.code().ok_or_else(|| "health command killed by a signal".to_owned()),
                Ok(Err(e)) => Err(format!("docker exec: {e}")),
                Err(_) => Err(format!("health command timed out after {} ms", timeout.as_millis())),
            }
        })
    }

    fn restart(&self) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            exec::run(&self.docker, ["restart", "-t", RESTART_GRACE_SECS, self.id.as_str()]).await.map(drop).map_err(|e| e.to_string())
        })
    }

    /// Docker supervises the process; the guest-side counters do not exist here.
    fn status(&self) -> ProcessState {
        ProcessState::default()
    }
}
