//! Recipe services (executor.md "Services"): native processes and docker compose services,
//! started in `depends_on` order, health-probed from the daemon (loopback, env ports only), and
//! monitored after they turn healthy.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Weak};
use std::time::Duration;

use mage_core::domain::recipe::{HealthSpec, RecipeSpec, ServiceSpec};
use nix::sys::signal::Signal;
use parking_lot::Mutex;
use tokio::io::AsyncReadExt;
use tokio::task::JoinHandle;

use super::compose::{ComposeProject, ServicePorts};
use super::launch::kill_group;
use super::logs::LogWriter;
use super::sandbox::Sandbox;
use super::{ports, sys};
use crate::ports::executor::{
    EnvPath, ExecError, ExecSpec, Exit, HealthCheckResult, HealthReport, LogSource, PortName, ProcessRole, RelPath, ServiceSelector,
    ServiceState, ServiceStatus, ServicesReport,
};

/// Log markers of a service that lost the bind race on a leased port.
const ADDR_IN_USE: [&str; 3] = ["EADDRINUSE", "address already in use", "Address already in use"];

enum Run {
    Native { pgid: i32, leaf: Option<std::path::PathBuf>, waiter: JoinHandle<()> },
    Compose { project: ComposeProject, service: String, logs: JoinHandle<()> },
}

struct Svc {
    status: ServiceStatus,
    run: Option<Run>,
    monitor: Option<JoinHandle<()>>,
    stopping: bool,
}

type Table = Arc<Mutex<BTreeMap<String, Svc>>>;

#[derive(Default)]
pub(crate) struct Services {
    table: Table,
}

fn now(sb: &Sandbox) -> time::OffsetDateTime {
    sb.host.clock.now()
}

/// Services to act on, with transitive dependencies, in start order.
fn start_order(recipe: &RecipeSpec, which: &ServiceSelector) -> Result<Vec<String>, ExecError> {
    let mut wanted: BTreeSet<String> = BTreeSet::new();
    let mut stack: Vec<String> = match which {
        ServiceSelector::All => recipe.services.keys().cloned().collect(),
        ServiceSelector::Named(names) => names.clone(),
    };
    while let Some(name) = stack.pop() {
        let spec = recipe.services.get(&name).ok_or_else(|| ExecError::NotFound(format!("service {name}")))?;
        if wanted.insert(name) {
            stack.extend(spec.depends_on.iter().cloned());
        }
    }
    let mut order = Vec::with_capacity(wanted.len());
    let mut placed: BTreeSet<String> = BTreeSet::new();
    while placed.len() < wanted.len() {
        let next: Vec<String> = wanted
            .iter()
            .filter(|n| !placed.contains(*n))
            .filter(|n| recipe.services[*n].depends_on.iter().all(|d| placed.contains(d)))
            .cloned()
            .collect();
        if next.is_empty() {
            return Err(ExecError::Invalid("services.depends_on has a cycle".into()));
        }
        for n in next {
            placed.insert(n.clone());
            order.push(n);
        }
    }
    Ok(order)
}

fn selected(recipe: Option<&RecipeSpec>, which: &ServiceSelector) -> Result<Vec<String>, ExecError> {
    match (recipe, which) {
        (Some(r), ServiceSelector::All) => Ok(r.services.keys().cloned().collect()),
        (Some(r), ServiceSelector::Named(names)) => {
            for n in names {
                if !r.services.contains_key(n) {
                    return Err(ExecError::NotFound(format!("service {n}")));
                }
            }
            Ok(names.clone())
        }
        (None, ServiceSelector::Named(names)) if !names.is_empty() => Err(ExecError::NotFound(format!("service {}", names[0]))),
        (None, _) => Ok(Vec::new()),
    }
}

fn service_ports(sb: &Sandbox, spec: &ServiceSpec) -> Vec<(PortName, SocketAddr)> {
    let vars = sb.vars.lock();
    spec.ports.iter().filter_map(|p| Some((PortName::new(p).ok()?, vars.ports.get(p)?))).collect()
}

