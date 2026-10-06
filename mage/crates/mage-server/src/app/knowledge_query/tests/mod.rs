//! Knowledge query tests over in-memory fakes (knowledge.md "Testing" → Query): every tool
//! against `MemKnowledgeStore` fixtures, scope enforcement, cursors, the 24 KiB cap, degraded
//! paths when layers are not Ready or the indexer is down, the memory cutoff, token checks and
//! the triage retrieval stages.

mod auth;
mod files;
mod graph;
mod history;
mod retrieval;
mod symbols;

use std::sync::Arc;

use mage_core::domain::{
    BlobOid, CommitSha, EntryKind, FileClass, Language, Layer, LayerEvent, LineRange, ManifestEntry, OrgId, Repo, RepoAccess, RepoId,
    RepoPath, RepoVersionKey, RunId, VersionReason,
};
use mage_core::domain::{IndexRevs, InstallationId};
use mage_core::ports::blob::{ArtifactStore, BlobKey, PutOptions};
use mage_core::ports::clock::Clock;
use mage_core::ports::knowledge::{
    AppMapRev, BlobMeta, ErrorsRev, KnowledgeStore, NameRef, ParsedBlob, ParserRev, RefKind, StageStatus, SymbolDef, SymbolKind,
};
use mage_core::ports::repos::RepoStore;
use mage_core::testing::{FakeClock, MemArtifactStore, MemJobQueue, MemKnowledgeStore, MemRepoStore, SeqIdGen};
use mage_core::wire::token::MemoryCutoff;
use parking_lot::Mutex;
use serde_json::{Value, json};
use uuid::Uuid;

use super::{CallError, KnowledgeScope, Service, ServiceDeps, SourceReader, ToolErrorCode};
use crate::testing::{FakeIndexerLink, MemChangeFeed, MemRunStore, MemTicketStore};

pub(super) const REV: u32 = 1;

pub(crate) struct Fx {
    pub clock: Arc<FakeClock>,
    pub repos: Arc<MemRepoStore>,
    pub knowledge: Arc<MemKnowledgeStore>,
    pub artifacts: Arc<MemArtifactStore>,
    pub indexer: Arc<FakeIndexerLink>,
    pub tickets: Arc<MemTicketStore>,
    pub runs: Arc<MemRunStore>,
    pub service: Service,
    pub org: OrgId,
    pub repo: RepoId,
    pub sha: CommitSha,
    manifest: Mutex<Vec<ManifestEntry>>,
    next_oid: Mutex<u8>,
}

#[track_caller]
pub(crate) fn path(s: &str) -> RepoPath {
    RepoPath::new(s).unwrap_or_else(|e| panic!("{s}: {e}"))
}

#[track_caller]
pub(super) fn lines(start: u32, end: u32) -> LineRange {
    LineRange::new(start, end).unwrap_or_else(|e| panic!("{e}"))
}

pub(super) fn sym(ord: u32, name: &str, kind: SymbolKind, (start, end): (u32, u32)) -> SymbolDef {
    SymbolDef {
        ord,
        name: name.into(),
        qualified: name.into(),
        kind,
        container: None,
        span: lines(start, end),
        name_pos: (start, 6),
        signature: Some(format!("func {name}()")),
        doc: None,
        exported: true,
    }
}

pub(super) fn call(ord: u32, name: &str, line: u32, enclosing: Option<u32>) -> NameRef {
    NameRef { ord, name: name.into(), qualifier: None, kind: RefKind::Call, pos: (line, 2), enclosing }
}

pub(super) fn sha(n: u8) -> CommitSha {
    CommitSha::from_bytes([n; 20])
}

impl Fx {
    /// One repo with a version at `sha(0xab)` whose layers are all Ready except Semantic
    /// (Skipped, as in v1) and Refs (Skipped: no recipe).
    pub async fn new() -> Fx {
        let fx = Fx::pending().await;
        for layer in [Layer::Files, Layer::Symbols, Layer::ErrorSites, Layer::AppMap, Layer::History] {
            fx.ready(layer).await;
        }
        fx.skip(Layer::Refs, "no recipe").await;
        fx.skip(Layer::Semantic, "disabled").await;
        fx
    }

