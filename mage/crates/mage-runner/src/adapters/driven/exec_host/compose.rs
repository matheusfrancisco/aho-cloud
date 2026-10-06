//! Docker Compose wrapper (executor.md "Services" → "Compose service", "v1 isolation rules" →
//! Containers). Run by the daemon only, never by the agent: argv only, cleared environment with
//! the daemon's `PATH` and `HOME` (plus `DOCKER_HOST` / `DOCKER_CONFIG` when set).

use std::ffi::{OsStr, OsString};
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Component, Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt as _};
use tokio::process::{Child, Command};

use crate::ports::executor::ExecError;

/// `version`, `config`, `ps`, `ls`.
const QUERY_TIMEOUT: Duration = Duration::from_secs(60);
/// `pull`, `build`.
const FETCH_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// `up`, `stop`, `down`.
const LIFECYCLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Bytes of stderr kept for error messages.
const STDERR_TAIL: usize = 2048;
/// Project name prefix of every Mage-owned compose project.
const PROJECT_PREFIX: &str = "mage-";

/// Daemon environment variables forwarded to `docker`.
const FORWARDED_ENV: [&str; 4] = ["PATH", "HOME", "DOCKER_HOST", "DOCKER_CONFIG"];

#[derive(Debug, Clone)]
pub(crate) struct Compose {
    docker: PathBuf,
}

