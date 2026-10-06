//! Response envelope, citations, tool errors, cursors and the result size cap (knowledge.md
//! "Response envelope").

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use mage_core::domain::{CommitSha, Layer, LayerState, RepoId, RepoPath, RepoVersion};
use mage_core::ports::knowledge::KnowledgeError;
use mage_core::ports::repos::RepoStoreError;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::domain::knowledge::{AnswerSource, layer_name};
use crate::error::AppError;
use crate::ports::indexer_link::IndexerLinkError;

/// Serialized result cap; items beyond are cut, `truncated: true`, `next_cursor` set.
pub const MAX_RESULT_BYTES: usize = 24 * 1024;

/// Citation object of every item that points at code. `ref` is `path:start-end@sha`, or
/// `path:line@sha` for one line; `sha` is always the full 40-hex sha.
pub fn citation(path: &RepoPath, start: u32, end: u32, sha: CommitSha) -> Value {
    let (start, end) = (start.max(1), end.max(start.max(1)));
    let lines = if start == end { start.to_string() } else { format!("{start}-{end}") };
    json!({
        "path": path.as_str(),
        "start_line": start,
        "end_line": end,
        "sha": sha.to_hex(),
        "ref": format!("{}:{lines}@{}", path.as_str(), sha.to_hex()),
    })
}

/// `degraded`: a fallback answered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Degraded {
    pub reason: String,
    pub wanted: String,
}

impl Degraded {
    pub fn new(reason: impl Into<String>, wanted: Layer) -> Self {
        Degraded { reason: reason.into(), wanted: layer_name(wanted).to_owned() }
    }
}

/// One tool result. `extra` holds the tool's own top-level fields (`overview`, `nodes`, `text`, …).
#[derive(Debug, Clone)]
pub struct Envelope {
    pub repo: RepoId,
    pub sha: CommitSha,
    pub layer: AnswerSource,
    pub degraded: Option<Degraded>,
    pub items: Vec<Value>,
    pub next_cursor: Option<String>,
    pub truncated: bool,
    pub notes: Vec<String>,
    pub extra: Map<String, Value>,
}

/// Which array of the result the size cap shortens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListField {
    Items,
    Extra(&'static str),
}

impl Envelope {
    pub fn new(repo: RepoId, sha: CommitSha, layer: impl Into<AnswerSource>) -> Self {
        Envelope {
            repo,
            sha,
            layer: layer.into(),
            degraded: None,
            items: Vec::new(),
            next_cursor: None,
            truncated: false,
            notes: Vec::new(),
            extra: Map::new(),
        }
    }

    pub fn note(&mut self, note: impl Into<String>) {
        self.notes.push(note.into());
    }

    pub fn set(&mut self, key: &str, value: Value) {
        self.extra.insert(key.to_owned(), value);
    }

    pub fn to_value(&self) -> Value {
        let mut object = Map::new();
        object.insert("repo".into(), json!(self.repo));
        object.insert("sha".into(), json!(self.sha));
        object.insert("layer".into(), json!(self.layer));
        object.insert("degraded".into(), json!(self.degraded));
        object.insert("items".into(), Value::Array(self.items.clone()));
        object.insert("next_cursor".into(), json!(self.next_cursor));
        object.insert("truncated".into(), json!(self.truncated));
        object.insert("notes".into(), json!(self.notes));
        for (key, value) in &self.extra {
            object.insert(key.clone(), value.clone());
        }
        Value::Object(object)
    }

    pub fn byte_len(&self) -> usize {
        serde_json::to_vec(&self.to_value()).map(|v| v.len()).unwrap_or(usize::MAX)
    }

    fn list_len(&self, field: ListField) -> usize {
        match field {
            ListField::Items => self.items.len(),
            ListField::Extra(name) => self.extra.get(name).and_then(Value::as_array).map_or(0, Vec::len),
        }
    }

    fn truncate_list(&mut self, field: ListField, n: usize) {
        match field {
            ListField::Items => self.items.truncate(n),
            ListField::Extra(name) => {
                if let Some(Value::Array(list)) = self.extra.get_mut(name) {
                    list.truncate(n);
                }
            }
        }
    }