    /// Every layer Pending (no revs set).
    pub async fn pending() -> Fx {
        let clock = Arc::new(FakeClock::new());
        let jobs = Arc::new(MemJobQueue::new(clock.clone(), Arc::new(SeqIdGen::new())));
        let repos = Arc::new(MemRepoStore::new(clock.clone(), jobs.clone()));
        let knowledge = Arc::new(MemKnowledgeStore::new(clock.clone(), repos.clone()));
        let artifacts = Arc::new(MemArtifactStore::new(clock.clone()));
        let indexer = Arc::new(FakeIndexerLink::new());
        let feed = Arc::new(MemChangeFeed::new(clock.clone()));
        let tickets = Arc::new(MemTicketStore::new(clock.clone(), jobs.clone(), feed));
        let runs = Arc::new(MemRunStore::new(clock.clone(), jobs.clone()));
        let org = OrgId::from_uuid(Uuid::from_u128(0x0a01));
        let repo = Repo {
            id: RepoId::from_uuid(Uuid::from_u128(0x0b01)),
            org_id: org,
            external_id: 42,
            owner: "acme".into(),
            name: "app".into(),
            default_branch: "main".into(),
            installation_id: InstallationId(7),
            access: RepoAccess::Granted,
            created_at: FakeClock::DEFAULT_START,
        };
        let repo = repos.upsert_repo(&repo, &[]).await.unwrap_or_else(|e| panic!("{e}")).id;
        let sha = sha(0xab);
        repos.add_version_reason(org, repo, sha, VersionReason::DefaultHead).await.unwrap_or_else(|e| panic!("{e}"));
        let service = Service::new(ServiceDeps {
            knowledge: knowledge.clone(),
            repos: repos.clone(),
            tickets: tickets.clone(),
            runs: runs.clone(),
            indexer: indexer.clone(),
            source: SourceReader::new(knowledge.clone(), artifacts.clone()),
        });
        Fx {
            clock,
            repos,
            knowledge,
            artifacts,
            indexer,
            tickets,
            runs,
            service,
            org,
            repo,
            sha,
            manifest: Mutex::new(Vec::new()),
            next_oid: Mutex::new(1),
        }
    }

    pub fn key(&self) -> RepoVersionKey {
        RepoVersionKey::new(self.repo, self.sha)
    }

    pub fn scope(&self) -> KnowledgeScope {
        KnowledgeScope {
            org: self.org,
            run: RunId::from_uuid(Uuid::from_u128(0x0c01)),
            repo: self.repo,
            sha: self.sha,
            memory: MemoryCutoff::default(),
            disabled_layers: Vec::new(),
        }
    }

    /// A second indexed version of the same repo, Files Ready, with `files` as its manifest.
    pub async fn other_version(&self, other: CommitSha, files: &[(&str, &str)]) -> RepoVersionKey {
        self.repos.add_version_reason(self.org, self.repo, other, VersionReason::Release(other.to_hex())).await.unwrap_or_else(|e| panic!("{e}"));
        let key = RepoVersionKey::new(self.repo, other);
        let mut entries = Vec::new();
        for (p, text) in files {
            let oid = self.store_blob(text).await;
            entries.push(self.entry(p, oid, text, FileClass::Source));
        }
        self.knowledge.put_manifest(self.org, &key, &entries).await.unwrap_or_else(|e| panic!("{e}"));
        self.repos.set_index_revs(self.org, key, &revs()).await.unwrap_or_else(|e| panic!("{e}"));
        self.repos.apply_layer_event(self.org, key, Layer::Files, LayerEvent::Start, &[]).await.unwrap_or_else(|e| panic!("{e}"));
        self.repos.apply_layer_event(self.org, key, Layer::Files, LayerEvent::Complete { at: self.clock.now() }, &[]).await.unwrap_or_else(|e| panic!("{e}"));
        key
    }

    pub async fn ready(&self, layer: Layer) {
        let key = self.key();
        if layer == Layer::Files || self.repos.index_revs(self.org, key).is_none() {
            self.repos.set_index_revs(self.org, key, &revs()).await.unwrap_or_else(|e| panic!("{e}"));
        }
        self.repos.apply_layer_event(self.org, key, layer, LayerEvent::Start, &[]).await.unwrap_or_else(|e| panic!("{e}"));
        self.repos.apply_layer_event(self.org, key, layer, LayerEvent::Complete { at: self.clock.now() }, &[]).await.unwrap_or_else(|e| panic!("{e}"));
    }

    pub async fn skip(&self, layer: Layer, reason: &str) {
        self.repos.apply_layer_event(self.org, self.key(), layer, LayerEvent::Skip { reason: reason.into() }, &[]).await.unwrap_or_else(|e| panic!("{e}"));
    }

    /// `Ready` → `Pending` (manual reset), so the layer is not Ready.
    pub async fn reset(&self, layer: Layer) {
        self.repos.apply_layer_event(self.org, self.key(), layer, LayerEvent::Reset, &[]).await.unwrap_or_else(|e| panic!("{e}"));
    }

    async fn store_blob(&self, text: &str) -> BlobOid {
        let oid = {
            let mut next = self.next_oid.lock();
            *next += 1;
            BlobOid::from_bytes([*next; 20])
        };
        let meta = BlobMeta { oid, size: text.len() as u64, line_count: line_count(text), is_text: true, content_stored: true };
        self.knowledge.put_blob_meta(self.org, &[meta]).await.unwrap_or_else(|e| panic!("{e}"));
        let opts = PutOptions { content_type: "application/octet-stream".into(), if_absent: true };
        self.artifacts.put(&BlobKey::blob_content(self.org, oid), text.as_bytes().to_vec(), &opts).await.unwrap_or_else(|e| panic!("{e}"));
        oid
    }

