//! `DevHost`: a real `exec_host` executor in dev mode over a caller-provided data dir, with a
//! loopback HTTP server that serves SourceBundles, for adapter and opt-in integration tests.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use mage_core::domain::recipe::RecipeSpec;
use mage_core::domain::{CommitSha, OrgId, RepoId};
use mage_core::ports::{SystemClock, UuidV7Gen};
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::adapters::driven::exec_host::{ExecHostConfig, HostDeps, HostExecutor};
use crate::adapters::driven::runner_state::SqliteRunnerState;
use crate::ports::executor::{BaseRef, BundleSource, ExecError, Executor, NullObserver, PrepareRequest};

/// A zstd-compressed tar of `files` (paths relative; `*.sh` files are executable).
pub fn bundle_bytes(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (path, bytes) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(if path.ends_with(".sh") { 0o755 } else { 0o644 });
        header.set_mtime(1_700_000_000);
        header.set_entry_type(tar::EntryType::Regular);
        if builder.append_data(&mut header, path, *bytes).is_err() {
            return Vec::new();
        }
    }
    let tar = builder.into_inner().unwrap_or_default();
    zstd::encode_all(tar.as_slice(), 3).unwrap_or_default()
}

/// Loopback HTTP server answering `GET /b/<sha256 hex>`.
struct BundleServer {
    addr: SocketAddr,
    blobs: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    task: tokio::task::JoinHandle<()>,
}

impl BundleServer {
    async fn start() -> Result<Self, ExecError> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let blobs: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::default();
        let served = Arc::clone(&blobs);
        let task = tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let blobs = Arc::clone(&served);
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !head.windows(4).any(|w| w == b"\r\n\r\n") && head.len() < 8192 {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => head.extend_from_slice(&buf[..n]),
                        }
                    }
                    let text = String::from_utf8_lossy(&head);
                    let path = text.split_whitespace().nth(1).unwrap_or("");
                    let body = path.strip_prefix("/b/").and_then(|k| blobs.lock().get(k).cloned());
                    let response = match body {
                        Some(body) => {
                            let mut r = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/zstd\r\nConnection: close\r\n\r\n", body.len()).into_bytes();
                            r.extend_from_slice(&body);
                            r
                        }
                        None => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
                    };
                    let _ = sock.write_all(&response).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        Ok(Self { addr, blobs, task })
    }

    fn publish(&self, bytes: Vec<u8>) -> BundleSource {
        let sha: [u8; 32] = Sha256::digest(&bytes).into();
        let hex = hex::encode(sha);
        let len = bytes.len() as u64;
        self.blobs.lock().insert(hex.clone(), bytes);
        BundleSource { url: format!("http://{}/b/{hex}", self.addr), sha256: sha, bytes: len }
    }
}

impl Drop for BundleServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub struct DevHost {
    pub executor: Arc<HostExecutor>,
    pub state: Arc<SqliteRunnerState>,
    pub org_id: OrgId,
    pub repo_id: RepoId,
    server: BundleServer,
}

impl DevHost {
    /// Dev profile over `data_dir` without a disk floor and without a runner Postgres.
    pub async fn start(data_dir: &Path) -> Result<Self, ExecError> {
        Self::start_with(Self::config(data_dir, None)).await
    }

    /// Like [`DevHost::start`] with `pg_url` as the runner Postgres. A boot reap's orphan scan
    /// drops every `mf_*`/`mb_*` database it does not know, so tests sharing one Postgres must not
    /// run `recover` on a host started this way.
    pub async fn start_with_pg(data_dir: &Path, pg_url: &str) -> Result<Self, ExecError> {
        Self::start_with(Self::config(data_dir, Some(pg_url.to_owned()))).await
    }

    fn config(data_dir: &Path, pg_url: Option<String>) -> ExecHostConfig {
        let mut cfg = ExecHostConfig::dev(data_dir.to_path_buf());
        cfg.postgres.url = pg_url;
        cfg.disk.min_free_gib = 0;
        cfg.disk.first_base_estimate_gib = 0;
        cfg
    }

    pub async fn start_with(cfg: ExecHostConfig) -> Result<Self, ExecError> {
        std::fs::create_dir_all(&cfg.data_dir)?;
        let state = Arc::new(SqliteRunnerState::open(&cfg.state_db_path(), Arc::new(SystemClock)).map_err(ExecError::from)?);
        let deps = HostDeps { state: state.clone(), clock: Arc::new(SystemClock), ids: Arc::new(UuidV7Gen) };
        let executor = HostExecutor::open(cfg, deps).await?;
        Ok(Self {
            executor,
            state,
            org_id: OrgId::from_uuid(uuid::Uuid::now_v7()),
            repo_id: RepoId::from_uuid(uuid::Uuid::now_v7()),
            server: BundleServer::start().await?,
        })
    }

    /// Serves a bundle of `files` and returns its signed-URL source.
    pub fn publish(&self, files: &[(&str, &[u8])]) -> BundleSource {
        self.server.publish(bundle_bytes(files))
    }

    /// Prepares a base from `files` (sha derived from the bundle digest).
    pub async fn prepare_files(&self, files: &[(&str, &[u8])], recipe: Option<RecipeSpec>) -> Result<BaseRef, ExecError> {
        let source = self.publish(files);
        let mut sha = [0u8; 20];
        sha.copy_from_slice(&source.sha256[..20]);
        let req = PrepareRequest { org_id: self.org_id, repo_id: self.repo_id, sha: CommitSha(sha), recipe, source_bundle: source };
        self.executor.prepare(req, &NullObserver).await
    }
}
