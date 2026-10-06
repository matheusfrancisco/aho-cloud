//! Runner use cases (`mage_runner::app::runner`; runs.md "Runner role", executor.md "Use cases").
//!
//! | Module | Contents |
//! |---|---|
//! | [`daemon`] | startup (recovery + register), lease capacity, run task registry, drain |
//! | [`task`] | per-run task: shared deps/config, dispatch per kind, Verify task, finish |
//! | [`session`] | harness run task (Learn, Triage, Diagnose, Fix) |
//! | [`pump`] | event buffer + flusher, command poller, heartbeat ticker, self-fencing |
//! | [`artifacts`] | artifact upload and packaging helpers |
//!
//! Module set shared with executor.md (`tools`, `verify`, `scip`, `media`, `recover`, `disk`):
//! append `pub mod` lines, never remove others'.

pub mod artifacts;
pub mod daemon;
pub mod pump;
pub mod session;
pub mod task;
pub mod media;
pub mod scip;
pub mod tools;
pub mod verify;
pub mod recover;
pub mod disk;
