use std::sync::Arc;

use mage_core::domain::{ArtifactKind, BlobOid, CommitSha, Language, Layer, LayerEvent, LayerState};
use mage_core::ports::ArtifactStore;
use mage_core::ports::blob::PutOptions;
use mage_core::ports::knowledge::{PageReq, ScipMetadata, ScipOccurrence, ScipRange, ScipRoles};
use uuid::Uuid;

use super::*;
use crate::testing::world::{World, WorldFile, world_org, world_path};
use crate::testing::{FakeScipReader, ScipFixture};

const SYMBOL: &str = "scip-typescript npm app 1.0.0 src/`conn.ts`/connect().";

fn range(line: u32, start: u32, end: u32) -> ScipRange {
    ScipRange { start_line: line, start_col: start, end_line: line, end_col: end }
}

fn occ(line: u32, symbol: &str, roles: u32) -> ScipOccurrence {
    ScipOccurrence { range: range(line, 2, 9), symbol: symbol.into(), roles: ScipRoles(roles), enclosing: None }
}

fn info(symbol: &str) -> ScipSymbolInfo {
    ScipSymbolInfo { symbol: symbol.into(), display_name: Some("connect".into()), kind: Some("Function".into()), documentation: None, relationships: Vec::new() }
}

fn doc(path: &str, language: &str, occurrences: Vec<ScipOccurrence>, symbols: Vec<ScipSymbolInfo>) -> ScipDocument {
    ScipDocument { relative_path: path.into(), language: language.into(), occurrences, symbols }
}

fn fixture() -> ScipFixture {
    ScipFixture {
        metadata: ScipMetadata { tool_name: "scip-typescript".into(), tool_version: "0.3.15".into(), project_root: "file:///repo".into() },
        documents: vec![
            doc("src/conn.ts", "TypeScript", vec![occ(3, SYMBOL, ScipRoles::DEFINITION), occ(4, "local 1", 0), occ(99, SYMBOL, 0)], vec![
                info(SYMBOL),
                info("local 1"),
            ]),
            doc("./src/api.ts", "TypeScript", vec![occ(7, SYMBOL, ScipRoles::READ_ACCESS)], Vec::new()),
            doc("node_modules/x/index.d.ts", "TypeScript", vec![occ(1, SYMBOL, 0)], Vec::new()),
            doc("../escape.ts", "TypeScript", vec![occ(1, SYMBOL, 0)], Vec::new()),
            doc("web/.git/config", "", vec![occ(1, SYMBOL, 0)], Vec::new()),
            doc("src/util.go", "Go", Vec::new(), Vec::new()),
        ],
        external_symbols: vec![info("scip-typescript npm react 18.2.0 `index.d.ts`/useState()."), info("local 9")],
    }
}

struct Env {
    w: World,
    reader: Arc<FakeScipReader>,
    data: tempfile::TempDir,
    uc: IngestScip,
}

impl Env {
    fn new() -> Env {
        let w = World::new();
        let reader = Arc::new(FakeScipReader::new());
        let data = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let uc = IngestScip::new(w.repos.clone(), w.knowledge.clone(), w.artifacts.clone(), reader.clone(), w.clock.clone(), data.path());
        Env { w, reader, data, uc }
    }

    /// A version with Files Ready and the fixture's TS files (10 lines each), plus the artifact.
    async fn setup(&self, ready: &[Layer]) -> (RepoVersionKey, ArtifactId, String) {
        let repo = self.w.repo("app").await;
        let key = self.w.version(repo.id, CommitSha::from_bytes([4; 20]), ready).await;
        let mut conn = WorldFile::source("src/conn.ts", BlobOid::from_bytes([1; 20]), Language::TypeScript);
        conn.lines = 10;
        let mut api = WorldFile::source("src/api.ts", BlobOid::from_bytes([2; 20]), Language::TypeScript);
        api.lines = 10;
        let go = WorldFile::source("src/util.go", BlobOid::from_bytes([3; 20]), Language::Go);
        self.w.manifest(&key, &[conn, api, go]).await;
        let run = RunId::from_uuid(Uuid::from_u128(0x0d01));
        let artifact = ArtifactId::from_uuid(Uuid::from_u128(0x0e01));
        let object = BlobKey::run_artifact(world_org(), run, ArtifactKind::ScipIndex, "typescript.scip").unwrap_or_else(|e| panic!("{e}"));
        let opts = PutOptions { content_type: "application/octet-stream".into(), if_absent: false };
        self.w.artifacts.put(&object, b"scip bytes".to_vec(), &opts).await.unwrap_or_else(|e| panic!("{e}"));
        self.reader.insert(self.data.path().join("scip").join(format!("{artifact}.scip")), fixture());
        (key, artifact, object.as_str().to_owned())
    }