#[derive(Debug, Clone)]
pub(crate) struct ComposeProject {
    /// `mage-<slug>`.
    pub name: String,
    /// Project directory (the staging or fork tree).
    pub dir: PathBuf,
    /// The user's compose file(s), absolute.
    pub files: Vec<PathBuf>,
    /// Mage-generated override, applied after the user files.
    pub override_file: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposeModel {
    /// Service names, sorted.
    pub services: Vec<String>,
    /// Top-level named volumes, sorted.
    pub volumes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServicePorts {
    pub service: String,
    /// `(host, container)`; the host side is always bound to 127.0.0.1.
    pub ports: Vec<(u16, u16)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContainerState {
    Running,
    Exited(i32),
    Absent,
}

impl Compose {
    /// `docker` defaults to the bare name, resolved through the daemon's `PATH` at spawn time.
    pub(crate) fn new(docker: Option<PathBuf>) -> Self {
        Self { docker: docker.unwrap_or_else(|| PathBuf::from("docker")) }
    }

    /// `docker compose version --short` succeeds → the version string; otherwise
    /// `Unavailable("docker compose")`.
    pub(crate) async fn available(&self) -> Result<String, ExecError> {
        let unavailable = || ExecError::Unavailable("docker compose".into());
        let out = self
            .run("version", args(["version", "--short"]), QUERY_TIMEOUT, true)
            .await
            .map_err(|_| unavailable())?;
        let version = String::from_utf8_lossy(&out).trim().to_owned();
        if version.is_empty() { Err(unavailable()) } else { Ok(version) }
    }

    /// Renders the user's model with `config --format json` (override excluded: the user model is
    /// what gets validated) and rejects anything that breaks the container isolation rules.
    pub(crate) async fn validate(&self, project: &ComposeProject, tree_root: &Path) -> Result<ComposeModel, ExecError> {
        let mut argv = project_args(project, false);
        argv.extend(args(["config", "--format", "json"]));
        let out = self.run("config", argv, QUERY_TIMEOUT, true).await?;
        let json: Value = serde_json::from_slice(&out)
            .map_err(|e| ExecError::Unavailable(format!("docker compose config: unparseable output: {e}")))?;
        check_model(&json, tree_root)
    }

    /// Writes the override file (mode 0600): loopback-only port publishing per service,
    /// `pull_policy: never` when `pull_never`, and every named volume redefined as a local bind to
    /// its directory (created here).
    pub(crate) fn write_override(
        path: &Path,
        services: &[ServicePorts],
        volumes: &[(String, PathBuf)],
        pull_never: bool,
    ) -> Result<(), ExecError> {
        for (name, dir) in volumes {
            if !dir.is_absolute() {
                return Err(ExecError::Invalid(format!("volume {name}: directory {} is not absolute", dir.display())));
            }
            std::fs::create_dir_all(dir)?;
        }
        let yaml = render_override(services, volumes, pull_never)?;
        let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
        // `mode` only applies on creation; an existing file keeps its mode otherwise.
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        file.write_all(yaml.as_bytes())?;
        file.sync_all()?;
        Ok(())
    }

    /// `pull --ignore-buildable`, retried without the flag on Compose versions that lack it.
    pub(crate) async fn pull(&self, project: &ComposeProject) -> Result<(), ExecError> {
        let mut argv = project_args(project, true);
        argv.extend(args(["pull", "--ignore-buildable"]));
        match self.run("pull", argv, FETCH_TIMEOUT, false).await {
            Err(ExecError::Unavailable(msg)) if msg.contains("unknown flag") => {
                let mut argv = project_args(project, true);
                argv.push("pull".into());
                self.run("pull", argv, FETCH_TIMEOUT, false).await.map(drop)
            }
            other => other.map(drop),
        }
    }

    pub(crate) async fn build(&self, project: &ComposeProject) -> Result<(), ExecError> {
        let mut argv = project_args(project, true);
        argv.push("build".into());
        self.run("build", argv, FETCH_TIMEOUT, false).await.map(drop)
    }

    /// `up -d --no-deps <service>`.
    pub(crate) async fn up(&self, project: &ComposeProject, service: &str) -> Result<(), ExecError> {
        let mut argv = project_args(project, true);
        argv.extend(args(["up", "-d", "--no-deps", service]));
        self.run("up", argv, LIFECYCLE_TIMEOUT, false).await.map(drop)
    }

    /// `stop -t <secs> <service>`.
    pub(crate) async fn stop(&self, project: &ComposeProject, service: &str, timeout_s: u32) -> Result<(), ExecError> {
        let mut argv = project_args(project, true);
        argv.extend(args(["stop", "-t", &timeout_s.to_string(), service]));
        // A long grace period must not be cut short by the command bound.
        let limit = LIFECYCLE_TIMEOUT.max(Duration::from_secs(u64::from(timeout_s) + 60));
        self.run("stop", argv, limit, false).await.map(drop)
    }

    /// `-p <name> down -v --remove-orphans`: by project name only, so it works after the files
    /// are gone.
    pub(crate) async fn down(&self, project_name: &str) -> Result<(), ExecError> {
        let argv = args(["-p", project_name, "down", "-v", "--remove-orphans"]);
        self.run("down", argv, LIFECYCLE_TIMEOUT, false).await.map(drop)
    }

    /// Spawns `logs -f --no-color --no-log-prefix <service>` with stdout and stderr piped and
    /// `kill_on_drop`; the caller pumps both into the service log.
    pub(crate) fn spawn_logs(&self, project: &ComposeProject, service: &str) -> Result<Child, ExecError> {
        let mut argv = project_args(project, true);
        argv.extend(args(["logs", "-f", "--no-color", "--no-log-prefix", service]));
        self.command(&argv)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| spawn_error("logs", &e))
    }

    /// `ls --all --format json` → names of Mage-owned projects.
    pub(crate) async fn mage_projects(&self) -> Result<Vec<String>, ExecError> {
        let out = self.run("ls", args(["ls", "--all", "--format", "json"]), QUERY_TIMEOUT, true).await?;
        parse_ls(&String::from_utf8_lossy(&out))
    }

    /// `ps --all --format json <service>` → running / exited(code) / absent.
    pub(crate) async fn service_state(&self, project: &ComposeProject, service: &str) -> Result<ContainerState, ExecError> {
        let mut argv = project_args(project, true);
        argv.extend(args(["ps", "--all", "--format", "json", service]));
        let out = self.run("ps", argv, QUERY_TIMEOUT, true).await?;
        parse_ps(&String::from_utf8_lossy(&out), service)
    }

    /// `docker compose <argv>` with the cleared environment, in its own process group.
    fn command(&self, argv: &[OsString]) -> Command {
        let mut cmd = Command::new(&self.docker);
        cmd.arg("compose").args(argv).env_clear().current_dir("/").stdin(Stdio::null()).process_group(0).kill_on_drop(true);
        for key in FORWARDED_ENV {
            if let Some(value) = std::env::var_os(key) {
                cmd.env(key, value);
            }
        }
        cmd
    }

    /// Runs one command to completion within `limit`. Returns stdout (empty unless
    /// `capture_stdout`); failures carry the stderr tail.
    async fn run(&self, verb: &str, argv: Vec<OsString>, limit: Duration, capture_stdout: bool) -> Result<Vec<u8>, ExecError> {
        let mut child = self
            .command(&argv)
            .stdout(if capture_stdout { Stdio::piped() } else { Stdio::null() })
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| spawn_error(verb, &e))?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let finished = tokio::time::timeout(limit, async {
            let (out, tail, status) = tokio::join!(read_all(stdout), read_tail(stderr), child.wait());
            (out, tail, status)
        })
        .await;
        let (out, tail, status) = match finished {
            Ok(done) => done,
            Err(_) => {
                kill_group(&child);
                return Err(ExecError::Unavailable(format!("docker compose {verb}: timed out after {}s", limit.as_secs())));
            }
        };
        let status = status.map_err(|e| ExecError::Unavailable(format!("docker compose {verb}: {e}")))?;
        if !status.success() {
            return Err(ExecError::Unavailable(format!("docker compose {verb}: {}", failure_detail(&tail, status))));
        }
        out.map_err(|e| ExecError::Unavailable(format!("docker compose {verb}: reading output: {e}")))
    }
}

fn args<'a>(items: impl IntoIterator<Item = &'a str>) -> Vec<OsString> {
    items.into_iter().map(OsString::from).collect()
}