/// Compose project of the sandbox: validated model, override with leased ports and volume binds.
pub(crate) async fn compose_project(sb: &Sandbox, recipe: &RecipeSpec, pull_never: bool) -> Result<Option<ComposeProject>, ExecError> {
    let compose_services: Vec<(&String, &ServiceSpec)> = recipe.services.iter().filter(|(_, s)| s.compose.is_some()).collect();
    if compose_services.is_empty() {
        return Ok(None);
    }
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    for (_, spec) in &compose_services {
        if let Some(c) = &spec.compose {
            let path = sb.ws.file_path(&RelPath::new(&c.file)?)?;
            if !files.contains(&path) {
                files.push(path);
            }
        }
    }
    let tree = sb.dir.join("tree");
    let mut project = ComposeProject { name: sb.compose_name.clone(), dir: tree.clone(), files, override_file: None };
    let model = sb.host.compose.validate(&project, &tree).await?;
    let vars = sb.vars.lock().clone();
    let mut overrides = Vec::new();
    for (_, spec) in &compose_services {
        if let Some(c) = &spec.compose {
            let mut mapped = Vec::new();
            for (port, container) in &c.ports {
                let host = vars.ports.get(port).ok_or_else(|| ExecError::Invalid(format!("compose port {port} is not leased")))?;
                mapped.push((host.port(), *container));
            }
            overrides.push(ServicePorts { service: c.service.clone(), ports: mapped });
        }
    }
    let volumes: Vec<(String, std::path::PathBuf)> =
        model.volumes.iter().map(|v| (v.clone(), sb.dir.join("state").join("volumes").join(v))).collect();
    let path = sb.run_dir().join("compose.override.yml");
    super::compose::Compose::write_override(&path, &overrides, &volumes, pull_never)?;
    project.override_file = Some(path);
    Ok(Some(project))
}

fn loopback_allowed(sb: &Sandbox, host: &str, port: u16) -> Result<(), String> {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let loopback = host == "localhost" || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback());
    if !loopback {
        return Err(format!("probe target {host} is not loopback"));
    }
    if !sb.vars.lock().ports.contains_port(port) {
        return Err(format!("probe port {port} is not one of this env's ports"));
    }
    Ok(())
}

/// One health probe (executor.md: http/tcp from the daemon, loopback env ports only; command as
/// an exec with a 10 s timeout).
async fn probe(sb: &Sandbox, name: &str, health: &HealthSpec) -> HealthCheckResult {
    let timeout = Duration::from_millis(u64::from(health.timeout_ms.max(1)));
    let (ok, detail) = match probe_inner(sb, name, health, timeout).await {
        Ok(detail) => (true, detail),
        Err(detail) => (false, detail),
    };
    HealthCheckResult { service: name.to_owned(), ok, detail, at: now(sb) }
}

