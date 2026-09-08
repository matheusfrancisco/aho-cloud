//! The Firecracker backend end to end with fake host programs (shell scripts that record their
//! arguments): image preparation, disks, config drive, tap, cgroup, nftables, VMM process,
//! the vsock control connection, and teardown.
// Helpers outside `#[test]` fns are test code too.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use cloud_contract::ledger::Stage;
use cloud_contract::manifest::Health;
use cloud_runtime::firecracker::{Firecracker, FirecrackerConfig};
use cloud_runtime::guest::control::{self, Client, Control, ProcessState};
use cloud_runtime::guest::{GuestConfig, ImageConfig, VOLUME_DEVICE, VolumeMount};
use cloud_runtime::image::{ImagePrograms, ImageStoreConfig};
use cloud_runtime::net::Allocator;
use cloud_runtime::vmm::{ReplicaSpec, Vmm, VmmError, VolumeSpec};
use futures::future::BoxFuture;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

const DIGEST: &str = "sha256:5555555555555555555555555555555555555555555555555555555555555555";

struct Host {
    _tmp: tempfile::TempDir,
    root: PathBuf,
}

impl Host {
    fn new() -> Host {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_owned();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::create_dir_all(root.join("log")).unwrap();
        let log = root.join("log");
        let log = log.display();
        let (config_hex, manifest_hex) = ("c".repeat(64), "a".repeat(64));
        let config = r#"{"config":{"Entrypoint":["/bin/app"],"Cmd":["serve"],"Env":["PATH=/usr/bin:/bin","LANG=C"],"WorkingDir":"/srv","User":"1000:1000"}}"#;
        let scripts = [
            (
                "skopeo",
                format!(
                    r#"echo "$@" >> {log}/skopeo.log
layout="${{3#oci:}}"; layout="${{layout%:img}}"
mkdir -p "$layout/blobs/sha256"
printf '%s' '{config}' > "$layout/blobs/sha256/{config_hex}"
printf '%s' '{{"schemaVersion":2,"config":{{"digest":"sha256:{config_hex}"}}}}' > "$layout/blobs/sha256/{manifest_hex}"
printf '%s' '{{"schemaVersion":2,"manifests":[{{"digest":"sha256:{manifest_hex}","annotations":{{"org.opencontainers.image.ref.name":"img"}}}}]}}' > "$layout/index.json""#
                ),
            ),
            ("umoci", format!("echo \"$@\" >> {log}/umoci.log\nmkdir -p \"$4/rootfs/bin\"\necho '{{}}' > \"$4/config.json\"")),
            (
                "mkfs.ext4",
                format!(
                    r#"echo "$@" >> {log}/mkfs.log
if [ -e {log}/fail-mkfs ]; then echo "mkfs.ext4: No space left on device" >&2; exit 1; fi
n=$#; i=0
for a in "$@"; do i=$((i+1)); if [ $i -eq $((n-1)) ]; then img="$a"; fi; done
echo "ext4 $*" > "$img""#
                ),
            ),
            ("ip", format!("echo \"$@\" >> {log}/ip.log\nif [ \"$1\" = link ] && [ \"$2\" = show ]; then exit 1; fi")),
            ("nft", format!("echo \"$@\" >> {log}/nft.log\ncp \"$2\" {log}/nft.applied")),
            ("firecracker", format!("echo \"$@\" >> {log}/fc.log\nexec sleep 30")),
        ];
        for (name, body) in scripts {
            let path = root.join("bin").join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(root.join("cloud-guest"), b"#!cloud-guest build 7").unwrap();
        std::fs::write(root.join("vmlinux"), b"kernel").unwrap();
        Host { _tmp: tmp, root }
    }

    fn bin(&self, name: &str) -> PathBuf {
        self.root.join("bin").join(name)
    }

    fn log(&self, name: &str) -> String {
        std::fs::read_to_string(self.root.join("log").join(name)).unwrap_or_default()
    }

    /// The fake VMM writes its argv after it was spawned; wait for it rather than race it.
    async fn wait_log(&self, name: &str) -> String {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let text = self.log(name);
            if !text.is_empty() || std::time::Instant::now() > deadline {
                return text;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    fn backend(&self) -> Firecracker {
        Firecracker::new(FirecrackerConfig {
            state_dir: self.root.join("state"),
            firecracker: self.bin("firecracker"),
            ip: self.bin("ip"),
            nft: self.bin("nft"),
            kernel: self.root.join("vmlinux"),
            images: ImageStoreConfig {
                dir: self.root.join("images"),
                programs: ImagePrograms { skopeo: self.bin("skopeo"), umoci: self.bin("umoci"), mkfs_ext4: self.bin("mkfs.ext4") },
                repo_template: "registry.test:5000/aho-cloud/{project}".into(),
                guest_binary: self.root.join("cloud-guest"),
            },
            cgroup_root: self.root.join("cgroup/aho-cloud"),
            vcpus: 2,
            mem_mib: 512,
            rootfs_mib: 1024,
            memory_overhead_mib: 64,
            pids_max: 48,
            gateway: Ipv4Addr::new(10, 77, 0, 1),
            proxy_port: 3128,
            dns_port: 53,
            inbound_sources: vec!["172.20.0.0/16".parse().unwrap()],
        })
    }
}

fn spec(alloc: &mut Allocator, id: &str, index: u16, volume: bool) -> ReplicaSpec {
    let lease = alloc.allocate().unwrap();
    ReplicaSpec {
        id: id.into(),
        project: "acme-shop".into(),
        stage: Stage::Live,
        process: "worker".into(),
        index,
        release: "r1".into(),
        image_digest: DIGEST.into(),
        guest: GuestConfig {
            process: "worker".into(),
            command: None,
            image: ImageConfig::default(),
            env: BTreeMap::from([("STRIPE_KEY".into(), "sk_live_1".into())]),
            port: None,
            health: Health::Command { argv: vec!["true".into()] },
            volume: volume.then(|| VolumeMount { device: VOLUME_DEVICE.into(), mount: "/data".into() }),
            dns: vec![lease.gateway],
        },
        volume: volume.then_some(VolumeSpec { size_mb: 256 }),
        lease,
    }
}

fn gone(pid: u32) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None) == Err(nix::errno::Errno::ESRCH)
}

#[tokio::test]
async fn boots_a_replica_from_an_image_digest_and_tears_it_down() {
    let host = Host::new();
    let fc = host.backend();
    fc.prepare_host().await.unwrap();
    let ip_log = host.log("ip.log");
    assert!(ip_log.contains("link add aho0 type dummy"), "{ip_log}");
    assert!(ip_log.contains("addr replace 10.77.0.1/32 dev aho0"));
    assert_eq!(std::fs::read_to_string(host.root.join("cgroup/aho-cloud/cgroup.subtree_control")).unwrap(), "+cpu +memory +pids");
    let rules = host.log("nft.applied");
    assert!(rules.contains("iifname \"ahotap*\" drop") && !rules.contains("saddr !="), "{rules}");

    let mut alloc = Allocator::new("10.77.0.0/24".parse().unwrap()).unwrap();
    let s = spec(&mut alloc, "vm-aaaa01", 0, true);
    let paths = fc.paths(&s);
    let started = fc.start(s.clone()).await.unwrap();
    let pid = started.pid.unwrap();
    assert!(fc.running("vm-aaaa01").await);

    // Image: pulled by digest from the project's repository, unpacked, init installed.
    assert_eq!(
        host.log("skopeo.log").trim(),
        format!(
            "copy docker://registry.test:5000/aho-cloud/acme-shop@{DIGEST} oci:{}/images/{}/layout:img",
            host.root.display(),
            &DIGEST[7..]
        )
    );
    let rootfs_dir = host.root.join("images").join(&DIGEST[7..]).join("bundle/rootfs");
    assert_eq!(std::fs::read(rootfs_dir.join(".cloud/guest")).unwrap(), b"#!cloud-guest build 7");
    let mkfs = host.log("mkfs.log");
    assert!(mkfs.contains(&format!("-q -F -d {} {} 1024M", rootfs_dir.display(), paths.rootfs.display())), "{mkfs}");
    let volume = paths.volume.clone().unwrap();
    assert_eq!(volume, host.root.join("state/volumes/acme-shop/live/worker-0.ext4"));
    assert!(mkfs.contains(&format!("-q -F {} 256M", volume.display())));

    // Config drive: the image's run config plus the platform env.
    let drive = GuestConfig::from_drive(&std::fs::read(&paths.config_drive).unwrap()).unwrap();
    assert_eq!(drive.image.entrypoint, ["/bin/app"]);
    assert_eq!(drive.image.user.as_deref(), Some("1000:1000"));
    assert_eq!(drive.argv().unwrap(), ["/bin/app", "serve"]);
    assert_eq!(drive.process_env()["STRIPE_KEY"], "sk_live_1");
    assert_eq!(drive.volume.as_ref().unwrap().mount, "/data");

    // The VMM got our config file; the config names every disk and the tap.
    assert_eq!(host.wait_log("fc.log").await.trim(), format!("--no-api --config-file {} --id vm-aaaa01", paths.config_file.display()));
    let config: serde_json::Value = serde_json::from_slice(&std::fs::read(&paths.config_file).unwrap()).unwrap();
    let drives: Vec<&str> = config["drives"].as_array().unwrap().iter().map(|d| d["drive_id"].as_str().unwrap()).collect();
    assert_eq!(drives, ["rootfs", "config", "volume"]);
    assert_eq!(config["drives"][2]["path_on_host"], volume.display().to_string());
    assert_eq!(config["network-interfaces"][0]["host_dev_name"], "ahotap2");
    assert_eq!(config["machine-config"]["vcpu_count"], 2);

    // Network: routed tap, and the ruleset now pins it to its address.
    let ip_log = host.log("ip.log");
    for line in [
        "tuntap add dev ahotap2 mode tap",
        "addr add 10.77.0.1/32 dev ahotap2",
        "link set dev ahotap2 up",
        "route replace 10.77.0.2/32 dev ahotap2",
    ] {
        assert!(ip_log.contains(line), "missing `{line}` in\n{ip_log}");
    }
    assert!(host.log("nft.applied").contains("iifname \"ahotap2\" ip saddr != 10.77.0.2 drop"));

    // Ceilings, with the VMM inside the cgroup.
    let cg = host.root.join("cgroup/aho-cloud/vm-aaaa01");
    let read = |f: &str| std::fs::read_to_string(cg.join(f)).unwrap();
    assert_eq!(read("cpu.max"), "200000 100000");
    assert_eq!(read("memory.max"), (576u64 * 1024 * 1024).to_string());
    assert_eq!(read("pids.max"), "48");
    assert_eq!(read("cgroup.procs"), pid.to_string());

    // A second replica of the same digest reuses the unpacked image.
    let s2 = spec(&mut alloc, "vm-aaaa02", 1, false);
    fc.start(s2).await.unwrap();
    assert_eq!(host.log("skopeo.log").lines().count(), 1);
    assert_eq!(host.log("umoci.log").lines().count(), 1);

    fc.stop("vm-aaaa01").await.unwrap();
    assert!(!fc.running("vm-aaaa01").await);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(gone(pid), "VMM process survived stop");
    assert!(!cg.exists());
    assert!(!paths.dir.exists());
    assert!(volume.exists(), "volumes outlive replicas");
    assert!(host.log("ip.log").contains("link del dev ahotap2"));
    let rules = host.log("nft.applied");
    assert!(!rules.contains("\"ahotap2\"") && rules.contains("iifname \"ahotap3\" ip saddr != 10.77.0.3 drop"));
    fc.stop("vm-aaaa01").await.unwrap();
    fc.stop("vm-aaaa02").await.unwrap();
}

#[tokio::test]
async fn a_failed_boot_leaves_nothing_behind() {
    let host = Host::new();
    let fc = host.backend();
    fc.prepare_host().await.unwrap();
    std::fs::write(host.root.join("log/fail-mkfs"), b"").unwrap();
    let mut alloc = Allocator::new("10.77.0.0/24".parse().unwrap()).unwrap();
    let s = spec(&mut alloc, "vm-bbbb01", 0, false);
    let paths = fc.paths(&s);
    let err = fc.start(s).await.unwrap_err();
    assert!(err.to_string().contains("No space left on device"), "{err}");
    assert!(!fc.running("vm-bbbb01").await);
    assert!(!paths.dir.exists());
    assert!(!host.root.join("cgroup/aho-cloud/vm-bbbb01").exists());
    assert!(host.log("fc.log").is_empty(), "the VMM must not start");

    let bad = spec(&mut alloc, "../escape", 0, false);
    assert!(matches!(fc.start(bad).await, Err(VmmError::NotFound(_))));
}

struct Guest;

impl Control for Guest {
    fn health(&self, argv: Vec<String>, _timeout: Duration) -> BoxFuture<'_, Result<i32, String>> {
        Box::pin(async move { Ok(if argv == ["true"] { 0 } else { 1 }) })
    }
    fn restart(&self) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    fn status(&self) -> ProcessState {
        ProcessState { pid: Some(1), starts: 1, last_exit: None }
    }
}

