//! `Service`: the Knowledge MCP tools (knowledge.md "Tools"). The MCP adapter hands it the
//! caller's [`KnowledgeScope`], a tool name and the raw arguments; it returns the envelope JSON.

use std::sync::Arc;

use mage_core::domain::{CommitSha, Layer, RepoId, RepoPath, RepoVersion, RepoVersionKey};
use mage_core::ports::knowledge::{KnowledgeStore, SymbolHit};
use mage_core::ports::repos::{RepoStore, RepoStoreError};
use mage_core::wire::mcp::KNOWLEDGE_TOOLS;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::envelope::{Envelope, ListField, ToolError, ToolErrorCode, citation, describe_state, layer_states};
use super::scope::KnowledgeScope;
use super::source::SourceReader;
use crate::domain::knowledge::{AnswerSource, layer_name};
use crate::ports::indexer_link::IndexerLink;
use crate::ports::runs::RunStore;
use crate::ports::ticket::TicketStore;

pub struct ServiceDeps {
    pub knowledge: Arc<dyn KnowledgeStore>,
    pub repos: Arc<dyn RepoStore>,
    pub tickets: Arc<dyn TicketStore>,
    pub runs: Arc<dyn RunStore>,
    pub indexer: Arc<dyn IndexerLink>,
    /// The same reader the triage evidence gate uses.
    pub source: SourceReader,
}

pub struct Service {
    pub(crate) knowledge: Arc<dyn KnowledgeStore>,
    pub(crate) repos: Arc<dyn RepoStore>,
    pub(crate) tickets: Arc<dyn TicketStore>,
    pub(crate) runs: Arc<dyn RunStore>,
    pub(crate) indexer: Arc<dyn IndexerLink>,
    pub(crate) source: SourceReader,
}

/// Why a call produced no tool result.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum CallError {
    /// Not one of [`KNOWLEDGE_TOOLS`] (JSON-RPC method-level error).
    #[error("unknown tool {0}")]
    UnknownTool(String),
    /// Arguments do not fit the input schema (JSON-RPC `-32602`).
    #[error("invalid arguments: {0}")]
    BadArguments(String),
    /// A tool-level failure (`isError: true` with the error object).
    #[error(transparent)]
    Tool(#[from] ToolError),
}

/// A successful tool result plus what the adapter logs and counts.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutput {
    pub value: Value,
    pub layer: AnswerSource,
    pub degraded: bool,
    pub items: usize,
    pub bytes: usize,
}

impl Service {
    pub fn new(deps: ServiceDeps) -> Self {
        Service {
            knowledge: deps.knowledge,
            repos: deps.repos,
            tickets: deps.tickets,
            runs: deps.runs,
            indexer: deps.indexer,
            source: deps.source,
        }
    }

    pub async fn call(&self, scope: &KnowledgeScope, tool: &str, args: Value) -> Result<ToolOutput, CallError> {
        let args = if args.is_null() { Value::Object(Default::default()) } else { args };
        let mut env = match tool {
            "repo_overview" => self.repo_overview(scope, parse(args)?).await?,
            "search_code" => self.search_code(scope, parse(args)?).await?,
            "read_file" => self.read_file(scope, parse(args)?).await?,
            "find_symbol" => self.find_symbol(scope, parse(args)?).await?,
            "references" => self.references(scope, parse(args)?).await?,
            "call_graph" => self.call_graph(scope, parse(args)?).await?,
            "lookup_error" => self.lookup_error(scope, parse(args)?).await?,
            "app_map" => self.app_map(scope, parse(args)?).await?,
            "route_info" => self.route_info(scope, parse(args)?).await?,
            "explain" => self.explain(scope, parse(args)?).await?,
            "glossary" => self.glossary(scope, parse(args)?).await?,
            "changes_between" => self.changes_between(scope, parse(args)?).await?,
            "blame" => self.blame(scope, parse(args)?).await?,
            "similar_tickets" => self.similar_tickets(scope, parse(args)?).await?,
            "run_recipe" => self.run_recipe(scope, parse(args)?).await?,
            other => {
                debug_assert!(!KNOWLEDGE_TOOLS.contains(&other), "every listed tool is dispatched");
                return Err(CallError::UnknownTool(other.to_owned()));
            }
        };
        // Tools that page set their own cursor; anything still too large is cut here.
        env.fit(ListField::Items);
        let value = env.to_value();
        let bytes = serde_json::to_vec(&value).map_or(0, |v| v.len());
        Ok(ToolOutput { layer: env.layer, degraded: env.degraded.is_some(), items: env.items.len(), bytes, value })
    }

