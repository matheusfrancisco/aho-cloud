//! The Docker backend against a fake `docker` CLI (a shell script that records its arguments and
//! answers like the daemon; `docker exec -i` runs the real `cloud-guest exec`), alone and under
//! the supervisor.
#![cfg(unix)]
// Helpers outside `#[test]` fns are test code too.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use cloud_api::exec::ExecRequest;
use cloud_api::internal::RunState;
use cloud_contract::ledger::Stage;
use cloud_runtime::docker::{Docker, DockerConfig, MANAGED_LABEL};
use cloud_runtime::egress::EgressPolicies;
use cloud_runtime::guest::control::Client;
use cloud_runtime::guest::exec::{collect, read_frame, write_frame};
use cloud_runtime::guest::{GuestConfig, ImageConfig, VolumeMount};
use cloud_runtime::net::Allocator;
use cloud_runtime::vmm::{DiskId, ReplicaSpec, Vmm, VmmError, VolumeSpec};
use cloud_runtime::{Supervisor, SupervisorConfig};
use common::{DIGEST_1, deployment};

const DIGEST: &str = "sha256:5555555555555555555555555555555555555555555555555555555555555555";

/// A fake `docker` in a temp dir. Every call appends its argv (joined by `\x1f`) to `docker.log`.
/// Answers: `run` prints `cid-<name>`; `port` prints `127.0.0.1:<log/port>` and an IPv6 line;
/// `inspect` prints `log/running` (default `true`); `exec -i` runs `cloud-guest exec` (this
/// build's), other `exec`s exit with `log/exec_code` (default 0); `rm` of a name containing
/// `missing` fails like the daemon, `volume rm` likewise; `ps` prints `log/ps`; `pull` fails
/// when `log/pull_fails` exists.
struct Host {
    _tmp: tempfile::TempDir,
    root: PathBuf,
}

impl Host {
    fn new() -> Host {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_owned();
        std::fs::create_dir_all(root.join("log")).unwrap();
        let log = root.join("log");
        let script = format!(
            r#"#!/bin/sh
log={log}
guest={guest}
(for a in "$@"; do printf '%s\037' "$a"; done; echo) >> "$log/docker.log"
case "$1" in
  pull) [ -e "$log/pull_fails" ] && {{ echo "manifest unknown" >&2; exit 1; }}; exit 0 ;;
  run)
    while [ $# -gt 0 ]; do [ "$1" = "--name" ] && name="$2"; shift; done
    echo "cid-$name" ;;
  port) echo "127.0.0.1:$(cat "$log/port" 2>/dev/null || echo 49153)"; echo "[::]:49153" ;;
  inspect) cat "$log/running" 2>/dev/null || echo true ;;
  exec) [ "$2" = -i ] && exec "$guest" exec; exit "$(cat "$log/exec_code" 2>/dev/null || echo 0)" ;;
  rm) case "$3" in *missing*) echo "Error response from daemon: No such container: $3" >&2; exit 1 ;; esac ;;
  volume) case "$3" in *missing*) echo "Error response from daemon: get $3: no such volume" >&2; exit 1 ;; esac ;;
  ps) cat "$log/ps" 2>/dev/null || true ;;
  restart) ;;
esac
"#,
            log = log.display(),
            guest = env!("CARGO_BIN_EXE_cloud-guest"),
        );
        let bin = root.join("docker");
        std::fs::write(&bin, script).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        Host { _tmp: tmp, root }
    }

    fn backend(&self) -> Docker {
        Docker::new(DockerConfig {
            docker: self.root.join("docker"),
            repo_template: "localhost:5055/aho-cloud/{project}".into(),
            state_dir: self.root.join("state"),
            publish_ip: Ipv4Addr::LOCALHOST,
            network: Some("aho-cloud".into()),
            mem_mib: 512,
            cpus: 1,
            pids_max: 64,
        })
    }

    fn set(&self, name: &str, value: &str) {
        std::fs::write(self.root.join("log").join(name), value).unwrap();
    }

    /// Recorded calls, one argv per entry.
    fn calls(&self) -> Vec<Vec<String>> {
        std::fs::read_to_string(self.root.join("log/docker.log"))
            .unwrap_or_default()
            .lines()
            .map(|l| l.split('\x1f').filter(|a| !a.is_empty()).map(str::to_owned).collect())
            .collect()
    }

    fn calls_to(&self, verb: &str) -> Vec<Vec<String>> {
        self.calls().into_iter().filter(|c| c.first().map(String::as_str) == Some(verb)).collect()
    }

    fn env_file(&self, id: &str) -> PathBuf {
        self.root.join("state/env").join(format!("{id}.env"))
    }
}

