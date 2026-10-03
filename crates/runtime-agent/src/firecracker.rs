//! The Firecracker backend (Linux/KVM hosts). Per replica: an ext4 rootfs built from the image
//! digest, an optional persistent volume disk, a config drive, a routed tap, a cgroup with
//! ceilings, and one `firecracker --no-api --config-file` process. The guest's control and exec
//! servers are reached through Firecracker's vsock socket.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use futures::future::BoxFuture;
use parking_lot::Mutex;
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::Child;

use crate::cgroup::{Cgroups, Limits};
use crate::exec::run;
use crate::guest::{CONFIG_DEVICE, CONFIG_DRIVE_ENV, CONTROL_PORT, EXEC_PORT, GUEST_INIT_PATH};
use crate::image::{ImageStore, ImageStoreConfig};
use crate::net::{FirewallPlan, GATEWAY_DEVICE, Ipv4Cidr, Lease, nft_ruleset};
use crate::vmm::{DiskId, GuestConn, ReplicaSpec, Started, Vmm, VmmError};

/// Under `volumes/`, the directory of sandbox disks; it has a `.`, so no project slug names it.
pub const SANDBOX_VOLUMES: &str = "aho.sandboxes";

/// Guest CID of every VM; each VM has its own vsock device, so CIDs need not be unique.
pub const GUEST_CID: u32 = 3;

/// Host-level settings for the backend.
#[derive(Clone, Debug)]
pub struct FirecrackerConfig {
    /// Holds `replicas/<id>/`, `volumes/`, `images/` (when not configured elsewhere) and `nft.rules`.
    pub state_dir: PathBuf,
    pub firecracker: PathBuf,
    pub ip: PathBuf,
    pub nft: PathBuf,
    /// Uncompressed guest kernel (`vmlinux`).
    pub kernel: PathBuf,
    pub images: ImageStoreConfig,
    pub cgroup_root: PathBuf,
    /// Defaults; a replica may carry its own.
    pub vcpus: u8,
    pub mem_mib: u32,
    pub rootfs_mib: u32,
    /// Allowance for the VMM process on top of guest memory in `memory.max`.
    pub memory_overhead_mib: u32,
    pub pids_max: u32,
    pub gateway: Ipv4Addr,
    pub proxy_port: u16,
    pub dns_port: u16,
    /// Networks allowed to open connections to guests (remote edge nodes).
    pub inbound_sources: Vec<Ipv4Cidr>,
}

/// Firecracker's `--config-file` document.
#[derive(Clone, Debug, Serialize)]
pub struct VmConfig {
    #[serde(rename = "boot-source")]
    pub boot_source: BootSource,
    pub drives: Vec<Drive>,
    #[serde(rename = "machine-config")]
    pub machine_config: MachineConfig,
    #[serde(rename = "network-interfaces")]
    pub network_interfaces: Vec<NetworkInterface>,
    pub vsock: Vsock,
}