/// `-f <file>… [-f <override>] -p <name> --project-directory <dir>`.
fn project_args(project: &ComposeProject, with_override: bool) -> Vec<OsString> {
    let mut argv = Vec::with_capacity(2 * project.files.len() + 6);
    let overrides = project.override_file.iter().filter(|_| with_override);
    for file in project.files.iter().chain(overrides) {
        argv.push(OsString::from("-f"));
        argv.push(file.as_os_str().to_owned());
    }
    argv.push("-p".into());
    argv.push(OsString::from(&project.name));
    argv.push("--project-directory".into());
    argv.push(project.dir.as_os_str().to_owned());
    argv
}

fn spawn_error(verb: &str, err: &std::io::Error) -> ExecError {
    if err.kind() == std::io::ErrorKind::NotFound {
        ExecError::Unavailable("docker".into())
    } else {
        ExecError::Unavailable(format!("docker compose {verb}: {err}"))
    }
}

fn kill_group(child: &Child) {
    use nix::sys::signal::{Signal, killpg};
    use nix::unistd::Pid;
    if let Some(pid) = child.id().and_then(|id| i32::try_from(id).ok()) {
        // The group may already be gone; `kill_on_drop` covers the leader either way.
        let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
    }
}

async fn read_all(pipe: Option<impl AsyncRead + Unpin>) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    if let Some(mut pipe) = pipe {
        pipe.read_to_end(&mut buf).await?;
    }
    Ok(buf)
}

/// Drains the pipe, keeping only (roughly) the last `STDERR_TAIL` bytes.
async fn read_tail(pipe: Option<impl AsyncRead + Unpin>) -> Vec<u8> {
    let mut tail = Vec::with_capacity(2 * STDERR_TAIL);
    let Some(mut pipe) = pipe else { return tail };
    let mut chunk = [0u8; 8192];
    loop {
        match pipe.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                tail.extend_from_slice(&chunk[..n]);
                if tail.len() > 2 * STDERR_TAIL {
                    tail.drain(..tail.len() - STDERR_TAIL);
                }
            }
        }
    }
    tail
}

/// The last ≤ `STDERR_TAIL` bytes of stderr (on a char boundary), or the exit status when silent.
fn failure_detail(stderr: &[u8], status: ExitStatus) -> String {
    let text = String::from_utf8_lossy(stderr);
    let text = text.trim();
    if text.is_empty() {
        return status.to_string();
    }
    let mut start = text.len().saturating_sub(STDERR_TAIL);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_owned()
}

// ---------------------------------------------------------------------------------------------
// Model validation
// ---------------------------------------------------------------------------------------------

/// Validates the JSON printed by `docker compose config --format json` against the container
/// isolation rules and returns the service and named-volume names.
pub(crate) fn check_model(json: &Value, tree_root: &Path) -> Result<ComposeModel, ExecError> {
    let root = json.as_object().ok_or_else(|| ExecError::Invalid("compose model is not a JSON object".into()))?;
    let tree = canonical_tree(tree_root);
    let empty = serde_json::Map::new();
    let section = |key: &str| -> Result<&serde_json::Map<String, Value>, ExecError> {
        match root.get(key) {
            None | Some(Value::Null) => Ok(&empty),
            Some(Value::Object(map)) => Ok(map),
            Some(_) => Err(ExecError::Invalid(format!("compose model: `{key}` is not a mapping"))),
        }
    };

    let services = section("services")?;
    for (name, service) in services {
        check_service(service, &tree).map_err(|reason| ExecError::Invalid(format!("{name}: {reason}")))?;
    }

    let volumes = section("volumes")?;
    for (name, volume) in volumes {
        check_volume(volume, &tree).map_err(|reason| ExecError::Invalid(format!("volume {name}: {reason}")))?;
    }

    for (name, network) in section("networks")? {
        if truthy(network.get("external")) {
            return Err(ExecError::Invalid(format!("network {name}: external networks are not allowed")));
        }
        if network.get("driver").and_then(Value::as_str) == Some("host") || network.get("name").and_then(Value::as_str) == Some("host") {
            return Err(ExecError::Invalid(format!("network {name}: the host network is not allowed")));
        }
    }

    for kind in ["secrets", "configs"] {
        for (name, entry) in section(kind)? {
            if let Some(file) = entry.get("file").and_then(Value::as_str) {
                if !inside(&tree, Path::new(file)) {
                    return Err(ExecError::Invalid(format!("{kind} {name}: file {file} is outside the tree")));
                }
            }
        }
    }

    let mut service_names: Vec<String> = services.keys().cloned().collect();
    service_names.sort();
    let mut volume_names: Vec<String> = volumes.keys().cloned().collect();
    volume_names.sort();
    Ok(ComposeModel { services: service_names, volumes: volume_names })
}

