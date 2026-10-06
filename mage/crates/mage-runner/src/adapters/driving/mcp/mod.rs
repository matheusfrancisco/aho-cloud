//! Run MCP server (executor.md "Run MCP"): one rmcp streamable-HTTP server per run on
//! `127.0.0.1:<leased port>/mcp`, implementing `RunMcpHost`. It only maps MCP onto
//! [`RunTools`]: `tools/list` → `RunTools::tool_defs`, `tools/call` → `RunTools::call` (the
//! request's cancellation token cancels the call, which kills its exec).
//!
//! In front of rmcp, a guard rejects: a missing or wrong bearer (401, constant-time compare), any
//! `Origin` header (403; no browser clients, DNS-rebinding guard), and a `Host` other than
//! `127.0.0.1:<port>` (403).

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use mage_core::ports::Clock;
use rand::RngCore;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation, InitializeResult, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde::Deserialize;
use serde_json::Value;
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::app::runner::tools::{RunTools, UnknownTool};
use crate::ports::run_mcp::{RunMcpEndpoint, RunMcpError, RunMcpHandle, RunMcpHost};
use crate::ports::runner_state::{PortOwner, PortRange, RunnerState};

/// Path of the MCP endpoint.
pub const MCP_PATH: &str = "/mcp";
/// How long a port that failed its bind test stays out of the pool.
const QUARANTINE: time::Duration = time::Duration::minutes(10);
/// Lease → bind attempts before giving up.
const BIND_ATTEMPTS: usize = 8;
/// Wait for the server task after cancelling it.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Run MCP listener settings (`[runner.ports].range` is the pool).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RunMcpConfig {
    /// Inclusive port pool `[start, end]` shared with env ports.
    pub port_range: [u16; 2],
}

impl Default for RunMcpConfig {
    fn default() -> Self {
        Self { port_range: [20000, 29999] }
    }
}

/// `RunMcpHost` on rmcp + axum.
pub struct RunMcpServer {
    state: Arc<dyn RunnerState>,
    clock: Arc<dyn Clock>,
    config: RunMcpConfig,
}

impl RunMcpServer {
    pub fn new(state: Arc<dyn RunnerState>, clock: Arc<dyn Clock>, config: RunMcpConfig) -> Self {
        Self { state, clock, config }
    }

    /// Leases a pool port for `owner` and binds it; a port that fails the bind goes to quarantine
    /// and another is leased.
    async fn bind(&self, owner: &PortOwner) -> Result<TcpListener, RunMcpError> {
        let range = PortRange { start: self.config.port_range[0], end: self.config.port_range[1] };
        for _ in 0..BIND_ATTEMPTS {
            let leases = self.state.lease_ports(owner, &[None], range).await.map_err(|e| RunMcpError::Unavailable(format!("port lease: {e:?}")))?;
            let Some(lease) = leases.first() else { return Err(RunMcpError::Unavailable("no port leased".into())) };
            match TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, lease.port))).await {
                Ok(listener) => return Ok(listener),
                Err(e) => {
                    tracing::warn!(port = lease.port, error = %e, "run mcp port bind failed; quarantining");
                    self.state
                        .quarantine_port(lease.port, self.clock.now() + QUARANTINE)
                        .await
                        .map_err(|e| RunMcpError::Unavailable(format!("port quarantine: {e:?}")))?;
                }
            }
        }
        let _ = self.state.release_ports(owner).await;
        Err(RunMcpError::Unavailable(format!("no bindable port after {BIND_ATTEMPTS} leases")))
    }
}

