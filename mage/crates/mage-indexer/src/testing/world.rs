//! In-memory core stores wired together for use-case tests ([`World`]): repos and versions,
//! manifests with blob metadata, parsed symbols, layer states.

use std::sync::Arc;

use mage_core::domain::{
    BlobOid, CommitSha, EntryKind, FileClass, IndexRevs, InstallationId, Language, Layer, LayerEvent, ManifestEntry, OrgId, Repo, RepoAccess, RepoId, RepoPath,
    RepoVersionKey, VersionReason,
};
use mage_core::ports::knowledge::{AppMapRev, BlobMeta, ErrorsRev, ParsedBlob, ParserRev};
use mage_core::ports::{KnowledgeStore, RepoStore};
use mage_core::testing::{FakeClock, MemArtifactStore, MemJobQueue, MemKnowledgeStore, MemRepoStore, SeqIdGen};
use uuid::Uuid;

/// Revisions every [`World`] version is indexed at.
pub const WORLD_REVS: IndexRevs = IndexRevs { parser: ParserRev(1), errors: ErrorsRev(1), appmap: AppMapRev(1) };

pub struct World {
    pub clock: Arc<FakeClock>,
    pub jobs: Arc<MemJobQueue>,
    pub repos: Arc<MemRepoStore>,
    pub knowledge: Arc<MemKnowledgeStore>,
    pub artifacts: Arc<MemArtifactStore>,
}

/// One manifest file of a [`World`] version.
pub struct WorldFile<'a> {
    pub path: &'a str,
    pub blob: BlobOid,
    pub lang: Option<Language>,
    pub class: FileClass,
    pub lines: u32,
}

impl<'a> WorldFile<'a> {
    pub fn source(path: &'a str, blob: BlobOid, lang: Language) -> Self {
        WorldFile { path, blob, lang: Some(lang), class: FileClass::Source, lines: 100 }
    }
}

pub fn world_org() -> OrgId {
    OrgId::from_uuid(Uuid::from_u128(0x0a01))
}

pub fn world_path(s: &str) -> RepoPath {
    RepoPath::new(s).unwrap_or_else(|e| panic!("bad test path {s:?}: {e}"))
}

impl World {
    pub fn new() -> Self {
        let clock = Arc::new(FakeClock::new());
        let jobs = Arc::new(MemJobQueue::new(clock.clone(), Arc::new(SeqIdGen::new())));
        let repos = Arc::new(MemRepoStore::new(clock.clone(), jobs.clone()));
        let knowledge = Arc::new(MemKnowledgeStore::new(clock.clone(), repos.clone()));
        let artifacts = Arc::new(MemArtifactStore::new(clock.clone()));
        World { clock, jobs, repos, knowledge, artifacts }
    }

    /// Repo `acme/{name}` of [`world_org`] on installation 7.
    pub async fn repo(&self, name: &str) -> Repo {
        let seed = 0x0c00 + name.bytes().fold(0u128, |h, b| h.wrapping_mul(31).wrapping_add(u128::from(b)));
        let repo = Repo {
            id: RepoId::from_uuid(Uuid::from_u128(seed)),
            org_id: world_org(),
            external_id: seed as u64,
            owner: "acme".into(),
            name: name.into(),
            default_branch: "main".into(),
            installation_id: InstallationId(7),
            access: RepoAccess::Granted,
            created_at: FakeClock::DEFAULT_START,
        };
        self.repos.upsert_repo(&repo, &[]).await.unwrap_or_else(|e| panic!("upsert_repo: {e}"))
    }

    /// A version at [`WORLD_REVS`] with `ready` layers `Ready`.
    pub async fn version(&self, repo: RepoId, sha: CommitSha, ready: &[Layer]) -> RepoVersionKey {
        let org = world_org();
        self.repos.add_version_reason(org, repo, sha, VersionReason::DefaultHead).await.unwrap_or_else(|e| panic!("add_version_reason: {e}"));
        let key = RepoVersionKey::new(repo, sha);
        self.repos.set_index_revs(org, key, &WORLD_REVS).await.unwrap_or_else(|e| panic!("set_index_revs: {e}"));
        for layer in ready {
            self.layer(key, *layer, LayerEvent::Start).await;
            self.layer(key, *layer, LayerEvent::Complete { at: FakeClock::DEFAULT_START }).await;
        }
        key
    }

    pub async fn layer(&self, key: RepoVersionKey, layer: Layer, event: LayerEvent) {
        self.repos.apply_layer_event(world_org(), key, layer, event, &[]).await.unwrap_or_else(|e| panic!("apply_layer_event: {e}"));
    }

    /// Manifest of `key` with blob metadata (line counts) for each file.
    pub async fn manifest(&self, key: &RepoVersionKey, files: &[WorldFile<'_>]) {
        let org = world_org();
        let entries: Vec<ManifestEntry> = files
            .iter()
            .map(|f| ManifestEntry {
                path: world_path(f.path),
                kind: EntryKind::File,
                executable: false,
                blob: f.blob,
                lang: f.lang,
                class: f.class,
                size: u64::from(f.lines) * 40,
                is_text: true,
                line_count: f.lines,
            })
            .collect();
        let metas: Vec<BlobMeta> =
            files.iter().map(|f| BlobMeta { oid: f.blob, size: u64::from(f.lines) * 40, line_count: f.lines, is_text: true, content_stored: true }).collect();
        self.knowledge.put_blob_meta(org, &metas).await.unwrap_or_else(|e| panic!("put_blob_meta: {e}"));
        self.knowledge.put_manifest(org, key, &entries).await.unwrap_or_else(|e| panic!("put_manifest: {e}"));
    }

    /// Parsed symbols/refs per blob at [`WORLD_REVS`].
    pub async fn parsed(&self, blobs: Vec<(BlobOid, ParsedBlob)>) {
        let blobs: Vec<(BlobOid, Result<ParsedBlob, String>)> = blobs.into_iter().map(|(oid, parsed)| (oid, Ok(parsed))).collect();
        self.knowledge.put_parsed(world_org(), WORLD_REVS.parser, &blobs).await.unwrap_or_else(|e| panic!("put_parsed: {e}"));
    }
}

impl Default for World {
    fn default() -> Self {
        Self::new()
    }
}
