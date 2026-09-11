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

#![forbid(unsafe_code)]

pub mod bridge;
pub mod limits;
pub mod server;
pub mod spawn;
pub mod token;
pub mod workspace;

pub use limits::{Denied, Limits};
pub use server::{Config, ConfigError, RPC_ROUTE, SessionHost};
pub use spawn::{CommandTemplate, Process, SpawnSpec, Spawned, Spawner, TokioSpawner};
pub use token::{Claims, TokenError, TokenKey};