async fn probe_inner(sb: &Sandbox, name: &str, health: &HealthSpec, timeout: Duration) -> Result<String, String> {
    if let Some(url) = &health.http {
        let rendered = sb.render(url).map_err(|e| e.to_string())?;
        let parsed = url::Url::parse(&rendered).map_err(|e| format!("health url: {e}"))?;
        let host = parsed.host_str().ok_or("health url has no host")?;
        let port = parsed.port_or_known_default().ok_or("health url has no port")?;
        loopback_allowed(sb, host, port)?;
        let resp = sb.host.probe_http.get(parsed).timeout(timeout).send().await.map_err(|e| e.without_url().to_string())?;
        let status = resp.status().as_u16();
        let expected = if health.expect_status.is_empty() { vec![200] } else { health.expect_status.clone() };
        return if expected.contains(&status) { Ok(format!("http {status}")) } else { Err(format!("http {status}")) };
    }
    if let Some(target) = &health.tcp {
        let rendered = sb.render(target).map_err(|e| e.to_string())?;
        let (host, port) = rendered.rsplit_once(':').ok_or("tcp target needs host:port")?;
        let port: u16 = port.parse().map_err(|_| "tcp port is not a number")?;
        loopback_allowed(sb, host, port)?;
        let addr = if host == "localhost" { format!("127.0.0.1:{port}") } else { format!("{}:{port}", host.trim_start_matches('[').trim_end_matches(']')) };
        let addr: SocketAddr = addr.parse().map_err(|_| "tcp target is not an address")?;
        return match tokio::time::timeout(timeout, tokio::net::TcpStream::connect(addr)).await {
            Ok(Ok(_)) => Ok(format!("tcp {addr} connected")),
            Ok(Err(e)) => Err(format!("tcp {addr}: {e}")),
            Err(_) => Err(format!("tcp {addr}: timed out")),
        };
    }
    if let Some(command) = &health.command {
        let rendered = sb.render(command).map_err(|e| e.to_string())?;
        let mut spec = ExecSpec::shell(rendered, ProcessRole::Exec);
        spec.timeout = Duration::from_secs(10);
        let log = LogWriter::open(&sb.logs_dir().join("svc").join(format!("{name}.health.log"))).map_err(|e| e.to_string())?;
        let out = sb.run(spec, None, None, Some((LogSource::Service(name.to_owned()), log))).await.map_err(|e| e.to_string())?;
        return if out.exit.success() { Ok("command exit 0".into()) } else { Err(format!("command {}", out.exit)) };
    }
    Err("no health probe configured".into())
}

fn pump<R: tokio::io::AsyncRead + Unpin + Send + 'static>(stream: Option<R>, log: LogWriter) {
    if let Some(mut stream) = stream {
        tokio::spawn(async move {
            let mut buf = vec![0u8; 16 * 1024];
            loop {
                match stream.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => log.write(&buf[..n]),
                }
            }
        });
    }
}

fn log_mentions_addr_in_use(sb: &Sandbox, name: &str) -> bool {
    let path = sb.log_path(&LogSource::Service(name.to_owned()));
    let Ok(bytes) = std::fs::read(&path) else { return false };
    let tail = &bytes[bytes.len().saturating_sub(64 * 1024)..];
    let text = String::from_utf8_lossy(tail);
    ADDR_IN_USE.iter().any(|m| text.contains(m))
}

impl Services {
    fn set_state(&self, name: &str, state: ServiceState, at: time::OffsetDateTime) {
        if let Some(svc) = self.table.lock().get_mut(name) {
            if svc.status.state != state {
                svc.status.state = state;
                svc.status.since = at;
            }
        }
    }

    fn state_of(&self, name: &str) -> Option<ServiceState> {
        self.table.lock().get(name).map(|s| s.status.state.clone())
    }

    fn snapshot(&self, sb: &Sandbox, names: &[String]) -> Vec<ServiceStatus> {
        let recipe = sb.vars.lock().recipe.clone();
        let table = self.table.lock();
        names
            .iter()
            .map(|n| {
                let ports = recipe.as_ref().and_then(|r| r.services.get(n)).map(|s| service_ports(sb, s)).unwrap_or_default();
                match table.get(n) {
                    Some(svc) => ServiceStatus { ports, ..svc.status.clone() },
                    None => ServiceStatus { name: n.clone(), state: ServiceState::Stopped, ports, since: now(sb), last_check: None },
                }
            })
            .collect()
    }