    /// The version at the scope sha with `Files` Ready (every tool except `similar_tickets` and
    /// `run_recipe`); otherwise `version_not_ready` carrying the layer states.
    pub(crate) async fn ctx<'a>(&self, scope: &'a KnowledgeScope) -> Result<Ctx<'a>, ToolError> {
        let version = match self.repos.version(scope.org, scope.version()).await {
            Ok(version) => version,
            Err(RepoStoreError::NotFound) => {
                return Err(ToolError::new(ToolErrorCode::VersionNotReady, format!("{} is not indexed yet", scope.version())));
            }
            Err(e) => return Err(e.into()),
        };
        let ctx = Ctx { scope, version };
        if !ctx.ready(Layer::Files) {
            return Err(ToolError::new(
                ToolErrorCode::VersionNotReady,
                format!("layer Files is {} at {}; retry later", ctx.state_of(Layer::Files), scope.version()),
            )
            .with_layers(ctx.layer_states()));
        }
        Ok(ctx)
    }
}

fn parse<T: DeserializeOwned>(args: Value) -> Result<T, CallError> {
    serde_json::from_value(args).map_err(|e| CallError::BadArguments(e.to_string()))
}

/// The `repo` / `sha` arguments every tool accepts must name the token's version.
pub(crate) fn check_scope(scope: &KnowledgeScope, repo: Option<&str>, sha: Option<&str>) -> Result<(), ToolError> {
    if let Some(repo) = repo {
        let parsed: RepoId = repo.trim().parse().map_err(|_| ToolError::invalid(format!("repo {repo:?} is not a repo id")))?;
        if parsed != scope.repo {
            return Err(ToolError::out_of_scope("this token only reads its own repo"));
        }
    }
    if let Some(sha) = sha {
        let parsed: CommitSha = parse_sha(sha)?;
        if parsed != scope.sha {
            return Err(ToolError::out_of_scope("this token only reads its own sha"));
        }
    }
    Ok(())
}

pub(crate) fn parse_sha(sha: &str) -> Result<CommitSha, ToolError> {
    sha.trim().to_ascii_lowercase().parse().map_err(|_| ToolError::invalid(format!("{sha:?} is not a 40-hex commit sha")))
}

/// One call's view of the scope version.
pub(crate) struct Ctx<'a> {
    pub scope: &'a KnowledgeScope,
    pub version: RepoVersion,
}

impl Ctx<'_> {
    pub fn key(&self) -> RepoVersionKey {
        self.scope.version()
    }

    pub fn org(&self) -> mage_core::domain::OrgId {
        self.scope.org
    }

    /// Ready at this version and not disabled for this token.
    pub fn ready(&self, layer: Layer) -> bool {
        !self.scope.layer_disabled(layer) && self.version.layer(layer).is_some_and(|s| s.is_ready())
    }

    /// `Disabled`, or the layer state (`Pending`, `Skipped: …`).
    pub fn state_of(&self, layer: Layer) -> String {
        if self.scope.layer_disabled(layer) {
            return "Disabled".into();
        }
        self.version.layer(layer).map_or_else(|| "Pending".into(), describe_state)
    }

    /// The `degraded.reason` of a fallback taken because `layer` is not Ready.
    pub fn not_ready_reason(&self, layer: Layer) -> String {
        if self.scope.layer_disabled(layer) {
            "layer_disabled".into()
        } else {
            format!("layer {} is {}", layer_name(layer), self.state_of(layer))
        }
    }

    pub fn layer_states(&self) -> serde_json::Map<String, Value> {
        layer_states(&self.version, &self.scope.disabled_layers)
    }

    /// `layer_not_ready` with the state (skip reason included) and a hint.
    pub fn require(&self, layer: Layer, hint: &str) -> Result<(), ToolError> {
        if self.ready(layer) {
            return Ok(());
        }
        let mut message = format!("layer {} is {} at this sha", layer_name(layer), self.state_of(layer));
        if !hint.is_empty() {
            message.push_str("; ");
            message.push_str(hint);
        }
        Err(ToolError::new(ToolErrorCode::LayerNotReady, message).with_layers(self.layer_states()))
    }

    pub fn envelope(&self, layer: impl Into<AnswerSource>) -> Envelope {
        Envelope::new(self.scope.repo, self.scope.sha, layer)
    }

    pub fn cite(&self, path: &RepoPath, start: u32, end: u32) -> Value {
        citation(path, start, end, self.scope.sha)
    }

    pub fn cite_symbol(&self, hit: &SymbolHit) -> Value {
        self.cite(&hit.at.path, hit.def.span.start(), hit.def.span.end())
    }
}