    /// Enforces [`MAX_RESULT_BYTES`] by cutting `field` from the end (at least one element stays).
    /// Returns how many elements were kept when something was cut; the caller sets
    /// `next_cursor` to resume after them.
    pub fn fit(&mut self, field: ListField) -> Option<usize> {
        if self.byte_len() <= MAX_RESULT_BYTES {
            return None;
        }
        let len = self.list_len(field);
        if len <= 1 {
            return None;
        }
        let full = self.clone();
        // Largest n in [1, len) whose result fits.
        let (mut lo, mut hi) = (1usize, len - 1);
        let mut best = 1;
        while lo <= hi {
            let mid = (lo + hi) / 2;
            let mut probe = full.clone();
            probe.truncate_list(field, mid);
            probe.truncated = true;
            probe.next_cursor = Some("x".repeat(160));
            if probe.byte_len() <= MAX_RESULT_BYTES {
                best = mid;
                lo = mid + 1;
            } else {
                hi = mid - 1;
            }
        }
        self.truncate_list(field, best);
        self.truncated = true;
        Some(best)
    }
}

// ---------------------------------------------------------------------------------------------
// Tool errors

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolErrorCode {
    /// `Files` not Ready (includes layer states).
    VersionNotReady,
    LayerNotReady,
    OutOfScope,
    NotFound,
    NotText,
    TooLarge,
    InvalidArgument,
    BadCursor,
    Timeout,
    Unavailable,
    RateLimited,
}

impl ToolErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            ToolErrorCode::VersionNotReady => "version_not_ready",
            ToolErrorCode::LayerNotReady => "layer_not_ready",
            ToolErrorCode::OutOfScope => "out_of_scope",
            ToolErrorCode::NotFound => "not_found",
            ToolErrorCode::NotText => "not_text",
            ToolErrorCode::TooLarge => "too_large",
            ToolErrorCode::InvalidArgument => "invalid_argument",
            ToolErrorCode::BadCursor => "bad_cursor",
            ToolErrorCode::Timeout => "timeout",
            ToolErrorCode::Unavailable => "unavailable",
            ToolErrorCode::RateLimited => "rate_limited",
        }
    }
}

/// A tool-level failure (MCP `isError: true`): `{"error": {"code", "message", "layers"?}}`.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{}: {message}", code.as_str())]
pub struct ToolError {
    pub code: ToolErrorCode,
    pub message: String,
    pub layers: Option<Map<String, Value>>,
}

impl ToolError {
    pub fn new(code: ToolErrorCode, message: impl Into<String>) -> Self {
        ToolError { code, message: message.into(), layers: None }
    }

    pub fn out_of_scope(message: impl Into<String>) -> Self {
        Self::new(ToolErrorCode::OutOfScope, message)
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ToolErrorCode::InvalidArgument, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ToolErrorCode::NotFound, message)
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(ToolErrorCode::Unavailable, message)
    }

    pub fn bad_cursor() -> Self {
        Self::new(ToolErrorCode::BadCursor, "cursor does not belong to this call; repeat the call without it")
    }

    pub fn with_layers(mut self, layers: Map<String, Value>) -> Self {
        self.layers = Some(layers);
        self
    }

    pub fn to_value(&self) -> Value {
        let mut error = Map::new();
        error.insert("code".into(), json!(self.code.as_str()));
        error.insert("message".into(), json!(self.message));
        if let Some(layers) = &self.layers {
            error.insert("layers".into(), Value::Object(layers.clone()));
        }
        json!({ "error": error })
    }
}

impl From<KnowledgeError> for ToolError {
    fn from(e: KnowledgeError) -> Self {
        match e {
            KnowledgeError::NotFound => ToolError::not_found("not found at this version"),
            KnowledgeError::Invalid(m) => ToolError::invalid(m),
            KnowledgeError::Timeout => ToolError::new(ToolErrorCode::Timeout, "knowledge query timed out"),
            KnowledgeError::Unavailable(m) => ToolError::unavailable(m),
            KnowledgeError::Conflict => ToolError::unavailable("knowledge store conflict; retry"),
        }
    }
}