    pub(crate) async fn start(&self, sb: &Arc<Sandbox>, which: ServiceSelector) -> Result<ServicesReport, ExecError> {
        sb.check_live()?;
        let recipe = sb.vars.lock().recipe.clone();
        let Some(recipe) = recipe else {
            selected(None, &which)?;
            return Ok(ServicesReport { services: Vec::new(), all_healthy: true });
        };
        let order = start_order(&recipe, &which)?;
        let requested = selected(Some(&recipe), &which)?;
        let mut compose: Option<ComposeProject> = None;
        for name in &order {
            if self.state_of(name) == Some(ServiceState::Healthy) {
                continue;
            }
            let spec = &recipe.services[name];
            let blocked: Vec<&String> = spec.depends_on.iter().filter(|d| self.state_of(d) != Some(ServiceState::Healthy)).collect();
            if !blocked.is_empty() {
                let at = now(sb);
                let reason = format!("dependency {} is not healthy", blocked[0]);
                self.table.lock().insert(
                    name.clone(),
                    Svc { status: ServiceStatus { name: name.clone(), state: ServiceState::Unhealthy { reason }, ports: vec![], since: at, last_check: None }, run: None, monitor: None, stopping: false },
                );
                continue;
            }
            if spec.compose.is_some() && compose.is_none() {
                compose = compose_project(sb, &recipe, true).await?;
            }
            let mut state = self.start_one(sb, name, spec, compose.as_ref()).await?;
            if !matches!(state, ServiceState::Healthy) && log_mentions_addr_in_use(sb, name) && !spec.ports.is_empty() {
                // Lost the bind race on a leased port: quarantine, re-lease, retry once.
                tracing::warn!(service = %name, "service hit EADDRINUSE; retrying with fresh ports");
                self.stop_one(sb, name, spec).await;
                self.relese_service_ports(sb, spec).await?;
                if spec.compose.is_some() {
                    compose = compose_project(sb, &recipe, true).await?;
                }
                state = self.start_one(sb, name, spec, compose.as_ref()).await?;
            }
            if state == ServiceState::Healthy {
                self.spawn_monitor(sb, name, spec);
            }
        }
        let services = self.snapshot(sb, &requested);
        let all_healthy = services.iter().all(|s| s.state == ServiceState::Healthy);
        if all_healthy {
            listener_warnings(sb);
        }
        Ok(ServicesReport { services, all_healthy })
    }

    async fn relese_service_ports(&self, sb: &Sandbox, spec: &ServiceSpec) -> Result<(), ExecError> {
        let old: Vec<u16> = {
            let vars = sb.vars.lock();
            spec.ports.iter().filter_map(|p| vars.ports.get(p)).map(|a| a.port()).collect()
        };
        ports::quarantine(&sb.host, &old).await?;
        let names: Vec<Option<PortName>> = spec.ports.iter().map(|p| PortName::new(p).map(Some)).collect::<Result<_, _>>()?;
        let leases = ports::lease(&sb.host, &sb.port_owner, &names).await?;
        let mut vars = sb.vars.lock();
        for lease in leases {
            if let Some(name) = lease.name {
                vars.ports.0.insert(name, SocketAddr::from(([127, 0, 0, 1], lease.port)));
            }
        }
        Ok(())
    }

