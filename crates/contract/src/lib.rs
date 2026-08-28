//! The contracts of aho cloud. Stack-agnostic: nothing here depends on aho, so a project built
//! with any tool produces the same documents and passes the same gate.
//!
//! - [`manifest`]: `deploy.aho.cloud/v1`, the handoff document;
//! - [`validate`]: the gate's rules, as typed refusals;
//! - [`diff`]: what a release widens or narrows; the approval card;
//! - [`policy`]: who must approve, and standing orders for on-call;
//! - [`edge`]: `edge.toml` route modes;
//! - [`principal`]: the `X-Principal-JWT` claims the app verifies;
//! - [`ledger`]: release-ledger entry bodies;
//! - [`draft`]: a manifest with per-field evidence, before confirmation.

#![forbid(unsafe_code)]

pub mod canonical;
pub mod diff;
pub mod draft;
pub mod edge;
pub mod ledger;
pub mod manifest;
pub mod policy;
pub mod principal;
pub mod ts;
pub mod validate;

pub use manifest::Manifest;
pub use validate::{Refusal, RefusalCode, validate};

/// The JSON Schema published for hand-written manifests.
pub fn manifest_schema() -> serde_json::Value {
    serde_json::to_value(schemars::schema_for!(Manifest)).unwrap_or_default()
}
