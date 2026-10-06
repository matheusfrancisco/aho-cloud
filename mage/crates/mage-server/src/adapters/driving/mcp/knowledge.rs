//! Knowledge MCP server (knowledge.md "Knowledge MCP", server.md "Knowledge MCP"): an rmcp
//! `ServerHandler` exposing the 15 read-only tools of `app::knowledge_query::Service`.
//!
//! - The HTTP layer (`adapters::driving::http::mcp`) verifies the `mgk_` token and puts the
//!   `KnowledgeScope` into the request extensions; rmcp hands the HTTP request parts to the
//!   handler, which reads the scope from them. No scope → the call is refused.
//! - Tool input schemas derive from the input structs (unknown fields rejected); arguments that
//!   do not fit are JSON-RPC `-32602`. Tool-level failures are `isError: true` results carrying
//!   `{"error": {"code", "message", "layers"?}}`, so the agent can adapt.
//! - Per run: 20 calls/s burst, 3000 calls per run (in-memory per server instance); over →
//!   `rate_limited`. Each call has 120 s.
//! - One `tracing` span per call with run, tool, layer, degraded, items, bytes, latency.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::http::request::Parts;
use mage_core::domain::RunId;
use parking_lot::Mutex;
use rmcp::handler::server::common::schema_for_input;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, Implementation, ListToolsResult, PaginatedRequestParams, ServerCapabilities,
    ServerConfig, Tool, ToolAnnotations,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};
use serde_json::Value;

use crate::app::knowledge_query::inputs::{
    AppMapInput, BlameInput, CallGraphInput, ChangesBetweenInput, ExplainInput, FindSymbolInput, GlossaryInput, LookupErrorInput,
    ReadFileInput, ReferencesInput, RepoOverviewInput, RouteInfoInput, RunRecipeInput, SearchCodeInput, SimilarTicketsInput,
};
use crate::app::knowledge_query::{CallError, KNOWLEDGE_TOOLS, KnowledgeScope, McpLimits, Service, ToolError, ToolErrorCode};

/// Wall-clock limit of one tool call.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// Tool descriptions: what each tool answers and when to use it.
pub mod tools {
    pub const REPO_OVERVIEW: &str = "Start here. Languages, top directories, entry points (main functions, manifests, READMEs), frameworks, \
        fact counts, which knowledge layers are ready at this commit, and the repo's run recipe status.";
    pub const SEARCH_CODE: &str = "Search the repository at this commit. mode=text (literal, default) or regex for code text; \
        mode=symbol for fuzzy symbol names; mode=path for file paths. Use lookup_error instead for log lines and error messages, \
        and find_symbol when you know a name.";
    pub const READ_FILE: &str = "Read a file (up to 400 lines / 24 KiB per call, lines prefixed `{:>6}\\t`; follow next_cursor for more) \
        or list a directory (path \"\" is the root). Cite what you read as path:start-end@sha.";
    pub const FIND_SYMBOL: &str = "Find definitions by name: functions, methods, types, constants, re-frame registrations. Accepts \
        qualified names (pkg.Func, Type.Method, ns/fn, :ns/kw, Class::method). match=auto tries exact, case-insensitive, prefix, fuzzy.";
    pub const REFERENCES: &str = "Where a symbol is used. Precise (SCIP) when an index exists for this repo, else name-level. \
        Give `symbol` or `path` + `line` (+ `column`).";
    pub const CALL_GRAPH: &str = "Callers and/or callees of a symbol up to 3 hops, as nodes and edges with call-site citations.";
    pub const LOOKUP_ERROR: &str = "Use lookup_error first for any log line, stack trace or error message: finds the code that \
        produces it (throws, log calls, HTTP error responses, UI messages, i18n keys) with the enclosing function and routes.";
    pub const APP_MAP: &str = "The application map: pages, components, client calls, endpoints, handlers, tables and their links. \
        Without focus: a site map. With focus (route, page, component, symbol, table, path): its neighborhood.";
    pub const ROUTE_INFO: &str = "Given a URL or path (from a ticket, browser or log), find the endpoint or page that serves it, \
        its handler, who calls it, tables it touches and the errors it can return.";
    pub const EXPLAIN: &str = "A fact sheet about a symbol or file: definition, container, outline, caller/callee counts, routes, \
        error sites and recent commits. Facts only; no generated summary.";
    pub const GLOSSARY: &str = "Map product words (from a ticket) to code: matching type names, tables, endpoints, pages, events, \
        i18n keys and UI labels. Without a term: the repo's main domain terms.";
    pub const CHANGES_BETWEEN: &str = "What changed between an older commit (`from`) and this commit: files, commits with their PRs \
        and ticket ids, and optionally diff hunks. Use it to check regressions since the last good deploy.";
    pub const BLAME: &str = "Who last changed lines of a file (up to 200 lines): commit, author name, time, PR and ticket ids.";
    pub const SIMILAR_TICKETS: &str = "Resolved tickets similar to this one (or to `text`), with their root cause and the cited code \
        (marked whether it still exists at this commit).";
    pub const RUN_RECIPE: &str = "The repo's run recipe (how to boot the app and its services for repro), validated or draft.";
}