    /// Starts one service and waits for its first healthy probe (or failure).
    async fn start_one(&self, sb: &Arc<Sandbox>, name: &str, spec: &ServiceSpec, compose: Option<&ComposeProject>) -> Result<ServiceState, ExecError> {
        let at = now(sb);
        let log = LogWriter::open(&sb.log_path(&LogSource::Service(name.to_owned())))?;
        let run = if let Some(c) = &spec.compose {
            let project = compose.ok_or_else(|| ExecError::Invalid("compose project missing".into()))?.clone();
            sb.host.compose.up(&project, &c.service).await?;
            let mut child = sb.host.compose.spawn_logs(&project, &c.service)?;
            pump(child.stdout.take(), log.clone());
            pump(child.stderr.take(), log.clone());
            let logs = tokio::spawn(async move {
                let _ = child.wait().await;
            });
            Run::Compose { project, service: c.service.clone(), logs }
        } else {
            let command = spec.run.as_deref().ok_or_else(|| ExecError::Invalid(format!("service {name} has neither run nor compose")))?;
            let rendered = sb.render(command)?;
            let cwd = EnvPath::ws(&spec.cwd)?;
            let mut child = sb
                .spawn_child(vec!["/bin/sh".into(), "-c".into(), rendered], &cwd, Some(&spec.env), &[], ProcessRole::Service(name.to_owned()))
                .await?;
            let (out, err) = child.take_pipes();
            pump(out, log.clone());
            pump(err, log.clone());
            let pgid = child.pgid();
            let leaf = child.leaf();
            let table = Arc::clone(&self.table);
            let svc_name = name.to_owned();
            let clock = Arc::clone(&sb.host.clock);
            let waiter = tokio::spawn(async move {
                use crate::ports::executor::EnvChild;
                let exit = child.wait().await.unwrap_or(Exit::Code(-1));
                let mut table = table.lock();
                if let Some(svc) = table.get_mut(&svc_name) {
                    svc.status.state = if svc.stopping { ServiceState::Stopped } else { ServiceState::Exited { exit } };
                    svc.status.since = clock.now();
                }
            });
            Run::Native { pgid, leaf, waiter }
        };
        let ports = service_ports(sb, spec);
        if let Some(old) = self.table.lock().insert(
            name.to_owned(),
            Svc { status: ServiceStatus { name: name.to_owned(), state: ServiceState::Starting, ports, since: at, last_check: None }, run: Some(run), monitor: None, stopping: false },
        ) {
            if let Some(m) = old.monitor {
                m.abort();
            }
        }
        let health = spec.health.clone().ok_or_else(|| ExecError::Invalid(format!("service {name} has no health probe")))?;
        let started = tokio::time::Instant::now();
        let period = Duration::from_secs(u64::from(health.start_period_s));
        let interval = Duration::from_millis(u64::from(health.interval_ms.max(50)));
        let mut last = String::from("no probe yet");
        loop {
            if let Some(ServiceState::Exited { exit }) = self.state_of(name) {
                return Ok(ServiceState::Exited { exit });
            }
            let compose_run = self.table.lock().get(name).and_then(|s| match &s.run {
                Some(Run::Compose { project, service, .. }) => Some((project.clone(), service.clone())),
                _ => None,
            });
            if let Some((project, service)) = compose_run {
                if let Ok(super::compose::ContainerState::Exited(code)) = sb.host.compose.service_state(&project, &service).await {
                    let state = ServiceState::Exited { exit: Exit::Code(code) };
                    self.set_state(name, state.clone(), now(sb));
                    return Ok(state);
                }
            }
            let check = probe(sb, name, &health).await;
            let ok = check.ok;
            last.clone_from(&check.detail);
            if let Some(svc) = self.table.lock().get_mut(name) {
                svc.status.last_check = Some(check);
            }
            if ok {
                self.set_state(name, ServiceState::Healthy, now(sb));
                return Ok(ServiceState::Healthy);
            }
            if started.elapsed() >= period {
                let state = ServiceState::Unhealthy { reason: format!("no healthy probe within {}s; last: {last}", period.as_secs()) };
                self.set_state(name, state.clone(), now(sb));
                return Ok(state);
            }
            tokio::time::sleep(interval).await;
        }
    }

    fn spawn_monitor(&self, sb: &Arc<Sandbox>, name: &str, spec: &ServiceSpec) {
        let Some(health) = spec.health.clone() else { return };
        let weak: Weak<Sandbox> = Arc::downgrade(sb);
        let table = Arc::clone(&self.table);
        let name = name.to_owned();
        let interval = Duration::from_millis(u64::from(health.interval_ms.max(50)));
        let retries = health.retries.max(1);
        let task_name = name.clone();
        let handle = tokio::spawn(async move {
            let mut failures = 0u32;
            loop {
                tokio::time::sleep(interval).await;
                let Some(sb) = weak.upgrade() else { return };
                if sb.check_live().is_err() {
                    return;
                }
                let state = table.lock().get(&task_name).map(|s| (s.status.state.clone(), s.stopping));
                match state {
                    Some((ServiceState::Healthy | ServiceState::Unhealthy { .. }, false)) => {}
                    _ => return,
                }
                let check = probe(&sb, &task_name, &health).await;
                let at = check.at;
                let mut table = table.lock();
                let Some(svc) = table.get_mut(&task_name) else { return };
                if check.ok {
                    failures = 0;
                    if svc.status.state != ServiceState::Healthy {
                        svc.status.state = ServiceState::Healthy;
                        svc.status.since = at;
                    }
                } else {
                    failures += 1;
                    if failures >= retries && matches!(svc.status.state, ServiceState::Healthy) {
                        svc.status.state = ServiceState::Unhealthy { reason: format!("{failures} consecutive failed probes; last: {}", check.detail) };
                        svc.status.since = at;
                    }
                }
                svc.status.last_check = Some(check);
            }
        });
        if let Some(svc) = self.table.lock().get_mut(&name) {
            svc.monitor = Some(handle);
        }
    }