impl From<IndexerLinkError> for ToolError {
    fn from(e: IndexerLinkError) -> Self {
        match e {
            IndexerLinkError::NotFound => ToolError::unavailable("the indexer has no checkout for this version yet"),
            IndexerLinkError::Invalid(m) => ToolError::invalid(m),
            IndexerLinkError::Timeout => ToolError::new(ToolErrorCode::Timeout, "indexer call timed out"),
            IndexerLinkError::Unavailable(m) => ToolError::unavailable(m),
            IndexerLinkError::Conflict => ToolError::unavailable("indexer conflict; retry"),
        }
    }
}

impl From<RepoStoreError> for ToolError {
    fn from(e: RepoStoreError) -> Self {
        match e {
            RepoStoreError::NotFound => ToolError::not_found("not found"),
            RepoStoreError::Invalid(m) => ToolError::invalid(m),
            RepoStoreError::Unavailable(m) => ToolError::unavailable(m),
            RepoStoreError::Conflict => ToolError::unavailable("repo store conflict; retry"),
        }
    }
}

impl From<AppError> for ToolError {
    fn from(e: AppError) -> Self {
        match e {
            AppError::NotFound(m) => ToolError::not_found(m),
            AppError::Invalid(m) => ToolError::invalid(m),
            AppError::Domain(d) => ToolError::invalid(d.to_string()),
            AppError::RateLimited { .. } => ToolError::new(ToolErrorCode::RateLimited, "rate limited"),
            AppError::Unauthorized | AppError::Forbidden(_) => ToolError::out_of_scope("not allowed for this token"),
            AppError::Conflict(m) | AppError::Unavailable(m) | AppError::Internal(m) => ToolError::unavailable(m),
        }
    }
}

/// `{"Files": "Ready", "AppMap": "Skipped: no supported framework", …}` of a version, with
/// disabled layers shown as `Disabled`.
pub fn layer_states(version: &RepoVersion, disabled: &[Layer]) -> Map<String, Value> {
    Layer::ALL
        .into_iter()
        .map(|layer| {
            let state = if disabled.contains(&layer) {
                "Disabled".to_owned()
            } else {
                version.layer(layer).map_or_else(|| "Pending".to_owned(), describe_state)
            };
            (layer_name(layer).to_owned(), Value::String(state))
        })
        .collect()
}

pub fn describe_state(state: &LayerState) -> String {
    match state {
        LayerState::Pending => "Pending".into(),
        LayerState::Running => "Running".into(),
        LayerState::Ready { .. } => "Ready".into(),
        LayerState::Failed { reason } => format!("Failed: {reason}"),
        LayerState::Skipped { reason } => format!("Skipped: {reason}"),
    }
}

// ---------------------------------------------------------------------------------------------
// Cursors

/// FNV-1a 64 over bytes.
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

#[derive(Serialize, Deserialize)]
struct CursorBody {
    v: u8,
    t: String,
    h: String,
    k: String,
}

/// Binds cursors to one tool and its canonical arguments (without the cursor).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorScope {
    tool: &'static str,
    hash: String,
}

impl CursorScope {
    /// `args` is the parsed input with defaults applied and `cursor` cleared; its serde form is
    /// the canonical argument text.
    pub fn new<T: Serialize>(tool: &'static str, args: &T) -> Self {
        let canonical = serde_json::to_vec(args).unwrap_or_default();
        CursorScope { tool, hash: format!("{:016x}", fnv1a64(&canonical)) }
    }

    /// `base64url(JSON {"v":1,"t":tool,"h":hash,"k":keyset})`.
    pub fn encode(&self, keyset: &str) -> String {
        let body = CursorBody { v: 1, t: self.tool.to_owned(), h: self.hash.clone(), k: keyset.to_owned() };
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&body).unwrap_or_default())
    }

    /// The keyset of a cursor issued by this call; anything else is `bad_cursor`.
    pub fn decode(&self, cursor: &str) -> Result<String, ToolError> {
        let bytes = URL_SAFE_NO_PAD.decode(cursor.trim()).map_err(|_| ToolError::bad_cursor())?;
        let body: CursorBody = serde_json::from_slice(&bytes).map_err(|_| ToolError::bad_cursor())?;
        if body.v != 1 || body.t != self.tool || body.h != self.hash {
            return Err(ToolError::bad_cursor());
        }
        Ok(body.k)
    }

    pub fn decode_opt(&self, cursor: Option<&str>) -> Result<Option<String>, ToolError> {
        cursor.map(|c| self.decode(c)).transpose()
    }
}

