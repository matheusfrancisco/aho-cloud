//! Knowledge query side (knowledge.md "Query side"; README module `knowledge_query`): the
//! Knowledge MCP tools ([`Service`]), the token → [`KnowledgeScope`] check
//! ([`AuthorizeKnowledge`]), the shared [`SourceReader`], the triage [`Retriever`] and the
//! retrieval eval ([`eval`]).
//!
//! Every answer is pinned to the token's sha, cites `path:start-end@sha`, and names the layer
//! that answered. A layer is read only when it is Ready at that sha and not disabled for the
//! token; otherwise the tool falls back as knowledge.md specifies (`degraded`) or reports
//! `layer_not_ready`. Layer 5 (Semantic) is not built in v1: `explain` returns the deterministic
//! fact sheet, labeled degraded, and the retriever's semantic stage reports that it did not run.

pub mod envelope;
pub mod errors;
pub mod eval;
pub mod inputs;
pub mod retrieval;
mod scope;
mod service;
mod source;
mod text;
mod tools;

#[cfg(test)]
pub(crate) mod tests;

use std::sync::Arc;
use std::time::Duration;

use mage_core::domain::{OrgId, RepoId};
use mage_core::wire::token::TokenScope;
use serde::Deserialize;
use url::Url;

use crate::app::auth::tokens::TokenService;
use crate::ports::indexer_link::IndexerLinkError;

pub use self::envelope::{Envelope, ToolError, ToolErrorCode};
pub use self::retrieval::{RetrievalLimits, RetrievalWeights, Retriever, RetrieverDeps};
pub use self::scope::{AuthorizeKnowledge, KnowledgeScope};
pub use self::service::{CallError, Service, ServiceDeps, ToolOutput};
pub use self::source::{SourceLines, SourceReadError, SourceReader, split_lines};
pub use crate::domain::knowledge::{
    AnswerSource, Candidate, CandidateSet, Signal, SimilarMatch, SimilarResolved, Stage, StageReport, SymbolSummary, TicketContext,
};
pub use mage_core::wire::mcp::KNOWLEDGE_TOOLS;
pub use mage_core::wire::token::MemoryCutoff;

/// `[server.knowledge]`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeConfig {
    /// Origin of the indexer's internal API (`http://10.x.x.x:8081`).
    pub indexer_url: Url,
    /// Per-call timeout of `IndexerLink` (knowledge.md: 6 s).
    #[serde(default = "default_indexer_timeout_ms")]
    pub indexer_timeout_ms: u64,
    #[serde(default)]
    pub retrieval: RetrievalWeights,
    #[serde(default)]
    pub mcp: McpLimits,
}

fn default_indexer_timeout_ms() -> u64 {
    6000
}

impl KnowledgeConfig {
    pub fn indexer_timeout(&self) -> Duration {
        Duration::from_millis(self.indexer_timeout_ms)
    }
}

/// Knowledge MCP limits per run (knowledge.md "Knowledge MCP").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpLimits {
    /// Token-bucket burst and refill per second.
    pub calls_per_second: u32,
    pub calls_per_run: u32,
}

impl Default for McpLimits {
    fn default() -> Self {
        McpLimits { calls_per_second: 20, calls_per_run: 3000 }
    }
}

/// Lifetime of an `mgi_` token (server.md: ≤ 60 s).
pub const INDEXER_TOKEN_TTL: time::Duration = time::Duration::seconds(60);

/// The per-call `mgi_` minter the HTTP `IndexerLink` takes: claims `{org, repo}`, fresh `jti`,
/// 60 s lifetime, signed by the platform `TokenService`.
pub fn indexer_token_minter(tokens: Arc<TokenService>) -> Arc<dyn Fn(OrgId, RepoId) -> Result<String, IndexerLinkError> + Send + Sync> {
    Arc::new(move |org, repo| {
        tokens
            .mint(TokenScope::Indexer { org, repo }, INDEXER_TOKEN_TTL)
            .map(|(token, _)| token.into_string())
            .map_err(|e| IndexerLinkError::Unavailable(format!("cannot sign the indexer token: {e}")))
    })
}