/// Tool table in [`KNOWLEDGE_TOOLS`] order.
pub fn tool_table() -> Result<Vec<Tool>, String> {
    let mut out = Vec::with_capacity(KNOWLEDGE_TOOLS.len());
    for name in KNOWLEDGE_TOOLS {
        let (description, schema) = match name {
            "repo_overview" => (tools::REPO_OVERVIEW, schema_for_input::<RepoOverviewInput>()?),
            "search_code" => (tools::SEARCH_CODE, schema_for_input::<SearchCodeInput>()?),
            "read_file" => (tools::READ_FILE, schema_for_input::<ReadFileInput>()?),
            "find_symbol" => (tools::FIND_SYMBOL, schema_for_input::<FindSymbolInput>()?),
            "references" => (tools::REFERENCES, schema_for_input::<ReferencesInput>()?),
            "call_graph" => (tools::CALL_GRAPH, schema_for_input::<CallGraphInput>()?),
            "lookup_error" => (tools::LOOKUP_ERROR, schema_for_input::<LookupErrorInput>()?),
            "app_map" => (tools::APP_MAP, schema_for_input::<AppMapInput>()?),
            "route_info" => (tools::ROUTE_INFO, schema_for_input::<RouteInfoInput>()?),
            "explain" => (tools::EXPLAIN, schema_for_input::<ExplainInput>()?),
            "glossary" => (tools::GLOSSARY, schema_for_input::<GlossaryInput>()?),
            "changes_between" => (tools::CHANGES_BETWEEN, schema_for_input::<ChangesBetweenInput>()?),
            "blame" => (tools::BLAME, schema_for_input::<BlameInput>()?),
            "similar_tickets" => (tools::SIMILAR_TICKETS, schema_for_input::<SimilarTicketsInput>()?),
            "run_recipe" => (tools::RUN_RECIPE, schema_for_input::<RunRecipeInput>()?),
            other => return Err(format!("no input type for tool {other}")),
        };
        let annotations = ToolAnnotations::new().read_only(true);
        out.push(Tool::new(name, description, schema).annotate(annotations));
    }
    Ok(out)
}

struct Bucket {
    tokens: f64,
    refilled: Instant,
    calls: u32,
}

/// Per-run call limits of this server instance: a token bucket of `calls_per_second` (burst and
/// refill) and a total of `calls_per_run`.
pub struct RunLimiter {
    limits: McpLimits,
    runs: Mutex<HashMap<RunId, Bucket>>,
}

/// Runs remembered before idle ones are dropped.
const LIMITER_PRUNE_AT: usize = 10_000;
const LIMITER_IDLE: Duration = Duration::from_secs(3600);

impl RunLimiter {
    pub fn new(limits: McpLimits) -> Self {
        RunLimiter { limits, runs: Mutex::new(HashMap::new()) }
    }

    /// Takes one call from the run's budget.
    pub fn admit(&self, run: RunId) -> Result<(), ToolError> {
        let now = Instant::now();
        let rate = f64::from(self.limits.calls_per_second.max(1));
        let mut runs = self.runs.lock();
        if runs.len() >= LIMITER_PRUNE_AT {
            runs.retain(|_, b| now.duration_since(b.refilled) < LIMITER_IDLE);
        }
        let bucket = runs.entry(run).or_insert(Bucket { tokens: rate, refilled: now, calls: 0 });
        bucket.tokens = (bucket.tokens + now.duration_since(bucket.refilled).as_secs_f64() * rate).min(rate);
        bucket.refilled = now;
        if bucket.calls >= self.limits.calls_per_run {
            return Err(ToolError::new(ToolErrorCode::RateLimited, format!("this run used its {} Knowledge MCP calls", self.limits.calls_per_run)));
        }
        if bucket.tokens < 1.0 {
            return Err(ToolError::new(ToolErrorCode::RateLimited, format!("more than {} calls per second; retry shortly", self.limits.calls_per_second)));
        }
        bucket.tokens -= 1.0;
        bucket.calls += 1;
        Ok(())
    }
}

/// The rmcp handler; one is built per request (stateless transport), sharing the service.
#[derive(Clone)]
pub struct KnowledgeMcp {
    service: Arc<Service>,
    limiter: Arc<RunLimiter>,
    tools: Arc<[Tool]>,
}

impl KnowledgeMcp {
    pub fn new(service: Arc<Service>, limiter: Arc<RunLimiter>, tools: Arc<[Tool]>) -> Self {
        KnowledgeMcp { service, limiter, tools }
    }