#[tokio::test]
async fn control_connections_go_through_the_vsock_socket() {
    let host = Host::new();
    let fc = host.backend();
    fc.prepare_host().await.unwrap();
    let mut alloc = Allocator::new("10.77.0.0/24".parse().unwrap()).unwrap();
    let s = spec(&mut alloc, "vm-cccc01", 0, false);
    let paths = fc.paths(&s);
    fc.start(s).await.unwrap();

    // Stand in for Firecracker's vsock multiplexer and the guest behind it.
    let listener = UnixListener::bind(&paths.vsock).unwrap();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut stream = BufReader::new(stream);
        let mut line = String::new();
        stream.read_line(&mut line).await.unwrap();
        assert_eq!(line, "CONNECT 52\n");
        stream.get_mut().write_all(b"OK 1073741824\n").await.unwrap();
        control::serve(stream, &Guest).await.unwrap();
    });
    let conn = fc.connect("vm-cccc01").await.unwrap();
    let mut client = Client::new(conn);
    assert_eq!(client.health(&["true".into()], Duration::from_secs(1)).await.unwrap(), 0);
    assert_eq!(client.status().await.unwrap().pid, Some(1));
    assert!(matches!(fc.connect("vm-nope").await, Err(VmmError::NotFound(_))));
    fc.stop("vm-cccc01").await.unwrap();
}