fn check_service(service: &Value, tree: &Path) -> Result<(), String> {
    let str_field = |key: &str| service.get(key).and_then(Value::as_str);
    if truthy(service.get("privileged")) {
        return Err("privileged is not allowed".into());
    }
    if non_empty(service.get("cap_add")) {
        return Err("cap_add is not allowed".into());
    }
    if non_empty(service.get("devices")) {
        return Err("devices are not allowed".into());
    }
    if non_empty(service.get("volumes_from")) {
        return Err("volumes_from is not allowed".into());
    }
    if let Some(mode) = str_field("network_mode") {
        if mode == "host" || mode.starts_with("service:") || mode.starts_with("container:") {
            return Err(format!("network_mode {mode:?} is not allowed"));
        }
    }
    for key in ["pid", "ipc"] {
        if let Some(mode) = str_field(key) {
            if mode == "host" || mode.starts_with("container:") {
                return Err(format!("{key} {mode:?} is not allowed"));
            }
        }
    }
    for key in ["userns_mode", "uts", "cgroup"] {
        if str_field(key) == Some("host") {
            return Err(format!("{key} \"host\" is not allowed"));
        }
    }
    if let Some(opts) = service.get("security_opt").and_then(Value::as_array) {
        if let Some(opt) = opts.iter().filter_map(Value::as_str).find(|o| o.contains("unconfined")) {
            return Err(format!("security_opt {opt:?} is not allowed"));
        }
    }
    for mount in service.get("volumes").and_then(Value::as_array).into_iter().flatten() {
        check_mount(mount, tree)?;
    }
    if let Some(build) = service.get("build") {
        check_build(build, tree)?;
    }
    Ok(())
}

fn check_mount(mount: &Value, tree: &Path) -> Result<(), String> {
    let source = mount.get("source").and_then(Value::as_str);
    let target = mount.get("target").and_then(Value::as_str);
    if let Some(sock) = [source, target].into_iter().flatten().find(|p| p.trim_end_matches('/').ends_with("docker.sock")) {
        return Err(format!("docker socket mount {sock} is not allowed"));
    }
    match mount.get("type").and_then(Value::as_str) {
        Some("bind") => match source {
            Some(src) if inside(tree, Path::new(src)) => Ok(()),
            Some(src) => Err(format!("bind mount {src} is outside the tree")),
            None => Err("bind mount without a source".into()),
        },
        Some("npipe") => Err("npipe mounts are not allowed".into()),
        _ => Ok(()),
    }
}

/// Build contexts are copied into images: local ones must stay inside the tree.
fn check_build(build: &Value, tree: &Path) -> Result<(), String> {
    let context = match build {
        Value::String(ctx) => Some(ctx.as_str()),
        _ => build.get("context").and_then(Value::as_str),
    };
    let local_context = context.filter(|ctx| !is_remote_context(ctx)).map(|ctx| tree.join(ctx));
    if let Some(ctx) = &local_context {
        if !inside(tree, ctx) {
            return Err(format!("build context {} is outside the tree", ctx.display()));
        }
        if let Some(dockerfile) = build.get("dockerfile").and_then(Value::as_str) {
            // Relative to the context; absolute paths replace it.
            if !inside(tree, &ctx.join(dockerfile)) {
                return Err(format!("dockerfile {dockerfile} is outside the tree"));
            }
        }
    }
    let extra: Vec<&str> = match build.get("additional_contexts") {
        Some(Value::Object(map)) => map.values().filter_map(Value::as_str).collect(),
        Some(Value::Array(items)) => {
            items.iter().filter_map(Value::as_str).map(|kv| kv.split_once('=').map_or(kv, |(_, v)| v)).collect()
        }
        _ => Vec::new(),
    };
    for value in extra {
        let local = match value.strip_prefix("oci-layout://") {
            Some(path) => Some(path),
            None if value.starts_with("service:") || value.starts_with("target:") || is_remote_context(value) => None,
            None => Some(value),
        };
        if let Some(path) = local {
            if !inside(tree, &tree.join(path)) {
                return Err(format!("build context {value} is outside the tree"));
            }
        }
    }
    Ok(())
}

fn is_remote_context(ctx: &str) -> bool {
    ctx.contains("://") || ctx.starts_with("git@") || ctx.starts_with("github.com/")
}