    async fn stop_one(&self, sb: &Sandbox, name: &str, spec: &ServiceSpec) {
        let run = {
            let mut table = self.table.lock();
            let Some(svc) = table.get_mut(name) else { return };
            svc.stopping = true;
            if let Some(m) = svc.monitor.take() {
                m.abort();
            }
            svc.run.take()
        };
        let grace = Duration::from_secs(u64::from(spec.stop_timeout_s));
        match run {
            Some(Run::Native { pgid, leaf, waiter }) => {
                sys::signal_group(pgid, Signal::SIGTERM);
                let abort = waiter.abort_handle();
                if tokio::time::timeout(grace, waiter).await.is_err() {
                    kill_group(pgid, leaf.as_deref());
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    abort.abort();
                    sb.reap_group(pgid, leaf.as_deref()).await;
                }
            }
            Some(Run::Compose { project, service, logs }) => {
                if let Err(err) = sb.host.compose.stop(&project, &service, spec.stop_timeout_s).await {
                    tracing::warn!(service = %name, error = %err, "compose stop failed");
                }
                logs.abort();
            }
            None => {}
        }
        self.set_state(name, ServiceState::Stopped, now(sb));
    }

    pub(crate) async fn stop(&self, sb: &Arc<Sandbox>, which: ServiceSelector) -> Result<(), ExecError> {
        sb.check_live()?;
        let recipe = sb.vars.lock().recipe.clone();
        let names = selected(recipe.as_ref(), &which)?;
        let Some(recipe) = recipe else { return Ok(()) };
        // Stop dependents before their dependencies: reverse start order over everything known.
        let order = start_order(&recipe, &ServiceSelector::All)?;
        for name in order.iter().rev().filter(|n| names.contains(n)) {
            self.stop_one(sb, name, &recipe.services[name]).await;
        }
        Ok(())
    }

    /// Stops everything without recipe lookups (destroy, set_recipe changes).
    pub(crate) async fn stop_all(&self, sb: &Sandbox, recipe: Option<&RecipeSpec>) {
        let names: Vec<String> = self.table.lock().keys().cloned().collect();
        for name in names {
            match recipe.and_then(|r| r.services.get(&name)) {
                Some(spec) => self.stop_one(sb, &name, spec).await,
                None => {
                    let run = self.table.lock().get_mut(&name).and_then(|s| {
                        s.stopping = true;
                        s.monitor.take().map(|m| m.abort());
                        s.run.take()
                    });
                    if let Some(Run::Native { pgid, leaf, waiter }) = run {
                        kill_group(pgid, leaf.as_deref());
                        waiter.abort();
                        sb.reap_group(pgid, leaf.as_deref()).await;
                    }
                }
            }
        }
    }

    /// Stops services whose definition changed or disappeared (Learn `set_recipe`).
    pub(crate) async fn stop_changed(&self, sb: &Sandbox, old: &RecipeSpec, new: &RecipeSpec) {
        for (name, spec) in &old.services {
            if new.services.get(name) != Some(spec) {
                self.stop_one(sb, name, spec).await;
                self.table.lock().remove(name);
            }
        }
    }