/// Keyset of a store-paged list: the store's `after` of the page that holds the next item, and
/// how many items of that page were already returned (cuts inside a page resume exactly).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreKey {
    #[serde(rename = "a")]
    pub after: Option<String>,
    #[serde(rename = "s")]
    pub skip: u32,
}

impl StoreKey {
    pub fn parse(keyset: Option<&str>) -> Result<StoreKey, ToolError> {
        match keyset {
            None => Ok(StoreKey::default()),
            Some(k) => serde_json::from_str(k).map_err(|_| ToolError::bad_cursor()),
        }
    }

    pub fn to_keyset(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use mage_core::domain::{OrgId, RepoVersionKey, VersionReason};
    use time::macros::datetime;
    use uuid::Uuid;

    use super::*;

    #[test]
    fn citation_ref_uses_full_sha_and_collapses_single_lines() {
        let sha = CommitSha::from_bytes([0xab; 20]);
        let path = RepoPath::new("gateway/api/conn.go").unwrap_or_else(|e| panic!("{e}"));
        let range = citation(&path, 120, 134, sha);
        assert_eq!(range["ref"], format!("gateway/api/conn.go:120-134@{}", "ab".repeat(20)));
        assert_eq!(range["sha"].as_str().map(str::len), Some(40));
        assert_eq!(citation(&path, 7, 7, sha)["ref"], format!("gateway/api/conn.go:7@{}", "ab".repeat(20)));
    }

    #[test]
    fn cursor_is_bound_to_tool_and_arguments() {
        let a = CursorScope::new("search_code", &json!({ "query": "x", "limit": 20 }));
        let cursor = a.encode("k1");
        assert_eq!(a.decode(&cursor), Ok("k1".to_owned()));
        let other_args = CursorScope::new("search_code", &json!({ "query": "y", "limit": 20 }));
        assert_eq!(other_args.decode(&cursor).map_err(|e| e.code), Err(ToolErrorCode::BadCursor));
        let other_tool = CursorScope::new("find_symbol", &json!({ "query": "x", "limit": 20 }));
        assert_eq!(other_tool.decode(&cursor).map_err(|e| e.code), Err(ToolErrorCode::BadCursor));
        assert_eq!(a.decode("not base64 !").map_err(|e| e.code), Err(ToolErrorCode::BadCursor));
    }

    #[test]
    fn fit_cuts_items_to_the_cap() {
        let mut env = Envelope::new(RepoId::from_uuid(Uuid::from_u128(1)), CommitSha::from_bytes([1; 20]), Layer::Files);
        env.items = (0..400).map(|i| json!({ "n": i, "text": "y".repeat(100) })).collect();
        let kept = env.fit(ListField::Items);
        let kept = kept.unwrap_or_else(|| panic!("40 KiB of items must be cut"));
        assert_eq!(env.items.len(), kept);
        assert!(env.truncated);
        env.next_cursor = Some("c".repeat(150));
        assert!(env.byte_len() <= MAX_RESULT_BYTES);
        let mut bigger = env.clone();
        bigger.items.push(json!({ "n": 9999, "text": "y".repeat(100) }));
        assert!(bigger.byte_len() > MAX_RESULT_BYTES - 60, "the cut keeps as many items as fit");
    }

    #[test]
    fn layer_states_show_disabled_layers() {
        let org = OrgId::from_uuid(Uuid::from_u128(1));
        let key = RepoVersionKey::new(RepoId::from_uuid(Uuid::from_u128(2)), CommitSha::from_bytes([1; 20]));
        let mut version = RepoVersion::new(org, key, VersionReason::DefaultHead, datetime!(2026-01-01 0:00 UTC));
        version.layers.insert(Layer::Files, LayerState::Ready { at: datetime!(2026-01-01 0:01 UTC) });
        version.layers.insert(Layer::AppMap, LayerState::Skipped { reason: "no supported framework".into() });
        let states = layer_states(&version, &[Layer::ErrorSites]);
        assert_eq!(states["Files"], "Ready");
        assert_eq!(states["AppMap"], "Skipped: no supported framework");
        assert_eq!(states["ErrorSites"], "Disabled");
        assert_eq!(states["Symbols"], "Pending");
    }
}
