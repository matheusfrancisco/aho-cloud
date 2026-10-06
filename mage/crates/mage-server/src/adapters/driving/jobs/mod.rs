//! Job handlers (`JobHandler` impls, `crate::adapters::driving::worker`), one module per area.
//! Append `pub mod` lines, never remove others'.
pub mod platform;
pub mod triage;
pub mod intake;
pub mod runs;
pub mod fix;
