//! Static asset service for the embedded `mage/web-app` bundle (server.md "Probes, static,
//! keys", "Adapter internals": SPA fallback only for the UI path set) and the trace-viewer
//! bundle on the trace host.
//!
//! The bundle is held in memory (built from a directory at startup or from a map the caller
//! embeds); lookups never touch the filesystem, so there is no path traversal surface.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use bytes::Bytes;

const INDEX: &str = "index.html";
const IMMUTABLE: &str = "public, max-age=31536000, immutable";
const NO_CACHE: &str = "no-cache";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AssetsError {
    #[error("asset bundle has no index.html")]
    NoIndex,
    #[error("reading asset bundle: {0}")]
    Io(String),
    #[error("asset path {0:?} is not a relative path")]
    BadPath(String),
}

#[derive(Debug, Clone)]
struct Asset {
    bytes: Bytes,
    content_type: &'static str,
}

/// An in-memory bundle: paths relative to the bundle root (`index.html`, `assets/app-1a2b.js`).
#[derive(Debug, Clone)]
pub struct Assets {
    files: Arc<HashMap<String, Asset>>,
}

impl Assets {
    /// From `(relative path, bytes)` pairs; `index.html` is required.
    pub fn from_map(files: impl IntoIterator<Item = (String, Bytes)>) -> Result<Assets, AssetsError> {
        let mut map = HashMap::new();
        for (path, bytes) in files {
            let path = path.trim_start_matches('/').to_owned();
            if path.is_empty() || path.split('/').any(|seg| seg.is_empty() || seg == "." || seg == "..") || path.contains('\\') {
                return Err(AssetsError::BadPath(path));
            }
            let content_type = content_type(&path);
            map.insert(path, Asset { bytes, content_type });
        }
        if !map.contains_key(INDEX) {
            return Err(AssetsError::NoIndex);
        }
        Ok(Assets { files: Arc::new(map) })
    }

    /// Reads every regular file under `dir` (symlinks are not followed).
    pub fn from_dir(dir: &Path) -> Result<Assets, AssetsError> {
        let mut files = Vec::new();
        collect(dir, "", &mut files)?;
        Assets::from_map(files)
    }

    fn get(&self, path: &str) -> Option<&Asset> {
        self.files.get(path)
    }

    fn respond(&self, path: &str, cache: &'static str) -> Response {
        match self.get(path) {
            Some(asset) => {
                let mut response = Response::new(Body::from(asset.bytes.clone()));
                let h = response.headers_mut();
                h.insert(header::CONTENT_TYPE, HeaderValue::from_static(asset.content_type));
                h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
                h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
                response
            }
            None => (StatusCode::NOT_FOUND, "not found").into_response(),
        }
    }

    fn index(&self) -> Response {
        self.respond(INDEX, NO_CACHE)
    }
}

fn collect(dir: &Path, prefix: &str, out: &mut Vec<(String, Bytes)>) -> Result<(), AssetsError> {
    let entries = std::fs::read_dir(dir).map_err(|e| AssetsError::Io(format!("{}: {e}", dir.display())))?;
    for entry in entries {
        let entry = entry.map_err(|e| AssetsError::Io(e.to_string()))?;
        let file_type = entry.file_type().map_err(|e| AssetsError::Io(e.to_string()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
        if file_type.is_dir() {
            collect(&entry.path(), &rel, out)?;
        } else if file_type.is_file() {
            let bytes = std::fs::read(entry.path()).map_err(|e| AssetsError::Io(format!("{rel}: {e}")))?;
            out.push((rel, Bytes::from(bytes)));
        }
    }
    Ok(())
}

fn content_type(path: &str) -> &'static str {
    let ext = path.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "txt" => "text/plain; charset=utf-8",
        "wasm" => "application/wasm",
        "webmanifest" => "application/manifest+json",
        _ => "application/octet-stream",
    }
}

