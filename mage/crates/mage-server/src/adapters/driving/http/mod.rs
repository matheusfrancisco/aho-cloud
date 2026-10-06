//! HTTP driving adapter (server.md "HTTP surface", "Request pipeline", "Adapter internals").
//! The platform shell lives in [`shell`] (router merge, middleware); each area adds its own
//! `pub fn router(state) -> axum::Router` module here. Append `pub mod` lines, never remove others'.

pub use shell::{AreaRouters, PlatformHttp, TraceHost, router};

pub mod api_orgs;
pub mod assets;
pub mod auth;
pub mod error;
pub mod feed_sse;
pub mod health;
pub mod shell;
pub mod api_repos;
pub mod hooks;
pub mod llm;
pub mod api_triage;
pub mod api_tickets;
pub mod mcp;
#[cfg(test)]
mod platform_tests;
pub mod runner;
pub mod api_runs;
pub mod run_sse;
pub mod api_fix;
