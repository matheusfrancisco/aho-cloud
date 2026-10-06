//! Driven adapters (port implementations). One module per adapter; append `pub mod` lines, never remove others'.

pub mod exec_host;
pub mod runner_state;
pub mod playwright;
pub mod link;
pub mod harness_common;
pub mod harness_omp;
pub mod harness_claude;