    async fn run(&self, key: RepoVersionKey, artifact: ArtifactId, object: &str) -> Result<JobDone, AppError> {
        let run = RunId::from_uuid(Uuid::from_u128(0x0d01));
        self.uc.run(world_org(), key.repo_id, key.sha, run, artifact, object).await
    }

    async fn refs_state(&self, key: RepoVersionKey) -> LayerState {
        let version = self.w.repos.version(world_org(), key).await.unwrap_or_else(|e| panic!("{e}"));
        version.layer(Layer::Refs).cloned().unwrap_or(LayerState::Pending)
    }
}

#[tokio::test]
async fn ingests_documents_mapped_to_manifest_blobs() {
    let e = Env::new();
    let (key, artifact, object) = e.setup(&[Layer::Files, Layer::Symbols]).await;
    assert_eq!(e.run(key, artifact, &object).await.unwrap_or_else(|err| panic!("{err}")), JobDone::done());
    assert!(e.refs_state(key).await.is_ready());

    let index = e.w.knowledge.scip_index_for(world_org(), &key).await.unwrap_or_else(|err| panic!("{err}")).unwrap_or_else(|| panic!("no index"));
    assert_eq!((index.artifact, index.sha, index.tool.as_str(), index.documents), (artifact, key.sha, "scip-typescript", 3));
    assert_eq!(index.languages, ["Go", "TypeScript"], "languages of stored documents only");

    let hits = e.w.knowledge.scip_occurrences(world_org(), artifact, &key, SYMBOL, true, &PageReq::first(50)).await.unwrap_or_else(|err| panic!("{err}")).items;
    let at: Vec<(&str, u32)> = hits.iter().map(|h| (h.path.as_str(), h.pos.0)).collect();
    assert_eq!(at, [("src/api.ts", 7), ("src/conn.ts", 3)], "out-of-manifest documents and out-of-range occurrences dropped");
    let local = e.w.knowledge.scip_occurrences(world_org(), artifact, &key, "local 1", true, &PageReq::first(50)).await.unwrap_or_else(|err| panic!("{err}")).items;
    assert!(local.is_empty(), "local symbols are never stored");
    let def = e.w.knowledge.scip_symbol_at(world_org(), artifact, &world_path("src/api.ts"), 7, Some(3)).await.unwrap_or_else(|err| panic!("{err}"));
    let def = def.unwrap_or_else(|| panic!("symbol at api.ts:7"));
    assert_eq!(def.symbol, SYMBOL);
    assert_eq!(def.def, Some((world_path("src/conn.ts"), range(3, 2, 9))));
    assert!(!e.data.path().join("scip").join(format!("{artifact}.scip")).exists(), "download deleted");
}

#[tokio::test]
async fn skipped_refs_reopen_for_a_learn_run_index() {
    let e = Env::new();
    let (key, artifact, object) = e.setup(&[Layer::Files]).await;
    e.w.layer(key, Layer::Refs, LayerEvent::Skip { reason: "no recipe".into() }).await;
    e.run(key, artifact, &object).await.unwrap_or_else(|err| panic!("{err}"));
    assert!(e.refs_state(key).await.is_ready());
    // Re-ingesting the same artifact (job retry after success) keeps the layer Ready.
    e.run(key, artifact, &object).await.unwrap_or_else(|err| panic!("{err}"));
    assert!(e.refs_state(key).await.is_ready());
}

#[tokio::test]
async fn preconditions() {
    let e = Env::new();
    let (key, artifact, object) = e.setup(&[]).await;
    assert!(matches!(e.run(key, artifact, &object).await, Err(AppError::Unavailable(_))), "files not ready: retry");
    let unknown = RepoVersionKey::new(key.repo_id, CommitSha::from_bytes([9; 20]));
    assert!(matches!(e.run(unknown, artifact, &object).await, Err(AppError::Unavailable(_))), "version not indexed yet: retry");
    e.w.layer(key, Layer::Files, LayerEvent::Start).await;
    e.w.layer(key, Layer::Files, LayerEvent::Complete { at: mage_core::testing::FakeClock::DEFAULT_START }).await;
    let foreign = object.replacen(&world_org().as_uuid().hyphenated().to_string(), &Uuid::from_u128(0x0f).hyphenated().to_string(), 1);
    assert!(matches!(e.run(key, artifact, &foreign).await, Err(AppError::Invalid(_))), "key of another org");
    assert_eq!(e.refs_state(key).await, LayerState::Pending, "nothing started");
}

#[test]
fn document_paths_are_normalized_and_confined() {
    assert_eq!(doc_path("./src/a.ts").map(RepoPath::into_string).as_deref(), Some("src/a.ts"));
    for bad in ["/abs/a.ts", "../a.ts", "src/../a.ts", ".git/config", "a/.git/b", ""] {
        assert_eq!(doc_path(bad), None, "{bad}");
    }
    assert!(is_local("local 12") && !is_local("scip-go gomod x v1 `x`/local."));
}