/// Public host UI: `/assets/{file}` (hashed, immutable) and the SPA shell for `/`, `/login`,
/// `/o/**` and the short links (`/tickets/{id}`, `/runs/{id}`, `/causes/{id}/brief`,
/// `/artifacts/{id}`). Nothing else.
pub fn router(assets: Assets) -> Router {
    Router::new()
        .route("/assets/{*file}", get(asset))
        .route("/", get(shell))
        .route("/login", get(shell))
        .route("/o/{*rest}", get(shell))
        .route("/tickets/{ticket}", get(shell))
        .route("/runs/{run}", get(shell))
        .route("/causes/{cause}/brief", get(shell))
        .route("/artifacts/{artifact}", get(shell))
        .with_state(assets)
}

async fn shell(State(assets): State<Assets>) -> Response {
    assets.index()
}

async fn asset(State(assets): State<Assets>, UrlPath(file): UrlPath<String>) -> Response {
    assets.respond(&format!("assets/{file}"), IMMUTABLE)
}

/// Trace host: the pinned Playwright trace-viewer bundle under `/trace/`. No cookies are ever
/// set on this host; the viewer fetches traces from `/share/v1/{token}` on the public host.
pub fn trace_router(viewer: Assets) -> Router {
    Router::new()
        .route("/trace", get(trace_index))
        .route("/trace/", get(trace_index))
        .route("/trace/{*file}", get(trace_file))
        .with_state(viewer)
        .layer(axum::middleware::from_fn(trace_headers))
}

async fn trace_index(State(viewer): State<Assets>) -> Response {
    viewer.index()
}

async fn trace_file(State(viewer): State<Assets>, UrlPath(file): UrlPath<String>) -> Response {
    let cache = if file == INDEX { NO_CACHE } else { "public, max-age=3600" };
    viewer.respond(&file, cache)
}

async fn trace_headers(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let mut response = next.run(req).await;
    let h = response.headers_mut();
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    response
}

#[cfg(test)]
mod tests {
    use axum::http::Request;
    use tower::ServiceExt;

    use super::*;

    fn bundle() -> Assets {
        Assets::from_map([
            ("index.html".to_owned(), Bytes::from_static(b"<html>shell</html>")),
            ("assets/app-1a2b.js".to_owned(), Bytes::from_static(b"console.log(1)")),
        ])
        .expect("bundle")
    }

    async fn get(router: &Router, path: &str) -> Response {
        router.clone().oneshot(Request::get(path).body(Body::empty()).expect("request")).await.expect("response")
    }

    #[tokio::test]
    async fn spa_shell_only_for_ui_paths_and_hashed_assets_are_immutable() {
        let router = router(bundle());
        for path in ["/", "/login", "/o/hoop/tickets", "/o/hoop", "/tickets/tkt_x", "/runs/run_x", "/causes/cau_x/brief", "/artifacts/art_x"] {
            let r = get(&router, path).await;
            assert_eq!(r.status(), StatusCode::OK, "{path}");
            assert_eq!(r.headers()[header::CACHE_CONTROL], NO_CACHE, "{path}");
        }
        let js = get(&router, "/assets/app-1a2b.js").await;
        assert_eq!(js.status(), StatusCode::OK);
        assert_eq!(js.headers()[header::CONTENT_TYPE], "text/javascript; charset=utf-8");
        assert_eq!(js.headers()[header::CACHE_CONTROL], IMMUTABLE);
        for path in ["/assets/missing.js", "/assets/../index.html", "/api/v1/nope", "/random", "/causes/cau_x"] {
            assert_eq!(get(&router, path).await.status(), StatusCode::NOT_FOUND, "{path}");
        }
    }

    #[test]
    fn bundles_need_an_index_and_relative_paths() {
        assert_eq!(Assets::from_map([("a.js".to_owned(), Bytes::new())]).err(), Some(AssetsError::NoIndex));
        assert!(matches!(Assets::from_map([("../x".to_owned(), Bytes::new())]), Err(AssetsError::BadPath(_))));
    }

    #[test]
    fn bundles_load_from_a_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("assets")).expect("mkdir");
        std::fs::write(dir.path().join("index.html"), "<html/>").expect("write");
        std::fs::write(dir.path().join("assets").join("x.css"), "a{}").expect("write");
        let assets = Assets::from_dir(dir.path()).expect("load");
        assert_eq!(assets.get("assets/x.css").map(|a| a.content_type), Some("text/css; charset=utf-8"));
    }
}
