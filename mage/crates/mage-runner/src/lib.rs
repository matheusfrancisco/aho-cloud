//! Runner role hexagon (`mage start runner`). Depends on `mage-core` only: the runner holds no
//! DB, blob, vault, GitHub, Linear, Slack, or tenant model credentials.
//! See `mage/docs/internals/{runs,executor,harness}.md`.

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
