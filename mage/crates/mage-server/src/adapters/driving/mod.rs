//! Driving adapters (inbound). One module per adapter; append `pub mod` lines, never remove others'.
pub mod http;
pub mod jobs;
pub mod worker;
pub mod mcp;