    pub(crate) async fn health(&self, sb: &Arc<Sandbox>) -> Result<HealthReport, ExecError> {
        sb.check_live()?;
        let recipe = sb.vars.lock().recipe.clone();
        let Some(recipe) = recipe else { return Ok(HealthReport { checks: Vec::new(), all_ok: true }) };
        let running: Vec<String> = self
            .table
            .lock()
            .iter()
            .filter(|(_, s)| !matches!(s.status.state, ServiceState::Stopped))
            .map(|(n, _)| n.clone())
            .collect();
        let mut checks = Vec::with_capacity(running.len());
        for name in running {
            let Some(health) = recipe.services.get(&name).and_then(|s| s.health.clone()) else { continue };
            let check = match self.state_of(&name) {
                Some(ServiceState::Exited { exit }) => HealthCheckResult { service: name.clone(), ok: false, detail: format!("exited ({exit})"), at: now(sb) },
                _ => probe(sb, &name, &health).await,
            };
            if let Some(svc) = self.table.lock().get_mut(&name) {
                svc.status.last_check = Some(check.clone());
            }
            checks.push(check);
        }
        let all_ok = checks.iter().all(|c| c.ok);
        Ok(HealthReport { checks, all_ok })
    }
}

/// Linux: warns about listeners of env processes on ports outside the env's `PortMap` (a fixed
/// port collides across forks).
fn listener_warnings(sb: &Sandbox) {
    #[cfg(target_os = "linux")]
    {
        let Some(cg) = &sb.cgroup else { return };
        let allowed = sb.vars.lock().ports.clone();
        for port in linux_listeners(cg) {
            if !allowed.contains_port(port) && Some(port) != sb.vars.lock().proxy_port {
                tracing::warn!(env = %sb.label, port, "env process listens on a port outside its PortMap");
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = sb;
}

#[cfg(target_os = "linux")]
fn linux_listeners(cgroup: &std::path::Path) -> BTreeSet<u16> {
    let mut pids = Vec::new();
    let mut stack = vec![cgroup.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if let Ok(procs) = std::fs::read_to_string(dir.join("cgroup.procs")) {
            pids.extend(procs.lines().filter_map(|l| l.trim().parse::<u32>().ok()));
        }
        if let Ok(rd) = std::fs::read_dir(&dir) {
            stack.extend(rd.flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).map(|e| e.path()));
        }
    }
    let mut inodes = BTreeSet::new();
    for pid in &pids {
        if let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) {
            for fd in fds.flatten() {
                if let Ok(target) = std::fs::read_link(fd.path()) {
                    let t = target.to_string_lossy();
                    if let Some(inode) = t.strip_prefix("socket:[").and_then(|s| s.strip_suffix(']')) {
                        inodes.insert(inode.to_owned());
                    }
                }
            }
        }
    }
    let mut ports = BTreeSet::new();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = std::fs::read_to_string(table) else { continue };
        for line in text.lines().skip(1) {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() > 9 && cols[3] == "0A" && inodes.contains(cols[9]) {
                if let Some(port) = cols[1].rsplit(':').next().and_then(|h| u16::from_str_radix(h, 16).ok()) {
                    ports.insert(port);
                }
            }
        }
    }
    ports
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_order_follows_depends_on_and_includes_dependencies() {
        let recipe = RecipeSpec::parse(
            r#"
version = 1
ports = ["a", "b", "c"]
[services.web]
run = "x"
ports = ["c"]
depends_on = ["api"]
[services.web.health]
tcp = "127.0.0.1:${port.c}"
[services.api]
run = "x"
ports = ["b"]
depends_on = ["db"]
[services.api.health]
tcp = "127.0.0.1:${port.b}"
[services.db]
run = "x"
ports = ["a"]
[services.db.health]
tcp = "127.0.0.1:${port.a}"
"#,
        )
        .unwrap();
        assert_eq!(start_order(&recipe, &ServiceSelector::All).unwrap(), vec!["db", "api", "web"]);
        assert_eq!(start_order(&recipe, &ServiceSelector::Named(vec!["api".into()])).unwrap(), vec!["db", "api"]);
        assert!(matches!(start_order(&recipe, &ServiceSelector::Named(vec!["nope".into()])), Err(ExecError::NotFound(_))));
    }
}