fn spec(id: &str, command: Option<Vec<String>>, volume: bool) -> ReplicaSpec {
    let lease = Allocator::new("10.77.0.0/24".parse().unwrap()).unwrap().allocate().unwrap();
    let mut env = BTreeMap::from([
        ("STRIPE_KEY".to_owned(), "sk_live_1".to_owned()),
        ("PORT".to_owned(), "8080".to_owned()),
        ("HTTP_PROXY".to_owned(), "http://10.77.0.1:3128".to_owned()),
        ("https_proxy".to_owned(), "http://10.77.0.1:3128".to_owned()),
        ("NO_PROXY".to_owned(), "localhost".to_owned()),
    ]);
    env.insert("GREETING".into(), "a=b c".into());
    let labels =
        [("aho-cloud.project", "acme-shop"), ("aho-cloud.stage", "preview"), ("aho-cloud.process", "web"), ("aho-cloud.release", "r1")];
    ReplicaSpec {
        id: id.into(),
        repo: "acme-shop".into(),
        image_digest: DIGEST.into(),
        hostname: "web-0".into(),
        labels: labels.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect(),
        guest: GuestConfig {
            process: "web".into(),
            command,
            image: ImageConfig::default(),
            env,
            port: Some(8080),
            volume: volume.then(|| VolumeMount { device: "/dev/vdc".into(), mount: "/data".into() }),
            dns: vec![lease.gateway],
        },
        volume: volume.then(|| VolumeSpec {
            disk: DiskId::Process { project: "acme-shop".into(), stage: Stage::Preview, process: "web".into(), index: 0 },
            size_mb: 256,
        }),
        cpus: None,
        mem_mib: None,
        lease,
    }
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[tokio::test]
async fn start_runs_the_pinned_image_with_secrets_only_in_a_private_env_file() {
    let host = Host::new();
    let docker = host.backend();
    docker.prepare().await.unwrap();
    let s = spec("vm-aaaa01", None, true);
    docker.start(s.clone()).await.unwrap();

    let image = format!("localhost:5055/aho-cloud/acme-shop@{DIGEST}");
    assert_eq!(host.calls_to("pull"), [vec!["pull".to_owned(), "-q".to_owned(), image.clone()]]);
    let run = &host.calls_to("run")[0];
    let env_path = host.env_file("vm-aaaa01");
    let joined = run.join(" ");
    for part in [
        "run -d --name vm-aaaa01".to_owned(),
        format!("--label {MANAGED_LABEL}=1"),
        "--label aho-cloud.project=acme-shop --label aho-cloud.stage=preview --label aho-cloud.process=web --label aho-cloud.release=r1"
            .to_owned(),
        format!("--env-file {}", env_path.display()),
        "--memory 512m --cpus 1 --pids-limit 64 --restart no --network aho-cloud".to_owned(),
        "-p 127.0.0.1::8080".to_owned(),
        "-v aho-cloud-acme-shop-preview-web-0:/data".to_owned(),
    ] {
        assert!(joined.contains(&part), "missing `{part}` in {joined}");
    }
    assert_eq!(run.last().unwrap(), &image, "no command: the image's own entrypoint runs");
    assert!(!joined.contains("sk_live_1"), "secret values never reach argv: {joined}");

    let env = std::fs::read_to_string(&env_path).unwrap();
    assert_eq!(env, "GREETING=a=b c\nPORT=8080\nSTRIPE_KEY=sk_live_1\n", "proxy variables are dropped");
    assert_eq!(mode(&env_path), 0o600);
    assert_eq!(mode(&host.root.join("state/env")), 0o700);

    // The published port is the replica's endpoint, not the unused lease address.
    assert_eq!(docker.endpoint("vm-aaaa01", s.lease.ip, 8080).to_string(), "127.0.0.1:49153");
    assert_eq!(docker.endpoint("vm-other", s.lease.ip, 8080).ip(), std::net::IpAddr::V4(s.lease.ip));
}

#[tokio::test]
async fn a_manifest_command_replaces_the_entrypoint() {
    let host = Host::new();
    let docker = host.backend();
    docker.prepare().await.unwrap();
    docker.start(spec("vm-cmd", Some(vec!["python".into(), "-m".into(), "worker".into()]), false)).await.unwrap();
    let run = &host.calls_to("run")[0];
    let tail: Vec<&str> = run.iter().rev().take(5).rev().map(String::as_str).collect();
    assert_eq!(tail, ["--entrypoint", "python", &format!("localhost:5055/aho-cloud/acme-shop@{DIGEST}"), "-m", "worker"]);
    assert!(!run.contains(&"-v".to_owned()), "no volume declared");
}

#[tokio::test]
async fn values_an_env_file_cannot_carry_are_refused_before_anything_runs() {
    let host = Host::new();
    let docker = host.backend();
    docker.prepare().await.unwrap();
    let mut s = spec("vm-nl", None, false);
    s.guest.env.insert("PEM".into(), "-----BEGIN\nKEY".into());
    let err = docker.start(s).await.unwrap_err();
    assert!(matches!(&err, VmmError::Docker(m) if m.contains("PEM")), "{err}");
    assert!(host.calls_to("run").is_empty() && host.calls_to("pull").is_empty());
    assert!(!host.env_file("vm-nl").exists());
}

#[tokio::test]
async fn a_failed_pull_leaves_no_container_and_no_secrets_on_disk() {
    let host = Host::new();
    let docker = host.backend();
    docker.prepare().await.unwrap();
    host.set("pull_fails", "");
    let err = docker.start(spec("vm-pull", None, false)).await.unwrap_err();
    assert!(err.to_string().contains("manifest unknown"), "{err}");
    assert!(host.calls_to("run").is_empty());
    assert_eq!(host.calls_to("rm"), [vec!["rm".to_owned(), "-f".to_owned(), "vm-pull".to_owned()]]);
    assert!(!host.env_file("vm-pull").exists());
}

#[tokio::test]
async fn stop_removes_container_env_file_and_endpoint_and_tolerates_unknown_ids() {
    let host = Host::new();
    let docker = host.backend();
    docker.prepare().await.unwrap();
    let s = spec("vm-stop", None, false);
    docker.start(s.clone()).await.unwrap();
    docker.stop("vm-stop").await.unwrap();
    assert!(!host.env_file("vm-stop").exists());
    assert_eq!(docker.endpoint("vm-stop", s.lease.ip, 8080).ip(), std::net::IpAddr::V4(s.lease.ip));
    docker.stop("vm-missing").await.unwrap();
    assert_eq!(host.calls_to("rm").len(), 2);
}

#[tokio::test]
async fn running_follows_the_container_state() {
    let host = Host::new();
    let docker = host.backend();
    assert!(docker.running("vm-x").await);
    host.set("running", "false\n");
    assert!(!docker.running("vm-x").await);
}

#[tokio::test]
async fn command_health_and_restart_speak_the_guest_protocol_over_docker_exec() {
    let host = Host::new();
    let docker = host.backend();
    let mut client = Client::new(docker.connect("vm-h").await.unwrap());
    host.set("exec_code", "3");
    let code = client.health(&["pg_isready".into(), "-q".into()], std::time::Duration::from_secs(30)).await.unwrap();
    assert_eq!(code, 3);
    assert_eq!(host.calls_to("exec")[0], ["exec", "vm-h", "pg_isready", "-q"]);
    client.restart().await.unwrap();
    assert_eq!(host.calls_to("restart")[0], ["restart", "-t", "10", "vm-h"]);
}

#[tokio::test]
async fn prepare_removes_containers_left_by_an_earlier_run() {
    let host = Host::new();
    host.set("ps", "cid1\ncid2\n");
    host.backend().prepare().await.unwrap();
    let ps = &host.calls_to("ps")[0];
    assert_eq!(ps.join(" "), format!("ps -aq --filter label={MANAGED_LABEL}=1"));
    assert_eq!(host.calls_to("rm"), [vec!["rm".to_owned(), "-f".to_owned(), "cid1".to_owned(), "cid2".to_owned()]]);
}

#[tokio::test]
async fn the_supervisor_reports_published_ports_as_replica_addresses() {
    // A real HTTP server stands in for the container's published port.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = axum::Router::new().route("/healthz", axum::routing::get(|| async { "ok" }));
    tokio::spawn(async move { axum::serve(listener, app).await });

    let host = Host::new();
    host.set("port", &port.to_string());
    let docker = host.backend();
    docker.prepare().await.unwrap();
    let cfg = SupervisorConfig {
        proxy_port: 3128,
        max_replicas: 4,
        health_timeout: std::time::Duration::from_millis(500),
        unhealthy_threshold: 1,
        startup_grace: std::time::Duration::ZERO,
        sandbox_failures: 3,
    };
    let sup = Supervisor::new(docker, Allocator::new("10.77.0.0/24".parse().unwrap()).unwrap(), EgressPolicies::default(), cfg);
    sup.apply(deployment("r1", DIGEST_1)).unwrap();
    sup.reconcile().await;

    let status = sup.status("acme-shop", Stage::Live).unwrap();
    assert_eq!(status.state, RunState::Running, "{status:?}");
    let web = status.processes.iter().find(|p| p.name == "web").unwrap();
    assert_eq!((web.replicas, web.healthy), (1, 1));
    assert_eq!(web.addresses, [format!("127.0.0.1:{port}")]);
    // The worker's command health check went through `docker exec`.
    let exec = host.calls_to("exec");
    assert!(exec.iter().any(|c| c.ends_with(&["python".into(), "-m".into(), "worker".into(), "--check".into()])), "{exec:?}");
}

#[tokio::test]
async fn a_sandbox_replica_runs_its_repo_with_its_own_volume_and_resources() {
    let host = Host::new();
    let docker = host.backend();
    docker.prepare().await.unwrap();
    let mut s = spec("vm-sbx01", None, false);
    s.repo = "aho.sandbox".into();
    s.hostname = "dev-box".into();
    s.labels = vec![("aho-cloud.sandbox".into(), "dev-box".into())];
    s.guest.port = Some(3000);
    s.guest.volume = Some(VolumeMount { device: "/dev/vdc".into(), mount: "/workspace".into() });
    s.volume = Some(VolumeSpec { disk: DiskId::Sandbox("dev-box".into()), size_mb: 10240 });
    (s.cpus, s.mem_mib) = (Some(2), Some(2048));
    docker.start(s).await.unwrap();

    let image = format!("localhost:5055/aho-cloud/aho.sandbox@{DIGEST}");
    assert_eq!(host.calls_to("pull"), [vec!["pull".to_owned(), "-q".to_owned(), image.clone()]]);
    let run = &host.calls_to("run")[0];
    let joined = run.join(" ");
    for part in [
        "--name vm-sbx01 --hostname dev-box".to_owned(),
        format!("--label {MANAGED_LABEL}=1 --label aho-cloud.sandbox=dev-box --env-file"),
        "--memory 2048m --cpus 2 --pids-limit 64".to_owned(),
        "-p 127.0.0.1::3000".to_owned(),
        "-v aho-cloud-sandbox.dev-box:/workspace".to_owned(),
    ] {
        assert!(joined.contains(&part), "missing `{part}` in {joined}");
    }
    assert!(!joined.contains("aho-cloud.project"), "{joined}");
    assert_eq!(run.last().unwrap(), &image, "the image's own idle command runs");
}

#[tokio::test]
async fn exec_streams_run_cloud_guest_exec_in_the_container() {
    let host = Host::new();
    let docker = host.backend();
    let sh = |s: &str| ExecRequest { argv: vec!["sh".into(), "-c".into(), s.into()], pty: None, env: BTreeMap::new(), cwd: None };
    let out = collect(docker.exec("vm-x").await.unwrap(), sh("echo out; echo err >&2; exit 3"), 1024).await.unwrap();
    assert_eq!((out.code, out.stdout.as_slice(), out.stderr.as_slice()), (3, &b"out\n"[..], &b"err\n"[..]));
    assert_eq!(host.calls_to("exec")[0], ["exec", "-i", "vm-x", "/usr/local/bin/cloud-guest", "exec"]);

    // Dropping the stream ends the session inside the container.
    let pid_file = host.root.join("log/session.pid");
    let mut conn = docker.exec("vm-x").await.unwrap();
    let script = format!("echo $$ > '{}'; echo ready; exec sleep 30", pid_file.display());
    write_frame(&mut conn, &cloud_api::exec::Frame::Request(sh(&script))).await.unwrap();
    assert_eq!(read_frame(&mut conn).await.unwrap(), Some(cloud_api::exec::Frame::Stdout(b"ready\n".to_vec())));
    let pid: i32 = std::fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
    drop(conn);
    let gone = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok() {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(gone.is_ok(), "the session outlived its stream");
}

#[tokio::test]
async fn destroying_a_volume_removes_it_and_tolerates_missing_ones() {
    let host = Host::new();
    let docker = host.backend();
    docker.destroy_volume(&DiskId::Sandbox("dev-box".into())).await.unwrap();
    docker.destroy_volume(&DiskId::Sandbox("missing-box".into())).await.unwrap();
    let disk = DiskId::Process { project: "acme-shop".into(), stage: Stage::Live, process: "worker".into(), index: 0 };
    docker.destroy_volume(&disk).await.unwrap();
    let rm: Vec<String> = host.calls_to("volume").into_iter().map(|c| c.join(" ")).collect();
    assert_eq!(
        rm,
        ["volume rm aho-cloud-sandbox.dev-box", "volume rm aho-cloud-sandbox.missing-box", "volume rm aho-cloud-acme-shop-live-worker-0"]
    );
}
