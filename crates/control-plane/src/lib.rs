//! The aho cloud control plane: an axum server for every route in [`cloud_api::routes`] plus the
//! internal site directory, observation and archive routes.
//!
//! - auth: device flow binding Ed25519 device keys, WebAuthn passkeys ([`webauthn`]), bearer
//!   sessions, org membership and owner checks ([`auth`]);
//! - releases: validation, provenance and upload checks, the per-project ledger ([`ledgers`]),
//!   builds, taint scan ([`taint`]), activation and deployment ([`ops`]), signed approvals
//!   ([`approvals`]);
//! - domains through `cloud_domains::Registry`, the edge's site directory ([`sites`]);
//! - storage behind [`store::Store`] (memory or Postgres) and [`blobs::BlobStore`];
//! - sibling services behind [`ports`], with HTTP adapters and in-process [`fakes`].

#![forbid(unsafe_code)]

pub mod approvals;
pub mod auth;
pub mod blobs;
pub mod clock;
pub mod config;
pub mod crypto;
pub mod error;
pub mod fakes;
pub mod ledgers;
pub mod ops;
pub mod ports;
pub mod routes;
pub mod sites;
pub mod state;
pub mod store;
pub mod taint;
pub mod view;
pub mod webauthn;

pub use config::{Config, MasterKey};
pub use error::AppError;
pub use routes::router;
pub use state::{AppState, Services};