#[derive(Clone, Debug, Serialize)]
pub struct BootSource {
    pub kernel_image_path: PathBuf,
    pub boot_args: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Drive {
    pub drive_id: String,
    pub path_on_host: PathBuf,
    pub is_root_device: bool,
    pub is_read_only: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct MachineConfig {
    pub vcpu_count: u8,
    pub mem_size_mib: u32,
    pub smt: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct NetworkInterface {
    pub iface_id: String,
    pub guest_mac: String,
    pub host_dev_name: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Vsock {
    pub guest_cid: u32,
    pub uds_path: PathBuf,
}

/// Where one replica's files live on the host.
#[derive(Clone, Debug)]
pub struct VmPaths {
    pub dir: PathBuf,
    pub rootfs: PathBuf,
    pub config_drive: PathBuf,
    pub volume: Option<PathBuf>,
    pub vsock: PathBuf,
    pub config_file: PathBuf,
    pub console: PathBuf,
}

/// Kernel command line: serial console, reboot-exits-VMM, static `ip=` config, our init, and
/// the config drive (unknown `KEY=value` parameters reach init as environment variables).
pub fn boot_args(lease: &Lease, hostname: &str) -> String {
    format!(
        "console=ttyS0 reboot=k panic=1 pci=off init={GUEST_INIT_PATH} ip={ip}::{gw}:{mask}:{hostname}:eth0:off:{gw} {CONFIG_DRIVE_ENV}={CONFIG_DEVICE}",
        ip = lease.ip,
        gw = lease.gateway,
        mask = lease.netmask(),
    )
}

/// The `--config-file` for `spec`. Drive order fixes guest devices: rootfs `/dev/vda`,
/// config `/dev/vdb`, volume `/dev/vdc`.
pub fn vm_config(kernel: &Path, vcpus: u8, mem_mib: u32, spec: &ReplicaSpec, paths: &VmPaths) -> VmConfig {
    let mut drives = vec![
        Drive { drive_id: "rootfs".into(), path_on_host: paths.rootfs.clone(), is_root_device: true, is_read_only: false },
        Drive { drive_id: "config".into(), path_on_host: paths.config_drive.clone(), is_root_device: false, is_read_only: true },
    ];
    if let Some(volume) = &paths.volume {
        drives.push(Drive { drive_id: "volume".into(), path_on_host: volume.clone(), is_root_device: false, is_read_only: false });
    }
    VmConfig {
        boot_source: BootSource { kernel_image_path: kernel.to_owned(), boot_args: boot_args(&spec.lease, &spec.hostname) },
        drives,
        machine_config: MachineConfig { vcpu_count: vcpus, mem_size_mib: mem_mib, smt: false },
        network_interfaces: vec![NetworkInterface {
            iface_id: "eth0".into(),
            guest_mac: spec.lease.mac.clone(),
            host_dev_name: spec.lease.tap.clone(),
        }],
        vsock: Vsock { guest_cid: GUEST_CID, uds_path: paths.vsock.clone() },
    }
}

fn io_err(what: &Path) -> impl FnOnce(io::Error) -> VmmError + '_ {
    move |e| VmmError::Io(what.display().to_string(), e)
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

struct Vm {
    child: Child,
    lease: Lease,
    vsock: PathBuf,
}

/// The real backend.
pub struct Firecracker {
    cfg: FirecrackerConfig,
    images: ImageStore,
    cgroups: Cgroups,
    vms: Mutex<HashMap<String, Vm>>,
    firewall: tokio::sync::Mutex<()>,
}

impl Firecracker {
    pub fn new(cfg: FirecrackerConfig) -> Self {
        let images = ImageStore::new(cfg.images.clone());
        let cgroups = Cgroups::new(cfg.cgroup_root.clone());
        Firecracker { cfg, images, cgroups, vms: Mutex::new(HashMap::new()), firewall: tokio::sync::Mutex::new(()) }
    }

    /// Paths for replica `spec`.
    pub fn paths(&self, spec: &ReplicaSpec) -> VmPaths {
        let dir = self.cfg.state_dir.join("replicas").join(&spec.id);
        VmPaths {
            rootfs: dir.join("rootfs.ext4"),
            config_drive: dir.join("config.drive"),
            volume: spec.volume.as_ref().map(|v| self.volume_path(&v.disk)),
            vsock: dir.join("vsock.sock"),
            config_file: dir.join("firecracker.json"),
            console: dir.join("console.log"),
            dir,
        }
    }

    /// Where `disk` lives: `volumes/<project>/<stage>/<process>-<index>.ext4`, or
    /// `volumes/`[`SANDBOX_VOLUMES`]`/<slug>.ext4`.
    pub fn volume_path(&self, disk: &DiskId) -> PathBuf {
        let volumes = self.cfg.state_dir.join("volumes");
        match disk {
            DiskId::Process { project, stage, process, index } => {
                volumes.join(project).join(stage.as_str()).join(format!("{process}-{index}.ext4"))
            }
            DiskId::Sandbox(slug) => volumes.join(SANDBOX_VOLUMES).join(format!("{slug}.ext4")),
        }
    }

    fn vcpus(&self, spec: &ReplicaSpec) -> u8 {
        spec.cpus.unwrap_or(self.cfg.vcpus)
    }

    fn mem_mib(&self, spec: &ReplicaSpec) -> u32 {
        spec.mem_mib.unwrap_or(self.cfg.mem_mib)
    }

    async fn vsock_stream(&self, id: &str) -> Result<UnixStream, VmmError> {
        let path = self.vms.lock().get(id).map(|vm| vm.vsock.clone()).ok_or_else(|| VmmError::NotFound(id.into()))?;
        UnixStream::connect(&path).await.map_err(io_err(&path))
    }

    /// One-time host setup: state directories, the cgroup root, the gateway address on a
    /// dummy device (so the egress proxy can bind it), and the initial ruleset.
    pub async fn prepare_host(&self) -> Result<(), VmmError> {
        for sub in ["replicas", "volumes"] {
            let dir = self.cfg.state_dir.join(sub);
            tokio::fs::create_dir_all(&dir).await.map_err(io_err(&dir))?;
        }
        self.cgroups.init()?;
        let ip = &self.cfg.ip;
        if run(ip, ["link", "show", "dev", GATEWAY_DEVICE]).await.is_err() {
            run(ip, ["link", "add", GATEWAY_DEVICE, "type", "dummy"]).await?;
        }
        let gw = format!("{}/32", self.cfg.gateway);
        run(ip, ["addr", "replace", gw.as_str(), "dev", GATEWAY_DEVICE]).await?;
        run(ip, ["link", "set", "dev", GATEWAY_DEVICE, "up"]).await?;
        self.apply_firewall().await
    }

    /// Rewrites the ruleset for the replicas running now and loads it with `nft -f`.
    async fn apply_firewall(&self) -> Result<(), VmmError> {
        let _serial = self.firewall.lock().await;
        let mut guests: Vec<(String, Ipv4Addr)> = self.vms.lock().values().map(|vm| (vm.lease.tap.clone(), vm.lease.ip)).collect();
        guests.sort();
        let plan = FirewallPlan {
            gateway: self.cfg.gateway,
            proxy_port: self.cfg.proxy_port,
            dns_port: self.cfg.dns_port,
            inbound_sources: self.cfg.inbound_sources.clone(),
            guests,
        };
        let path = self.cfg.state_dir.join("nft.rules");
        tokio::fs::write(&path, nft_ruleset(&plan)).await.map_err(io_err(&path))?;
        run(&self.cfg.nft, [OsStr::new("-f"), path.as_os_str()]).await?;
        Ok(())
    }

    async fn setup_tap(&self, lease: &Lease) -> Result<(), VmmError> {
        let ip = &self.cfg.ip;
        let tap = lease.tap.as_str();
        // A tap left behind by a crashed agent would make `tuntap add` fail.
        let _ = run(ip, ["link", "del", "dev", tap]).await;
        run(ip, ["tuntap", "add", "dev", tap, "mode", "tap"]).await?;
        let gw = format!("{}/32", lease.gateway);
        run(ip, ["addr", "add", gw.as_str(), "dev", tap]).await?;
        run(ip, ["link", "set", "dev", tap, "up"]).await?;
        let guest = format!("{}/32", lease.ip);
        run(ip, ["route", "replace", guest.as_str(), "dev", tap]).await?;
        Ok(())
    }

    async fn teardown(&self, id: &str, lease: &Lease, dir: &Path) {
        if let Err(e) = run(&self.cfg.ip, ["link", "del", "dev", lease.tap.as_str()]).await {
            tracing::warn!(id, error = %e, "tap removal failed");
        }
        if let Err(e) = self.cgroups.remove(id) {
            tracing::warn!(id, error = %e, "cgroup removal failed");
        }
        if let Err(e) = tokio::fs::remove_dir_all(dir).await
            && e.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(id, error = %e, "replica directory removal failed");
        }
    }

    /// Builds the disks and boots; on error the caller tears down.
    async fn boot(&self, spec: &ReplicaSpec, paths: &VmPaths) -> Result<Child, VmmError> {
        let image = self.images.prepare(&spec.repo, &spec.image_digest).await?;
        self.images.rootfs_disk(&image, &paths.rootfs, self.cfg.rootfs_mib).await?;
        if let (Some(volume), Some(path)) = (&spec.volume, &paths.volume)
            && tokio::fs::metadata(path).await.is_err()
        {
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await.map_err(io_err(parent))?;
            }
            self.images.empty_disk(path, volume.size_mb).await?;
        }
        let mut guest = spec.guest.clone();
        guest.image = image.config.clone();
        tokio::fs::write(&paths.config_drive, guest.to_drive()?).await.map_err(io_err(&paths.config_drive))?;

        let config = vm_config(&self.cfg.kernel, self.vcpus(spec), self.mem_mib(spec), spec, paths);
        let json = serde_json::to_vec_pretty(&config).map_err(|e| VmmError::Io("firecracker.json".into(), e.into()))?;
        tokio::fs::write(&paths.config_file, json).await.map_err(io_err(&paths.config_file))?;

        self.setup_tap(&spec.lease).await?;
        let limits = Limits::for_vm(self.vcpus(spec), self.mem_mib(spec), self.cfg.memory_overhead_mib, self.cfg.pids_max);
        let cgroup = self.cgroups.create(&spec.id, &limits)?;

        let console = std::fs::File::create(&paths.console).map_err(io_err(&paths.console))?;
        let console_err = console.try_clone().map_err(io_err(&paths.console))?;
        let mut child = tokio::process::Command::new(&self.cfg.firecracker)
            .arg("--no-api")
            .arg("--config-file")
            .arg(&paths.config_file)
            .arg("--id")
            .arg(&spec.id)
            .stdin(Stdio::null())
            .stdout(console)
            .stderr(console_err)
            .kill_on_drop(true)
            .spawn()
            .map_err(io_err(&self.cfg.firecracker))?;
        let Some(pid) = child.id() else {
            return Err(VmmError::Io(spec.id.clone(), io::Error::other("firecracker exited at once")));
        };
        if let Err(e) = cgroup.add_pid(pid) {
            let _ = child.kill().await;
            return Err(e.into());
        }
        Ok(child)
    }
}

impl Vmm for Firecracker {
    fn start(&self, spec: ReplicaSpec) -> BoxFuture<'_, Result<Started, VmmError>> {
        Box::pin(async move {
            if !valid_id(&spec.id) {
                return Err(VmmError::NotFound(spec.id));
            }
            let paths = self.paths(&spec);
            match tokio::fs::remove_dir_all(&paths.dir).await {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(io_err(&paths.dir)(e)),
            }
            tokio::fs::create_dir_all(&paths.dir).await.map_err(io_err(&paths.dir))?;
            let child = match self.boot(&spec, &paths).await {
                Ok(child) => child,
                Err(e) => {
                    self.teardown(&spec.id, &spec.lease, &paths.dir).await;
                    return Err(e);
                }
            };
            let pid = child.id();
            self.vms.lock().insert(spec.id.clone(), Vm { child, lease: spec.lease.clone(), vsock: paths.vsock });
            if let Err(e) = self.apply_firewall().await {
                // A guest the firewall does not know about must not run.
                let _ = self.stop(&spec.id).await;
                return Err(e);
            }
            tracing::info!(id = %spec.id, repo = %spec.repo, hostname = %spec.hostname, ?pid, ip = %spec.lease.ip, "replica booted");
            Ok(Started { pid })
        })
    }

    fn stop<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), VmmError>> {
        Box::pin(async move {
            let Some(mut vm) = self.vms.lock().remove(id) else {
                return Ok(());
            };
            let _ = vm.child.start_kill();
            let _ = vm.child.wait().await;
            let dir = self.cfg.state_dir.join("replicas").join(id);
            self.teardown(id, &vm.lease, &dir).await;
            self.apply_firewall().await
        })
    }

