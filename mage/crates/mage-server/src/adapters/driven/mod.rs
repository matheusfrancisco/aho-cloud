//! Driven adapters (port implementations). One module per adapter; append `pub mod` lines, never remove others'.
pub mod github;
pub mod linear;
pub mod slack;
pub mod identity;
pub mod signer;
pub mod vault;
pub mod indexer_link;
pub mod pg;
pub mod llm_upstream;
pub mod secretscan;
