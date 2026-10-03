//! Entry A session host: runs `ahoc` per workspace and bridges aho rpc (NDJSON over stdio) to a
//! browser WebSocket.
//!
//! 1. The control plane calls `POST /internal/v1/workspaces` (service token) with a
//!    [`cloud_api::internal::StartWorkspace`]. The host creates or resumes the workspace directory
//!    for `(project, user)` and answers [`cloud_api::internal::WorkspaceStarted`] with a
//!    short-lived [`token`]. `session_id` is `None` until the workspace's first `hello_ok`.
//! 2. The browser opens `GET /rpc/{workspace}?token=…` as a WebSocket. After its first frame (which
//!    must be `hello`) the host spawns the configured command through a [`Spawner`] and pumps one
//!    text message per NDJSON line in each direction ([`bridge`]).
//! 3. The session id from `hello_ok` is persisted, so the next connection passes `--session`.
//! 4. For the console's code view and "Deploy to preview" the control plane (service token)
//!    reads the workspace without starting it: `WORKSPACE_FILES` (the tree a deploy ships),
//!    `WORKSPACE_FILE` (one file) and `WORKSPACE_PACKAGE` (archive, verified session log,
//!    `deploy.json` and draft), all `cloud-workdir` on the project directory. A tampered session
//!    log refuses the package with `422 invalid_session`, so a deploy never loses its
//!    attestation silently.
//! 5. Sandboxes (only with a runtime agent configured; otherwise these routes answer 503
//!    `sandboxes_disabled`). The host reaches the runtime agent's exec socket with its service
//!    token ([`runtime`]):
//!    * `SANDBOX_TERMINALS` mints a single-use terminal token bound to one exec request
//!      (lifetime at most [`terminal::TERMINAL_TOKEN_TTL`]); `GET /terminal/{sandbox}?token=`
//!      relays binary exec frames between the browser and that exec session ([`terminal`]).
//!    * `SANDBOX_CHATS` is `WORKSPACES` for the aho inside a sandbox: `GET
//!      /sandbox-rpc/{chat}?token=` is the same rpc bridge, its child an exec session running
//!      `aho --project /workspace rpc [--session <id>]`, the session id remembered per
//!      `(sandbox, user)`.

#![forbid(unsafe_code)]

pub mod bridge;
mod content;
pub mod limits;
pub mod runtime;
pub mod server;
pub mod spawn;
pub mod terminal;
pub mod token;
pub mod workspace;

pub use limits::{Denied, Limits};
pub use server::{Config, ConfigError, RPC_ROUTE, SANDBOX_RPC_ROUTE, SandboxConfig, SessionHost, TERMINAL_ROUTE};
pub use spawn::{CommandTemplate, Process, SpawnSpec, Spawned, Spawner, TokioSpawner};
pub use token::{Claims, TerminalClaims, TokenError, TokenKey};
