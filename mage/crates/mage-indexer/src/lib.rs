//! Indexer role hexagon (`mage start indexer`). See `mage/docs/internals/knowledge.md`.
//! Layout: domain/ ports/ app/ adapters/{driving,driven}/ testing/ compose.rs.

pub mod adapters;
pub mod app;
pub mod compose;
pub mod config;
pub mod domain;
pub mod error;
pub mod ports;

pub use compose::{RunError, run};
pub use config::Config;

#[cfg(any(test, feature = "testing"))]
pub mod testing;