fn check_volume(volume: &Value, tree: &Path) -> Result<(), String> {
    if truthy(volume.get("external")) {
        return Err("external volumes are not allowed".into());
    }
    if let Some(driver) = volume.get("driver").and_then(Value::as_str) {
        if driver != "local" {
            return Err(format!("driver {driver:?} is not allowed (only \"local\")"));
        }
    }
    // A local volume with a `device` is a host mount in disguise.
    if let Some(device) = volume.get("driver_opts").and_then(|o| o.get("device")).and_then(Value::as_str) {
        if !inside(tree, Path::new(device)) {
            return Err(format!("device {device} is outside the tree"));
        }
    }
    Ok(())
}

/// `true`, a non-empty string other than "false", or a mapping (old-style `external: {name}`).
fn truthy(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => !s.is_empty() && !s.eq_ignore_ascii_case("false"),
        Some(Value::Object(_)) => true,
        _ => false,
    }
}

fn non_empty(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(map)) => !map.is_empty(),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

fn canonical_tree(tree_root: &Path) -> PathBuf {
    resolve(&lexical(tree_root, Path::new("/")))
}

/// Whether `path` (absolute, or relative to `tree`) resolves to `tree` or below it.
fn inside(tree: &Path, path: &Path) -> bool {
    resolve(&lexical(path, tree)).starts_with(tree)
}

/// `base.join(path)` with `.` and `..` folded away, without touching the filesystem.
fn lexical(path: &Path, base: &Path) -> PathBuf {
    let joined = if path.is_absolute() { path.to_path_buf() } else { base.join(path) };
    let mut out = PathBuf::from("/");
    for component in joined.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::ParentDir => {
                out.pop();
            }
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
    out
}

