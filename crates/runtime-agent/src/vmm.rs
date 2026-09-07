//! The VM backend seam. [`crate::firecracker::Firecracker`] is the real one; the supervisor
//! only ever talks to this trait.

use std::net::{Ipv4Addr, SocketAddr};

use cloud_contract::ledger::Stage;
use futures::future::BoxFuture;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::guest::GuestConfig;
use crate::net::Lease;

/// A byte stream to a guest's control server.
pub trait GuestStream: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> GuestStream for T {}

pub type GuestConn = Box<dyn GuestStream>;

/// A persistent disk that outlives replicas: the same `(project, stage, process, index)` gets
/// the same volume across restarts and releases.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolumeSpec {
    pub size_mb: u32,
}

/// Everything needed to boot one replica.
#[derive(Clone, Debug)]
pub struct ReplicaSpec {
    /// Unique per boot: `[a-z0-9-]`, at most 64 bytes (Firecracker `--id`, cgroup name).
    pub id: String,
    pub project: String,
    pub stage: Stage,
    pub process: String,
    pub index: u16,
    pub release: String,
    pub image_digest: String,
    pub lease: Lease,
    /// The config drive; the backend fills in `image` from the pulled image.
    pub guest: GuestConfig,
    pub volume: Option<VolumeSpec>,
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

    /// Where the host reaches `port` of the replica at `ip`.
    fn endpoint(&self, _id: &str, ip: Ipv4Addr, port: u16) -> SocketAddr {
        SocketAddr::new(ip.into(), port)
    }
}