    fn scope(context: &RequestContext<RoleServer>) -> Option<KnowledgeScope> {
        context.extensions.get::<Parts>().and_then(|parts| parts.extensions.get::<KnowledgeScope>()).cloned()
    }
}

fn tool_error(e: &ToolError) -> CallToolResult {
    CallToolResult::structured_error(e.to_value())
}

impl ServerHandler for KnowledgeMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("mage-knowledge", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Read-only knowledge of one repository at one commit. Every answer names its sha and the layer that \
                 answered; cite code as path:start-end@sha exactly as returned. Start with repo_overview; use lookup_error \
                 for any error text, route_info for URLs, find_symbol/references/call_graph for code navigation, read_file to \
                 quote code.",
            )
    }

    async fn list_tools(&self, _request: Option<PaginatedRequestParams>, _context: RequestContext<RoleServer>) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(self.tools.to_vec()))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.tools.iter().find(|t| t.name == name).cloned()
    }

    async fn call_tool(&self, request: CallToolRequestParams, context: RequestContext<RoleServer>) -> Result<CallToolResponse, McpError> {
        let Some(scope) = Self::scope(&context) else {
            return Err(McpError::invalid_request("missing knowledge scope", None));
        };
        let tool = request.name.to_string();
        let span = tracing::info_span!(
            "knowledge_tool",
            run = %scope.run,
            tool = %tool,
            layer = tracing::field::Empty,
            degraded = tracing::field::Empty,
            items = tracing::field::Empty,
            bytes = tracing::field::Empty,
            latency_ms = tracing::field::Empty,
            error = tracing::field::Empty,
        );
        if let Err(e) = self.limiter.admit(scope.run) {
            span.record("error", e.code.as_str());
            return Ok(tool_error(&e).into());
        }
        let started = Instant::now();
        let args = request.arguments.map(Value::Object).unwrap_or(Value::Null);
        let outcome = tokio::time::timeout(CALL_TIMEOUT, self.service.call(&scope, &tool, args)).await;
        span.record("latency_ms", started.elapsed().as_millis() as u64);
        match outcome {
            Err(_) => {
                let e = ToolError::new(ToolErrorCode::Timeout, "the call took longer than 120 s");
                span.record("error", e.code.as_str());
                Ok(tool_error(&e).into())
            }
            Ok(Ok(out)) => {
                span.record("layer", out.layer.name());
                span.record("degraded", out.degraded);
                span.record("items", out.items as u64);
                span.record("bytes", out.bytes as u64);
                span.in_scope(|| tracing::info!("knowledge tool call"));
                Ok(CallToolResult::structured(out.value).into())
            }
            Ok(Err(CallError::Tool(e))) => {
                span.record("error", e.code.as_str());
                Ok(tool_error(&e).into())
            }
            Ok(Err(CallError::BadArguments(message))) => {
                span.record("error", "invalid_params");
                Err(McpError::invalid_params(message, None))
            }
            Ok(Err(CallError::UnknownTool(name))) => {
                span.record("error", "unknown_tool");
                Err(McpError::invalid_params(format!("unknown tool {name}"), None))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;

    #[test]
    fn tool_table_lists_every_tool_with_an_object_schema_rejecting_unknown_fields() {
        let tools = tool_table().unwrap_or_else(|e| panic!("{e}"));
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        assert_eq!(names, KNOWLEDGE_TOOLS.to_vec());
        for tool in &tools {
            assert_eq!(tool.input_schema.get("type").and_then(Value::as_str), Some("object"), "{}", tool.name);
            assert_eq!(tool.input_schema.get("additionalProperties"), Some(&Value::Bool(false)), "{}", tool.name);
            assert!(tool.description.as_deref().is_some_and(|d| !d.is_empty()));
        }
        let search = &tools[1].input_schema;
        assert_eq!(search["properties"]["limit"]["maximum"], 50);
        assert_eq!(search["required"], serde_json::json!(["query"]));
    }

    #[test]
    fn limiter_enforces_burst_and_run_total() {
        let run = RunId::from_uuid(Uuid::from_u128(1));
        let burst = RunLimiter::new(McpLimits { calls_per_second: 2, calls_per_run: 100 });
        assert!(burst.admit(run).is_ok() && burst.admit(run).is_ok());
        assert_eq!(burst.admit(run).map_err(|e| e.code), Err(ToolErrorCode::RateLimited), "third call within the second");
        let other = RunId::from_uuid(Uuid::from_u128(2));
        assert!(burst.admit(other).is_ok(), "runs have separate buckets");

        let total = RunLimiter::new(McpLimits { calls_per_second: 1000, calls_per_run: 3 });
        for _ in 0..3 {
            assert!(total.admit(run).is_ok());
        }
        assert_eq!(total.admit(run).map_err(|e| e.code), Err(ToolErrorCode::RateLimited));
    }
}
