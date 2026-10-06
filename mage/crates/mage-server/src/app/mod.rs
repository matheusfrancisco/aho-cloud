//! Use cases, one module per part. Never imports `crate::adapters`. Append `pub mod` lines, never remove others'.
pub mod intake;
pub mod auth;
pub mod orgs;
pub mod settings;
pub mod knowledge_query;
pub mod repos;
pub mod gateway;
pub mod fix;
pub mod triage;
pub mod runs;
