//! The aho cloud runtime agent: runs approved deployments as Firecracker microVMs on a
//! Linux/KVM host (or, for local development, as Docker containers), and `cloud-guest`, the
//! PID 1 inside each VM.
//!
//! - [`supervisor`]: desired state per (project, stage); reconcile, scale, restart,
//!   maintenance, sleep, health checks;
//! - [`vmm`]: the VM backend seam; [`firecracker`]: the real backend; [`docker`]: the local
//!   development backend (no isolation boundary);
//! - [`image`]: OCI image → ext4 rootfs via skopeo, umoci and mkfs.ext4;
//! - [`net`]: guest addresses, MACs, taps and the nftables ruleset;
//! - [`egress`]: the CONNECT proxy enforcing each deployment's egress allowlist;
//! - [`cgroup`]: cgroup v2 ceilings per replica;
//! - [`api`]: the internal HTTP API (`cloud_api::internal::routes::DEPLOYMENT*`);
//! - [`guest`]: config drive, control protocol and process runner used by `cloud-guest`.

#![forbid(unsafe_code)]

pub mod api;
pub mod cgroup;
pub mod docker;
pub mod egress;
pub mod exec;
pub mod firecracker;
pub mod guest;
pub mod health;
pub mod image;
pub mod net;
pub mod supervisor;
pub mod vmm;

pub use supervisor::{Supervisor, SupervisorConfig, SupervisorError};
pub use vmm::{ReplicaSpec, Started, Vmm, VmmError};