    fn entry(&self, p: &str, oid: BlobOid, text: &str, class: FileClass) -> ManifestEntry {
        let lang = match path(p).extension() {
            Some("go") => Some(Language::Go),
            Some("ts") => Some(Language::TypeScript),
            Some("cljs") => Some(Language::ClojureScript),
            _ => None,
        };
        ManifestEntry {
            path: path(p),
            kind: EntryKind::File,
            executable: false,
            blob: oid,
            lang,
            class,
            size: text.len() as u64,
            is_text: true,
            line_count: line_count(text),
        }
    }

    /// Adds a text file to the scope version's manifest (and to the fake indexer's checkout).
    pub async fn file(&self, p: &str, text: &str) -> BlobOid {
        self.file_with(p, text, FileClass::Source).await
    }

    pub async fn file_with(&self, p: &str, text: &str, class: FileClass) -> BlobOid {
        let oid = self.store_blob(text).await;
        let entry = self.entry(p, oid, text, class);
        self.put_entry(entry).await;
        self.indexer.put_file(self.org, self.key(), path(p), text, matches!(class, FileClass::Generated | FileClass::Vendored));
        oid
    }

    /// Adds a raw manifest entry (binary, submodule, symlink, large files).
    pub async fn put_entry(&self, entry: ManifestEntry) {
        let entries = {
            let mut manifest = self.manifest.lock();
            manifest.retain(|e| e.path != entry.path);
            manifest.push(entry);
            manifest.sort_by(|a, b| a.path.cmp(&b.path));
            manifest.clone()
        };
        self.knowledge.put_manifest(self.org, &self.key(), &entries).await.unwrap_or_else(|e| panic!("{e}"));
    }

    pub async fn put_blob(&self, oid: BlobOid, bytes: &[u8], is_text: bool, stored: bool) {
        let meta = BlobMeta { oid, size: bytes.len() as u64, line_count: if is_text { line_count(std::str::from_utf8(bytes).unwrap_or("")) } else { 0 }, is_text, content_stored: stored };
        self.knowledge.put_blob_meta(self.org, &[meta]).await.unwrap_or_else(|e| panic!("{e}"));
        if stored {
            let opts = PutOptions { content_type: "application/octet-stream".into(), if_absent: true };
            self.artifacts.put(&BlobKey::blob_content(self.org, oid), bytes.to_vec(), &opts).await.unwrap_or_else(|e| panic!("{e}"));
        }
    }

    pub async fn parsed(&self, oid: BlobOid, symbols: Vec<SymbolDef>, refs: Vec<NameRef>) {
        let parsed = ParsedBlob { lang: Language::Go, status: StageStatus::Ok, symbols, refs, literals: Vec::new() };
        self.knowledge.put_parsed(self.org, ParserRev(REV), &[(oid, Ok(parsed))]).await.unwrap_or_else(|e| panic!("{e}"));
    }

    pub async fn call(&self, tool: &str, args: Value) -> Result<Value, CallError> {
        self.call_as(&self.scope(), tool, args).await
    }

    pub async fn call_as(&self, scope: &KnowledgeScope, tool: &str, args: Value) -> Result<Value, CallError> {
        self.service.call(scope, tool, args).await.map(|out| out.value)
    }

    /// The tool error code of a failed call.
    pub async fn error(&self, tool: &str, args: Value) -> ToolErrorCode {
        match self.call(tool, args).await {
            Err(CallError::Tool(e)) => e.code,
            other => panic!("{tool}: expected a tool error, got {other:?}"),
        }
    }
}

pub(super) fn revs() -> IndexRevs {
    IndexRevs { parser: ParserRev(REV), errors: ErrorsRev(REV), appmap: AppMapRev(REV) }
}

pub(super) fn line_count(text: &str) -> u32 {
    let newlines = text.bytes().filter(|b| *b == b'\n').count();
    let extra = usize::from(!text.is_empty() && !text.ends_with('\n'));
    u32::try_from(newlines + extra).unwrap_or(u32::MAX)
}

/// The envelope fields every successful result carries.
#[track_caller]
pub(super) fn assert_envelope(value: &Value, fx: &Fx, layer: &str) {
    assert_eq!(value["repo"], json!(fx.repo), "repo");
    assert_eq!(value["sha"], json!(fx.sha.to_hex()), "sha");
    assert_eq!(value["layer"], layer, "layer: {value}");
    for key in ["degraded", "items", "next_cursor", "truncated", "notes"] {
        assert!(value.get(key).is_some(), "envelope field {key} in {value}");
    }
}

