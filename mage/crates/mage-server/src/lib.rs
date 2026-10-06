//! Server role hexagon (`mage start server`). See `mage/docs/internals/server.md` and the part docs.
//! Layout: domain/ ports/ app/ adapters/{driving,driven}/ testing/ compose.rs.
//! `app/` and `domain/` never import `crate::adapters`.

pub mod adapters;
pub mod app;
pub mod compose;
pub mod config;
pub mod domain;
pub mod error;
pub mod ports;

pub use compose::{Assets, RunError, run};
pub use config::Config;

#[cfg(any(test, feature = "testing"))]
pub mod testing;
