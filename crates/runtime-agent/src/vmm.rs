//! The VM backend seam. [`crate::firecracker::Firecracker`] is the real one; the supervisor
//! only ever talks to this trait.

use std::net::{Ipv4Addr, SocketAddr};

use cloud_contract::ledger::Stage;
use futures::future::BoxFuture;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::guest::{GUEST_INIT_PATH, GuestConfig};
use crate::net::Lease;

/// A byte stream to a guest's control or exec server.
pub trait GuestStream: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> GuestStream for T {}

pub type GuestConn = Box<dyn GuestStream>;

/// Label keys the supervisor puts on replicas (Docker `--label`s).
pub mod labels {
    pub const PROJECT: &str = "aho-cloud.project";
    pub const STAGE: &str = "aho-cloud.stage";
    pub const PROCESS: &str = "aho-cloud.process";
    pub const INDEX: &str = "aho-cloud.index";
    pub const RELEASE: &str = "aho-cloud.release";
    pub const SANDBOX: &str = "aho-cloud.sandbox";
}

/// Which persistent disk: the same id is the same disk across boots, releases and specs.
/// Deployment and sandbox disks can never name each other.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiskId {
    /// One process slot of a deployment.
    Process { project: String, stage: Stage, process: String, index: u16 },
    /// A sandbox's workspace, by slug.
    Sandbox(String),
}

/// A persistent disk that outlives replicas.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolumeSpec {
    pub disk: DiskId,
    pub size_mb: u32,
}

/// Everything needed to boot one replica.
#[derive(Clone, Debug)]
pub struct ReplicaSpec {
    /// Unique per boot: `[a-z0-9-]`, at most 64 bytes (Firecracker `--id`, cgroup name,
    /// container name).
    pub id: String,
    /// Substituted for `{project}` in the image-repo template: the project of a deployment,
    /// `SandboxSpec.image_repo` of a sandbox.
    pub repo: String,
    pub image_digest: String,
    /// The guest's hostname.
    pub hostname: String,
    /// What the replica belongs to, as `(key, value)` pairs (see [`labels`]).
    pub labels: Vec<(String, String)>,
    pub lease: Lease,
    /// The config drive; the backend fills in `image` from the pulled image.
    pub guest: GuestConfig,
    pub volume: Option<VolumeSpec>,
    /// vCPUs and memory instead of the backend's defaults.
    pub cpus: Option<u8>,
    pub mem_mib: Option<u32>,
}

impl ReplicaSpec {
    /// The value of label `key`.
    pub fn label(&self, key: &str) -> Option<&str> {
        self.labels.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Started {
    /// Host pid of the VMM process, when there is one.
    pub pid: Option<u32>,
}

#[derive(Debug, thiserror::Error)]
pub enum VmmError {
    #[error(transparent)]
    Image(#[from] crate::image::ImageError),
    #[error(transparent)]
    Exec(#[from] crate::exec::ExecError),
    #[error(transparent)]
    Cgroup(#[from] crate::cgroup::CgroupError),
    #[error(transparent)]
    Guest(#[from] crate::guest::GuestError),
    #[error("{0}: {1}")]
    Io(String, std::io::Error),
    #[error("no replica `{0}`")]
    NotFound(String),
    #[error("guest control: {0}")]
    Control(String),
    #[error("docker: {0}")]
    Docker(String),
}

/// Boots, stops and reaches replicas.
pub trait Vmm: Send + Sync + 'static {
    fn start(&self, spec: ReplicaSpec) -> BoxFuture<'_, Result<Started, VmmError>>;

    /// Stops and discards the replica (its volume stays). Unknown ids are already stopped.
    fn stop<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), VmmError>>;

    /// Whether the replica's VM is still up.
    fn running<'a>(&'a self, id: &'a str) -> BoxFuture<'a, bool>;

    /// Opens a control connection to the replica's `cloud-guest`.
    fn connect<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<GuestConn, VmmError>>;

    /// Opens a fresh stream to the replica's exec server: one
    /// [`crate::guest::exec`] session, ended by dropping the stream.
    fn exec<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<GuestConn, VmmError>>;

    /// Destroys a persistent disk; no replica may be using it. A disk that does not exist is
    /// already destroyed.
    fn destroy_volume<'a>(&'a self, disk: &'a DiskId) -> BoxFuture<'a, Result<(), VmmError>>;

    /// Where the host reaches `port` of the replica at `ip`.
    fn endpoint(&self, _id: &str, ip: Ipv4Addr, port: u16) -> SocketAddr {
        SocketAddr::new(ip.into(), port)
    }

    /// Path of `cloud-guest` inside this backend's guests (run by exec sessions for file
    /// listings and reads).
    fn guest_program(&self) -> &str {
        GUEST_INIT_PATH
    }
}