/// `path:start-end@sha` of the scope sha.
pub(super) fn cite_ref(fx: &Fx, p: &str, start: u32, end: u32) -> String {
    if start == end { format!("{p}:{start}@{}", fx.sha.to_hex()) } else { format!("{p}:{start}-{end}@{}", fx.sha.to_hex()) }
}

#[tokio::test]
async fn every_listed_tool_dispatches_and_unknown_tools_are_rejected() {
    let fx = Fx::new().await;
    for tool in super::KNOWLEDGE_TOOLS {
        // Tools with required arguments fail with -32602; never with UnknownTool.
        let got = fx.call(tool, json!({})).await;
        assert!(!matches!(got, Err(CallError::UnknownTool(_))), "{tool}");
    }
    assert!(matches!(fx.call("drop_tables", json!({})).await, Err(CallError::UnknownTool(_))));
    assert!(matches!(fx.call("read_file", json!({ "path": "a", "bogus": 1 })).await, Err(CallError::BadArguments(_))), "unknown fields are -32602");
    assert!(matches!(fx.call("read_file", json!({ "path": 7 })).await, Err(CallError::BadArguments(_))));
}

#[tokio::test]
async fn files_not_ready_is_version_not_ready_except_memory_and_recipe() {
    let fx = Fx::pending().await;
    for (tool, args) in [
        ("repo_overview", json!({})),
        ("read_file", json!({ "path": "a.go" })),
        ("search_code", json!({ "query": "x" })),
        ("find_symbol", json!({ "name": "x" })),
        ("lookup_error", json!({ "text": "boom" })),
        ("app_map", json!({})),
        ("blame", json!({ "path": "a.go", "start_line": 1, "end_line": 2 })),
    ] {
        match fx.call(tool, args).await {
            Err(CallError::Tool(e)) => {
                assert_eq!(e.code, ToolErrorCode::VersionNotReady, "{tool}");
                let layers = e.layers.unwrap_or_else(|| panic!("{tool}: layer states"));
                assert_eq!(layers["Files"], "Pending");
            }
            other => panic!("{tool}: {other:?}"),
        }
    }
    let recipe = fx.call("run_recipe", json!({})).await.unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(recipe["layer"], "Recipe");
    assert_eq!(recipe["status"], "none");
    let similar = fx.call("similar_tickets", json!({ "text": "login fails" })).await.unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(similar["layer"], "Memory");
}

#[tokio::test]
async fn repo_and_sha_arguments_must_match_the_token() {
    let fx = Fx::new().await;
    fx.file("a.go", "package a\n").await;
    let other_sha = sha(0x01).to_hex();
    let other_repo = RepoId::from_uuid(Uuid::from_u128(0x0bff)).to_string();
    for (tool, args) in [
        ("read_file", json!({ "path": "a.go", "sha": other_sha })),
        ("read_file", json!({ "path": "a.go", "repo": other_repo })),
        ("search_code", json!({ "query": "package", "sha": other_sha })),
        ("find_symbol", json!({ "name": "x", "repo": other_repo })),
        ("run_recipe", json!({ "repo": other_repo })),
        ("similar_tickets", json!({ "text": "x", "sha": other_sha })),
        ("changes_between", json!({ "from": other_sha, "to": other_sha })),
    ] {
        assert_eq!(fx.error(tool, args).await, ToolErrorCode::OutOfScope, "{tool}");
    }
    let same = fx.call("read_file", json!({ "path": "a.go", "sha": fx.sha.to_hex(), "repo": fx.repo.to_string() })).await;
    assert!(same.is_ok(), "{same:?}");
    // `from` must be a commit of this repo known to History or an indexed version.
    assert_eq!(fx.error("changes_between", json!({ "from": sha(0x02).to_hex() })).await, ToolErrorCode::OutOfScope);
}

#[tokio::test]
async fn disabled_layers_behave_as_not_ready() {
    let fx = Fx::new().await;
    let oid = fx.file("api/conn.go", "package api\n\nfunc Connect() {}\n").await;
    fx.parsed(oid, vec![sym(0, "Connect", SymbolKind::Function, (3, 3))], vec![]).await;
    let mut scope = fx.scope();
    scope.disabled_layers = vec![Layer::Symbols];
    match fx.call_as(&scope, "find_symbol", json!({ "name": "Connect" })).await {
        Err(CallError::Tool(e)) => {
            assert_eq!(e.code, ToolErrorCode::LayerNotReady);
            assert_eq!(e.layers.unwrap_or_default()["Symbols"], "Disabled");
        }
        other => panic!("{other:?}"),
    }
    let found = fx.call("find_symbol", json!({ "name": "Connect" })).await.unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(found["items"][0]["name"], "Connect", "the same call works without the ablation");
}