/// 32 bytes from the OS RNG, base64url without padding.
fn new_bearer() -> String {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

#[async_trait]
impl RunMcpHost for RunMcpServer {
    async fn start(&self, tools: Arc<RunTools>) -> Result<Box<dyn RunMcpHandle>, RunMcpError> {
        let owner = PortOwner::RunMcp(tools.run_id());
        let listener = self.bind(&owner).await?;
        let port = listener.local_addr().map_err(|e| RunMcpError::Unavailable(e.to_string()))?.port();
        let bearer = new_bearer();
        let cancel = CancellationToken::new();
        let router = router(tools.clone(), &bearer, port, cancel.child_token());
        let task = tokio::spawn({
            let cancel = cancel.clone();
            async move {
                let served = axum::serve(listener, router).with_graceful_shutdown(cancel.cancelled_owned()).await;
                if let Err(e) = served {
                    tracing::warn!(error = %e, "run mcp server stopped with an error");
                }
            }
        });
        Ok(Box::new(RunMcpServerHandle {
            endpoint: RunMcpEndpoint { url: format!("http://127.0.0.1:{port}{MCP_PATH}"), bearer },
            cancel,
            tools,
            task: parking_lot::Mutex::new(Some(task)),
            state: self.state.clone(),
            owner,
        }))
    }
}

/// The axum router: guard + rmcp service at [`MCP_PATH`].
pub fn router(tools: Arc<RunTools>, bearer: &str, port: u16, cancel: CancellationToken) -> axum::Router {
    let config = StreamableHttpServerConfig::default()
        .with_allowed_hosts(["127.0.0.1"])
        .enforce_origin_validation()
        .with_sse_keep_alive(Some(Duration::from_secs(15)))
        .with_cancellation_token(cancel);
    let handler = RunMcpHandler { tools };
    let service = StreamableHttpService::new(move || Ok(handler.clone()), Arc::new(LocalSessionManager::default()), config);
    let guard = Arc::new(Guard { bearer: format!("Bearer {bearer}").into_bytes(), host: format!("127.0.0.1:{port}") });
    axum::Router::new().route_service(MCP_PATH, service).layer(axum::middleware::from_fn_with_state(guard, guard_request))
}

struct Guard {
    /// The full expected `Authorization` value.
    bearer: Vec<u8>,
    host: String,
}

async fn guard_request(State(guard): State<Arc<Guard>>, request: Request, next: Next) -> Response {
    let headers = request.headers();
    if headers.contains_key(header::ORIGIN) {
        return (StatusCode::FORBIDDEN, "browser requests are not accepted").into_response();
    }
    let host_ok = headers.get(header::HOST).and_then(|h| h.to_str().ok()).is_some_and(|h| h == guard.host);
    if !host_ok {
        return (StatusCode::FORBIDDEN, "unexpected Host").into_response();
    }
    let presented = headers.get(header::AUTHORIZATION).map(|v| v.as_bytes()).unwrap_or_default();
    // Constant time in the value; the length is not secret (fixed by construction).
    let ok = presented.len() == guard.bearer.len() && bool::from(presented.ct_eq(&guard.bearer));
    if !ok {
        return (StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE, "Bearer")], "missing or wrong bearer").into_response();
    }
    next.run(request).await
}

/// rmcp handler: one per session, all sharing the run's `RunTools`.
#[derive(Clone)]
struct RunMcpHandler {
    tools: Arc<RunTools>,
}

fn tool_of(def: crate::app::runner::tools::ToolDef) -> Tool {
    Tool::new(def.name, def.description, def.input_schema)
}

impl ServerHandler for RunMcpHandler {
    fn get_info(&self) -> InitializeResult {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("mage_run", env!("CARGO_PKG_VERSION")))
            .with_instructions("Tools that act on this run's disposable environment, and the run's result tool.")
    }

    async fn list_tools(&self, _request: Option<PaginatedRequestParams>, _context: RequestContext<RoleServer>) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(self.tools.tool_defs().into_iter().map(tool_of).collect()))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.tools.tool_defs().into_iter().find(|d| d.name == name).map(tool_of)
    }

    async fn call_tool(&self, request: CallToolRequestParams, context: RequestContext<RoleServer>) -> Result<CallToolResponse, ErrorData> {
        let args = request.arguments.map(Value::Object).unwrap_or(Value::Null);
        match self.tools.call(&request.name, args, context.ct.clone()).await {
            Ok(response) if response.ok => {
                let mut result = CallToolResult::success(vec![ContentBlock::text(response.text)]);
                if response.value.is_object() {
                    result.structured_content = Some(response.value);
                }
                Ok(result.into())
            }
            Ok(response) => Ok(CallToolResult::error(vec![ContentBlock::text(response.text)]).into()),
            Err(UnknownTool(name)) => Err(ErrorData::invalid_params(format!("unknown tool: {name}"), None)),
        }
    }
}

struct RunMcpServerHandle {
    endpoint: RunMcpEndpoint,
    cancel: CancellationToken,
    tools: Arc<RunTools>,
    task: parking_lot::Mutex<Option<JoinHandle<()>>>,
    state: Arc<dyn RunnerState>,
    owner: PortOwner,
}

#[async_trait]
impl RunMcpHandle for RunMcpServerHandle {
    fn endpoint(&self) -> &RunMcpEndpoint {
        &self.endpoint
    }

    async fn shutdown(&self) {
        self.tools.shutdown();
        self.cancel.cancel();
        let task = self.task.lock().take();
        if let Some(mut task) = task
            && tokio::time::timeout(SHUTDOWN_GRACE, &mut task).await.is_err()
        {
            task.abort();
        }
        if let Err(e) = self.state.release_ports(&self.owner).await {
            tracing::warn!(error = ?e, "releasing the run mcp port failed; the reaper releases it");
        }
    }
}

#[cfg(test)]
mod tests;