/// Resolves symlinks through the longest existing ancestor of a lexically clean absolute path;
/// the missing remainder is appended unchanged (Docker creates missing bind sources there).
fn resolve(clean: &Path) -> PathBuf {
    let mut existing = clean;
    let mut missing: Vec<&OsStr> = Vec::new();
    loop {
        if let Ok(real) = std::fs::canonicalize(existing) {
            let mut out = real;
            out.extend(missing.iter().rev());
            return out;
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                missing.push(name);
                existing = parent;
            }
            _ => return clean.to_path_buf(),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Override file
// ---------------------------------------------------------------------------------------------

fn render_override(services: &[ServicePorts], volumes: &[(String, PathBuf)], pull_never: bool) -> Result<String, ExecError> {
    let mut yaml = String::from("# Generated by mage; not part of the repository.\n");
    if services.is_empty() {
        yaml.push_str("services: {}\n");
    } else {
        yaml.push_str("services:\n");
    }
    for entry in services {
        let ports: Vec<String> =
            entry.ports.iter().map(|(host, container)| quote(&format!("127.0.0.1:{host}:{container}"))).collect();
        yaml.push_str(&format!("  {}:\n    ports: !override [{}]\n", quote(&entry.service), ports.join(", ")));
        if pull_never {
            yaml.push_str("    pull_policy: \"never\"\n");
        }
    }
    if !volumes.is_empty() {
        yaml.push_str("volumes:\n");
    }
    for (name, dir) in volumes {
        let device = dir
            .to_str()
            .ok_or_else(|| ExecError::Invalid(format!("volume {name}: directory {} is not UTF-8", dir.display())))?;
        yaml.push_str(&format!(
            "  {}: !override\n    driver: \"local\"\n    driver_opts:\n      type: \"none\"\n      o: \"bind\"\n      device: {}\n",
            quote(name),
            quote(device),
        ));
    }
    Ok(yaml)
}

/// JSON string literals are valid YAML double-quoted scalars.
fn quote(s: &str) -> String {
    Value::String(s.to_owned()).to_string()
}

// ---------------------------------------------------------------------------------------------
// `ps` / `ls` output
// ---------------------------------------------------------------------------------------------

/// Either one JSON array, or one JSON object per line (newer Compose).
fn json_records(out: &str) -> Result<Vec<Value>, ExecError> {
    let trimmed = out.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let bad = |e: serde_json::Error| ExecError::Unavailable(format!("docker compose: unparseable JSON output: {e}"));
    if trimmed.starts_with('[') {
        return match serde_json::from_str(trimmed).map_err(bad)? {
            Value::Array(items) => Ok(items),
            other => Ok(vec![other]),
        };
    }
    trimmed.lines().map(str::trim).filter(|l| !l.is_empty()).map(|l| serde_json::from_str(l).map_err(bad)).collect()
}

fn parse_ps(out: &str, service: &str) -> Result<ContainerState, ExecError> {
    let mut state = ContainerState::Absent;
    for record in json_records(out)? {
        if record.get("Service").and_then(Value::as_str).is_some_and(|s| s != service) {
            continue;
        }
        let exit = record.get("ExitCode").and_then(Value::as_i64).and_then(|c| i32::try_from(c).ok()).unwrap_or(-1);
        match record.get("State").and_then(Value::as_str).unwrap_or_default() {
            "running" | "paused" => return Ok(ContainerState::Running),
            "exited" | "dead" | "restarting" => {
                if state == ContainerState::Absent {
                    state = ContainerState::Exited(exit);
                }
            }
            // "created", "removing": no container has run (or one is going away).
            _ => {}
        }
    }
    Ok(state)
}

fn parse_ls(out: &str) -> Result<Vec<String>, ExecError> {
    Ok(json_records(out)?
        .iter()
        .filter_map(|r| r.get("Name").and_then(Value::as_str))
        .filter(|name| name.starts_with(PROJECT_PREFIX))
        .map(str::to_owned)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("data")).unwrap();
        dir
    }

    fn reject(model: Value, tree: &Path, needle: &str) {
        match check_model(&model, tree) {
            Err(ExecError::Invalid(msg)) => assert!(msg.contains(needle), "{msg:?} lacks {needle:?}"),
            other => panic!("expected Invalid containing {needle:?}, got {other:?}"),
        }
    }

    fn with_service(service: Value) -> Value {
        json!({ "name": "mage-x", "services": { "api": service } })
    }

    #[test]
    fn accepts_bind_inside_tree_and_named_volume() {
        let t = tree();
        let root = t.path();
        let model = json!({
            "services": {
                "web": {
                    "image": "alpine",
                    "network_mode": "bridge",
                    "pid": "service:db",
                    "security_opt": ["no-new-privileges:true"],
                    "build": { "context": root.to_str().unwrap(), "dockerfile": "Dockerfile" },
                    "volumes": [
                        { "type": "bind", "source": root.join("data").to_str().unwrap(), "target": "/data" },
                        { "type": "bind", "source": root.join("not/yet").to_str().unwrap(), "target": "/later" },
                        { "type": "volume", "source": "dbdata", "target": "/var/lib/x" },
                        { "type": "tmpfs", "target": "/tmp" }
                    ]
                },
                "db": { "image": "postgres", "cap_add": [], "devices": [] }
            },
            "volumes": { "dbdata": { "name": "mage-x_dbdata" }, "cache": { "driver": "local" } },
            "networks": { "default": { "name": "mage-x_default" } }
        });
        let got = check_model(&model, root).unwrap();
        assert_eq!(got.services, vec!["db".to_owned(), "web".to_owned()]);
        assert_eq!(got.volumes, vec!["cache".to_owned(), "dbdata".to_owned()]);
    }

    #[test]
    fn rejects_privileged_caps_devices_and_volumes_from() {
        let t = tree();
        reject(with_service(json!({ "privileged": true })), t.path(), "api: privileged");
        reject(with_service(json!({ "cap_add": ["NET_ADMIN"] })), t.path(), "api: cap_add");
        reject(
            with_service(json!({ "devices": [{ "source": "/dev/fuse", "target": "/dev/fuse", "permissions": "rwm" }] })),
            t.path(),
            "api: devices",
        );
        reject(with_service(json!({ "volumes_from": ["other"] })), t.path(), "api: volumes_from");
        assert!(check_model(&with_service(json!({ "privileged": false })), t.path()).is_ok());
    }

    #[test]
    fn rejects_host_and_foreign_namespaces() {
        let t = tree();
        for mode in ["host", "service:db", "container:abc"] {
            reject(with_service(json!({ "network_mode": mode })), t.path(), "network_mode");
        }
        reject(with_service(json!({ "pid": "host" })), t.path(), "api: pid");
        reject(with_service(json!({ "ipc": "host" })), t.path(), "api: ipc");
        reject(with_service(json!({ "userns_mode": "host" })), t.path(), "api: userns_mode");
        reject(with_service(json!({ "security_opt": ["seccomp:unconfined"] })), t.path(), "security_opt");
        reject(with_service(json!({ "security_opt": ["apparmor=unconfined"] })), t.path(), "security_opt");
    }

    #[test]
    fn rejects_binds_outside_tree() {
        let t = tree();
        let root = t.path();
        reject(with_service(json!({ "volumes": [{ "type": "bind", "source": "/etc", "target": "/x" }] })), root, "outside the tree");
        let dotdot = format!("{}/data/../../escape", root.display());
        reject(with_service(json!({ "volumes": [{ "type": "bind", "source": dotdot, "target": "/x" }] })), root, "outside the tree");
        // A symlink inside the tree pointing out is resolved, including for missing leaves under it.
        std::os::unix::fs::symlink("/etc", root.join("link")).unwrap();
        for src in [root.join("link"), root.join("link/missing/child")] {
            let mount = json!({ "volumes": [{ "type": "bind", "source": src.to_str().unwrap(), "target": "/x" }] });
            reject(with_service(mount), root, "outside the tree");
        }
        reject(json!({ "services": {}, "secrets": { "s": { "file": "/etc/passwd" } } }), root, "secrets s");
        reject(with_service(json!({ "build": { "context": "/", "dockerfile": "Dockerfile" } })), root, "build context");
        reject(
            with_service(json!({ "build": { "context": root.to_str().unwrap(), "additional_contexts": { "x": "/etc" } } })),
            root,
            "build context /etc",
        );
        assert!(
            check_model(&with_service(json!({ "build": { "context": "https://github.com/x/y.git" } })), root).is_ok()
        );
    }

    #[test]
    fn rejects_docker_socket_mounts() {
        let t = tree();
        let root = t.path();
        let sock = root.join("docker.sock");
        let inside_src = json!({ "volumes": [{ "type": "bind", "source": sock.to_str().unwrap(), "target": "/s" }] });
        reject(with_service(inside_src), root, "docker socket");
        let by_target = json!({ "volumes": [{ "type": "volume", "source": "v", "target": "/var/run/docker.sock" }] });
        reject(with_service(by_target), root, "docker socket");
    }

    #[test]
    fn rejects_foreign_volumes() {
        let t = tree();
        reject(json!({ "services": {}, "volumes": { "v": { "driver": "nfs" } } }), t.path(), "volume v: driver");
        reject(json!({ "services": {}, "volumes": { "v": { "external": true } } }), t.path(), "volume v: external");
        reject(json!({ "services": {}, "volumes": { "v": { "external": { "name": "x" } } } }), t.path(), "external");
        let device = json!({ "services": {}, "volumes": { "v": { "driver_opts": { "type": "none", "o": "bind", "device": "/etc" } } } });
        reject(device, t.path(), "volume v: device");
        reject(json!({ "services": {}, "networks": { "h": { "name": "host", "external": false } } }), t.path(), "network h");
    }

    #[test]
    fn override_golden() {
        let services = vec![
            ServicePorts { service: "api".into(), ports: vec![(20001, 8080), (20002, 9090)] },
            ServicePorts { service: "we\"b".into(), ports: vec![] },
        ];
        let volumes = vec![("dbdata".to_owned(), PathBuf::from("/srv/fork/state/volumes/dbdata"))];
        let got = render_override(&services, &volumes, true).unwrap();
        let want = concat!(
            "# Generated by mage; not part of the repository.\n",
            "services:\n",
            "  \"api\":\n",
            "    ports: !override [\"127.0.0.1:20001:8080\", \"127.0.0.1:20002:9090\"]\n",
            "    pull_policy: \"never\"\n",
            "  \"we\\\"b\":\n",
            "    ports: !override []\n",
            "    pull_policy: \"never\"\n",
            "volumes:\n",
            "  \"dbdata\": !override\n",
            "    driver: \"local\"\n",
            "    driver_opts:\n",
            "      type: \"none\"\n",
            "      o: \"bind\"\n",
            "      device: \"/srv/fork/state/volumes/dbdata\"\n",
        );
        assert_eq!(got, want);
        assert!(!render_override(&services, &[], false).unwrap().contains("pull_policy"));
    }

    #[test]
    fn write_override_creates_dirs_with_private_mode() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("compose.override.yml");
        std::fs::write(&path, "stale").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let vol = t.path().join("state/volumes/dbdata");
        let services = [ServicePorts { service: "api".into(), ports: vec![(20001, 80)] }];
        Compose::write_override(&path, &services, &[("dbdata".into(), vol.clone())], false).unwrap();
        assert!(vol.is_dir());
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert!(std::fs::read_to_string(&path).unwrap().contains("127.0.0.1:20001:80"));
        let relative = Compose::write_override(&path, &[], &[("v".into(), PathBuf::from("rel"))], false);
        assert!(matches!(relative, Err(ExecError::Invalid(_))));
    }

    #[test]
    fn ps_parses_array_and_ndjson() {
        let array = r#"[{"Name":"mage-x-api-1","Service":"api","State":"running","ExitCode":0},
                        {"Name":"mage-x-db-1","Service":"db","State":"exited","ExitCode":3}]"#;
        assert_eq!(parse_ps(array, "api").unwrap(), ContainerState::Running);
        assert_eq!(parse_ps(array, "db").unwrap(), ContainerState::Exited(3));
        assert_eq!(parse_ps(array, "cache").unwrap(), ContainerState::Absent);
        let ndjson = "{\"Service\":\"api\",\"State\":\"exited\",\"ExitCode\":137}\n{\"Service\":\"api\",\"State\":\"running\",\"ExitCode\":0}\n";
        assert_eq!(parse_ps(ndjson, "api").unwrap(), ContainerState::Running);
        assert_eq!(parse_ps("{\"Service\":\"api\",\"State\":\"exited\",\"ExitCode\":1}\n", "api").unwrap(), ContainerState::Exited(1));
        assert_eq!(parse_ps("{\"Service\":\"api\",\"State\":\"created\",\"ExitCode\":0}", "api").unwrap(), ContainerState::Absent);
        assert_eq!(parse_ps("", "api").unwrap(), ContainerState::Absent);
        assert_eq!(parse_ps("[]\n", "api").unwrap(), ContainerState::Absent);
        assert!(matches!(parse_ps("not json", "api"), Err(ExecError::Unavailable(_))));
    }

    #[test]
    fn ls_keeps_mage_projects() {
        let out = r#"[{"Name":"mage-abc","Status":"running(1)","ConfigFiles":"/x/compose.yml"},
                      {"Name":"other","Status":"exited(1)","ConfigFiles":"/y.yml"},
                      {"Name":"mage-def","Status":"exited(2)","ConfigFiles":""}]"#;
        assert_eq!(parse_ls(out).unwrap(), vec!["mage-abc".to_owned(), "mage-def".to_owned()]);
        assert_eq!(parse_ls("{\"Name\":\"mage-z\"}\n{\"Name\":\"z\"}\n").unwrap(), vec!["mage-z".to_owned()]);
        assert!(parse_ls("[]").unwrap().is_empty());
    }

    #[test]
    fn failure_detail_keeps_the_tail_on_a_char_boundary() {
        use std::os::unix::process::ExitStatusExt as _;
        let status = ExitStatus::from_raw(1 << 8);
        let long = format!("{}é{}", "x".repeat(10), "y".repeat(STDERR_TAIL - 1));
        let detail = failure_detail(long.as_bytes(), status);
        assert!(detail.len() <= STDERR_TAIL && detail.ends_with('y') && !detail.contains('x'));
        assert!(failure_detail(b"  \n", status).contains('1'));
    }

    #[tokio::test]
    async fn missing_docker_binary_is_unavailable_docker() {
        let compose = Compose::new(Some(PathBuf::from("/nonexistent/mage-docker")));
        assert_eq!(compose.down("mage-x").await, Err(ExecError::Unavailable("docker".into())));
        assert_eq!(compose.available().await, Err(ExecError::Unavailable("docker compose".into())));
    }

    /// Opt-in: needs a working Docker daemon with Compose ≥ 2.24 (`MAGE_TEST_DOCKER=1`).
    #[tokio::test]
    async fn docker_lifecycle() {
        if std::env::var("MAGE_TEST_DOCKER").as_deref() != Ok("1") {
            eprintln!("skipping docker_lifecycle: set MAGE_TEST_DOCKER=1 to run");
            return;
        }
        let compose = Compose::new(None);
        compose.available().await.unwrap();

        let t = tempfile::tempdir().unwrap();
        let tree = t.path().join("tree");
        std::fs::create_dir_all(tree.join("data")).unwrap();
        let file = tree.join("compose.yml");
        std::fs::write(
            &file,
            "services:\n  app:\n    image: alpine:3.20\n    command: [\"sleep\", \"300\"]\n    ports: [\"80\"]\n    volumes:\n      - ./data:/data\n      - appdata:/state\nvolumes:\n  appdata: {}\n",
        )
        .unwrap();
        let name = format!("mage-test-{}", uuid::Uuid::new_v4().simple());
        let override_file = t.path().join("compose.override.yml");
        let project = ComposeProject {
            name: name.clone(),
            dir: tree.clone(),
            files: vec![file],
            override_file: Some(override_file.clone()),
        };

        let model = compose.validate(&project, &tree).await.unwrap();
        assert_eq!(model, ComposeModel { services: vec!["app".into()], volumes: vec!["appdata".into()] });
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let vol_dir = t.path().join("state/volumes/appdata");
        Compose::write_override(
            &override_file,
            &[ServicePorts { service: "app".into(), ports: vec![(port, 80)] }],
            &[("appdata".into(), vol_dir)],
            false,
        )
        .unwrap();

        let result = async {
            compose.pull(&project).await?;
            compose.up(&project, "app").await?;
            assert_eq!(compose.service_state(&project, "app").await?, ContainerState::Running);
            assert!(compose.mage_projects().await?.contains(&name));
            compose.stop(&project, "app", 1).await?;
            assert!(matches!(compose.service_state(&project, "app").await?, ContainerState::Exited(_)));
            Ok::<_, ExecError>(())
        }
        .await;
        let down = compose.down(&name).await;
        result.unwrap();
        down.unwrap();
        assert!(!compose.mage_projects().await.unwrap().contains(&name));
        assert_eq!(compose.service_state(&project, "app").await.unwrap(), ContainerState::Absent);
    }
}