    fn running<'a>(&'a self, id: &'a str) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            let mut vms = self.vms.lock();
            vms.get_mut(id).is_some_and(|vm| matches!(vm.child.try_wait(), Ok(None)))
        })
    }

    fn connect<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<GuestConn, VmmError>> {
        Box::pin(async move { vsock_handshake(self.vsock_stream(id).await?, CONTROL_PORT).await })
    }

    fn exec<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<GuestConn, VmmError>> {
        Box::pin(async move { vsock_handshake(self.vsock_stream(id).await?, EXEC_PORT).await })
    }

    fn destroy_volume<'a>(&'a self, disk: &'a DiskId) -> BoxFuture<'a, Result<(), VmmError>> {
        Box::pin(async move {
            let names = match disk {
                DiskId::Process { project, process, .. } => vec![project.as_str(), process.as_str()],
                DiskId::Sandbox(slug) => vec![slug.as_str()],
            };
            if !names.iter().all(|n| valid_id(n)) {
                return Err(VmmError::NotFound(format!("{disk:?}")));
            }
            let path = self.volume_path(disk);
            match tokio::fs::remove_file(&path).await {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(io_err(&path)(e)),
            }
        })
    }
}

/// Firecracker's host-initiated vsock handshake: `CONNECT <port>\n` → `OK <host port>\n`.
pub async fn vsock_handshake(stream: UnixStream, port: u32) -> Result<GuestConn, VmmError> {
    let mut stream = BufReader::new(stream);
    stream.get_mut().write_all(format!("CONNECT {port}\n").as_bytes()).await.map_err(|e| VmmError::Control(e.to_string()))?;
    let mut line = String::new();
    let mut limited = (&mut stream).take(64);
    limited.read_line(&mut line).await.map_err(|e| VmmError::Control(e.to_string()))?;
    if !line.starts_with("OK ") || !line.ends_with('\n') {
        return Err(VmmError::Control(format!("vsock handshake refused: {:?}", line.trim_end())));
    }
    Ok(Box::new(stream))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use cloud_contract::ledger::Stage;

    use super::*;
    use crate::guest::GuestConfig;
    use crate::net::{Allocator, Ipv4Cidr};
    use crate::vmm::VolumeSpec;

    #[test]
    fn config_file_for_a_sample_replica() {
        let mut alloc = Allocator::new("10.77.0.0/24".parse::<Ipv4Cidr>().unwrap()).unwrap();
        let lease = alloc.allocate().unwrap();
        let spec = ReplicaSpec {
            id: "vm-0a1b2c".into(),
            repo: "shop".into(),
            image_digest: format!("sha256:{}", "a".repeat(64)),
            hostname: "web-0".into(),
            labels: vec![],
            lease,
            guest: GuestConfig {
                process: "web".into(),
                command: None,
                image: Default::default(),
                env: BTreeMap::new(),
                port: Some(8080),
                volume: None,
                dns: vec![],
            },
            volume: Some(VolumeSpec {
                disk: DiskId::Process { project: "shop".into(), stage: Stage::Live, process: "web".into(), index: 0 },
                size_mb: 1024,
            }),
            cpus: None,
            mem_mib: None,
        };
        let dir = PathBuf::from("/var/lib/aho/replicas/vm-0a1b2c");
        let paths = VmPaths {
            rootfs: dir.join("rootfs.ext4"),
            config_drive: dir.join("config.drive"),
            volume: Some("/var/lib/aho/volumes/shop/live/web-0.ext4".into()),
            vsock: dir.join("vsock.sock"),
            config_file: dir.join("firecracker.json"),
            console: dir.join("console.log"),
            dir: dir.clone(),
        };
        let json = serde_json::to_value(vm_config(Path::new("/var/lib/aho/vmlinux"), 2, 1024, &spec, &paths)).unwrap();
        let expected = serde_json::json!({
            "boot-source": {
                "kernel_image_path": "/var/lib/aho/vmlinux",
                "boot_args": "console=ttyS0 reboot=k panic=1 pci=off init=/.cloud/guest ip=10.77.0.2::10.77.0.1:255.255.255.0:web-0:eth0:off:10.77.0.1 CLOUD_GUEST_CONFIG=/dev/vdb"
            },
            "drives": [
                {"drive_id": "rootfs", "path_on_host": "/var/lib/aho/replicas/vm-0a1b2c/rootfs.ext4", "is_root_device": true, "is_read_only": false},
                {"drive_id": "config", "path_on_host": "/var/lib/aho/replicas/vm-0a1b2c/config.drive", "is_root_device": false, "is_read_only": true},
                {"drive_id": "volume", "path_on_host": "/var/lib/aho/volumes/shop/live/web-0.ext4", "is_root_device": false, "is_read_only": false}
            ],
            "machine-config": {"vcpu_count": 2, "mem_size_mib": 1024, "smt": false},
            "network-interfaces": [{"iface_id": "eth0", "guest_mac": "02:fc:0a:4d:00:02", "host_dev_name": "ahotap2"}],
            "vsock": {"guest_cid": 3, "uds_path": "/var/lib/aho/replicas/vm-0a1b2c/vsock.sock"}
        });
        assert_eq!(json, expected);
    }

    #[test]
    fn ids_are_restricted() {
        assert!(valid_id("vm-0a1b2c"));
        assert!(!valid_id("../x"));
        assert!(!valid_id(&"a".repeat(65)));
        assert!(!valid_id(""));
    }

    #[tokio::test]
    async fn vsock_handshake_accepts_ok_and_rejects_anything_else() {
        use tokio::net::UnixListener;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            for reply in ["OK 1073741824\n", "nope\n"] {
                let (mut s, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 11];
                s.read_exact(&mut buf).await.unwrap();
                assert_eq!(&buf, b"CONNECT 52\n");
                s.write_all(reply.as_bytes()).await.unwrap();
                s.write_all(b"after").await.unwrap();
                let mut rest = Vec::new();
                let _ = s.read_to_end(&mut rest).await;
            }
        });
        let mut conn = vsock_handshake(UnixStream::connect(&path).await.unwrap(), 52).await.unwrap();
        let mut after = [0u8; 5];
        conn.read_exact(&mut after).await.unwrap();
        assert_eq!(&after, b"after");
        drop(conn);
        let refused = vsock_handshake(UnixStream::connect(&path).await.unwrap(), 52).await;
        assert!(matches!(refused, Err(VmmError::Control(_))));
        server.await.unwrap();
    }
}
