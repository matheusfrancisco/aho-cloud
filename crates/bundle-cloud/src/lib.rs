//! The aho cloud product composition: everything `aho-lab` ships, plus drafting and proposing
//! releases (`cloud_draft`, `cloud_propose`) and the principal contract (`edge_routes`,
//! `edge_verifier`, prompt section). The `ahoc` binary serves it over aho rpc.

#![forbid(unsafe_code)]

use aho_cap::Composition;

pub const NAME: &str = "aho-cloud";

/// `aho_lab::lab()` plus the cloud capabilities. `cloud_propose` stays Locked until the
/// `ahocloud` credential is set.
pub fn cloud() -> Composition {
    Composition::new(NAME)
        .include(aho_lab::lab())
        .consume::<cap_deploy::DeployDraft>()
        .consume::<cap_deploy::DeployPropose>()
        .consume::<cap_edge_contract::EdgeContract>()
}
