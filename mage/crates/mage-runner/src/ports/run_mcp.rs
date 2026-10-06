//! `RunMcpHost` (executor.md "`RunMcpHost`", Additions): the session use case starts the per-run
//! Run MCP server without importing `crate::adapters`. The adapter
//! (`adapters::driving::mcp`) only maps MCP calls onto [`RunTools`].

use std::sync::Arc;

use async_trait::async_trait;

use crate::app::runner::tools::RunTools;
use crate::error::AppError;

#[async_trait]
pub trait RunMcpHost: Send + Sync {
    async fn start(&self, tools: Arc<RunTools>) -> Result<Box<dyn RunMcpHandle>, RunMcpError>;
}

#[async_trait]
pub trait RunMcpHandle: Send + Sync {
    fn endpoint(&self) -> &RunMcpEndpoint;
    /// Graceful: stop accepting, cancel in-flight calls (which kills their execs). Idempotent.
    async fn shutdown(&self);
}

#[derive(Clone, PartialEq, Eq)]
pub struct RunMcpEndpoint {
    /// `http://127.0.0.1:<port>/mcp`.
    pub url: String,
    /// 32 random bytes, base64url; sent as `Authorization: Bearer <bearer>`.
    pub bearer: String,
}

impl std::fmt::Debug for RunMcpEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunMcpEndpoint").field("url", &self.url).field("bearer", &"<redacted>").finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RunMcpError {
    #[error("not found")]
    NotFound,
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("invalid: {0}")]
    Invalid(String),
    /// No port could be leased or bound, or the server failed to start.
    #[error("unavailable: {0}")]
    Unavailable(String),
}

impl From<RunMcpError> for AppError {
    fn from(err: RunMcpError) -> Self {
        match err {
            RunMcpError::NotFound => AppError::NotFound("run mcp".into()),
            RunMcpError::Conflict(m) => AppError::Conflict(m),
            RunMcpError::Invalid(m) => AppError::Invalid(m),
            RunMcpError::Unavailable(m) => AppError::Unavailable(m),
        }
    }
}
