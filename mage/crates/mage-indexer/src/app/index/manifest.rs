//! UC `index.manifest` (layer `Files`; knowledge.md "UC `index.manifest`", "Blob-hash caching"):
//! list the tree, store raw text blob contents and `blobs` rows once per org and oid, write the
//! sha's manifest, then plan `index.parse` batches over the blobs without a `Parse(rev)` marker.

use std::collections::{BTreeSet, HashMap, HashSet};

use mage_core::domain::{BlobOid, EntryKind, FileClass, Layer, LayerEvent, LayerState, ManifestEntry, RepoVersion, VersionReason};
use mage_core::ports::blob::{BlobKey, PutOptions};
use mage_core::ports::knowledge::{BlobMeta, BlobStage};

use super::layers::{Started, start, switch_rev};
use super::{IndexPipeline, IndexTarget, JobDone};
use crate::domain::files;
use crate::error::AppError;
use crate::ports::git::{MirrorRef, TreeEntryKind, TreeListing};

/// Text blobs up to this size get a content object (`content_stored`).
pub const BLOB_CONTENT_MAX_BYTES: u64 = 2 * 1024 * 1024;
/// Larger blobs are read once (not stored) to learn `is_text` / `line_count`, up to this size.
pub const BLOB_SNIFF_MAX_BYTES: u64 = 64 * 1024 * 1024;
/// Parse candidates are at most this large.
pub const PARSE_MAX_BLOB_BYTES: u64 = 1024 * 1024;
pub const PARSE_BATCH_MAX_BLOBS: usize = 256;
pub const PARSE_BATCH_MAX_BYTES: u64 = 8 * 1024 * 1024;
/// `head` handed to `Parser::detect_language`.
pub const LANGUAGE_HEAD_BYTES: usize = 512;
/// Bytes of a blob kept for language and class sniffing.
const HEAD_KEEP_BYTES: usize = files::BINARY_SNIFF_BYTES;
/// One `read_blobs` call reads at most this many blobs / bytes.
const READ_CHUNK_BLOBS: usize = 512;
const READ_CHUNK_BYTES: u64 = 32 * 1024 * 1024;
/// Oids per `blob_meta` / `missing` / `put_blob_meta` call.
const STORE_CHUNK: usize = 1000;

const CONTENT_TYPE: &str = "application/octet-stream";

impl IndexPipeline {
    pub async fn manifest(&self, t: IndexTarget) -> Result<JobDone, AppError> {
        let repos = self.deps.repos.as_ref();
        let (org, key, m) = (t.org, t.key(), t.mirror());
        let version = repos.version(org, key).await?;
        if start(repos, org, key, Layer::Files, version.layer(Layer::Files)).await? == Started::Closed {
            return Ok(JobDone::done());
        }

        let listing = self.deps.git.list_tree(&m, t.sha).await?;
        if listing.skipped_paths > 0 {
            tracing::warn!(org = %org, %key, skipped = listing.skipped_paths, "tree paths skipped (non-UTF-8 or invalid)");
        }
        let entries = self.build_entries(&t, &m, &listing).await?;
        self.deps.knowledge.put_manifest(org, &key, &entries).await?;
        let now = self.deps.clock.now();
        repos.apply_layer_event(org, key, Layer::Files, LayerEvent::Complete { at: now }, &[]).await?;

        self.plan_parse(&t, &entries).await?;
        self.warm_checkout(&t, &version).await;
        Ok(JobDone::done())
    }

    /// DefaultHead and Deployed versions get their search checkout right away; a failure only
    /// means the first search materializes it.
    async fn warm_checkout(&self, t: &IndexTarget, version: &RepoVersion) {
        let Some(live) = &self.checkouts else { return };
        let eager = version.reasons.iter().any(|r| matches!(r, VersionReason::DefaultHead | VersionReason::Deployed(_)));
        if !eager {
            return;
        }
        if let Err(err) = live.checkout(t.org, t.repo, t.sha).await {
            tracing::warn!(org = %t.org, sha = %t.sha, error = %err, "eager checkout failed");
        }
    }

    /// Steps 3-5: blob contents and metas (cached by oid), languages, classes.
    async fn build_entries(&self, t: &IndexTarget, m: &MirrorRef, listing: &TreeListing) -> Result<Vec<ManifestEntry>, AppError> {
        let parser = self.deps.parser.as_ref();
        let blob_oids: Vec<BlobOid> = listing
            .entries
            .iter()
            .filter_map(|e| match e.kind {
                TreeEntryKind::File { oid, .. } | TreeEntryKind::Symlink { oid } => Some(oid),
                TreeEntryKind::Submodule { .. } => None,
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut metas: HashMap<BlobOid, BlobMeta> = HashMap::with_capacity(blob_oids.len());
        for chunk in blob_oids.chunks(STORE_CHUNK) {
            metas.extend(self.deps.knowledge.blob_meta(t.org, chunk).await?.into_iter().map(|meta| (meta.oid, meta)));
        }

        // Bytes are needed for unknown blobs (meta + content) and for heads: shebang sniffing of
        // extension-less files, generated-file headers of source files.
        let mut need: Vec<(BlobOid, u64)> = Vec::new();
        let mut seen = HashSet::new();
        for entry in &listing.entries {
            let (oid, needs_head) = match entry.kind {
                TreeEntryKind::File { oid, .. } => {
                    let by_path = parser.detect_language(&entry.path, b"");
                    let needs_head = match by_path {
                        Some(lang) => parser.languages().contains(&lang),
                        None => entry.path.extension().is_none(),
                    };
                    (oid, needs_head)
                }
                TreeEntryKind::Symlink { oid } => (oid, false),
                TreeEntryKind::Submodule { .. } => continue,
            };
            if (needs_head || !metas.contains_key(&oid)) && seen.insert(oid) {
                need.push((oid, entry.size.unwrap_or(0)));
            }
        }

        let mut heads: HashMap<BlobOid, Vec<u8>> = HashMap::with_capacity(need.len());
        let mut new_metas: Vec<BlobMeta> = Vec::new();
        let mut oversized: Vec<BlobOid> = Vec::new();
        for chunk in read_chunks(&need) {
            for read in self.deps.git.read_blobs(m, &chunk, BLOB_CONTENT_MAX_BYTES).await? {
                let Some(bytes) = read.bytes else {
                    if !metas.contains_key(&read.oid) {
                        oversized.push(read.oid);
                    }
                    continue;
                };
                heads.insert(read.oid, bytes[..bytes.len().min(HEAD_KEEP_BYTES)].to_vec());
                if metas.contains_key(&read.oid) {
                    continue;
                }
                let is_text = files::is_text(&bytes);
                let line_count = if is_text { files::line_count(&bytes) } else { 0 };
                if is_text {
                    let opts = PutOptions { content_type: CONTENT_TYPE.to_owned(), if_absent: true };
                    self.deps.artifacts.put(&BlobKey::blob_content(t.org, read.oid), bytes, &opts).await?;
                }
                let meta = BlobMeta { oid: read.oid, size: read.size, line_count, is_text, content_stored: is_text };
                metas.insert(read.oid, meta);
                new_metas.push(meta);
            }
        }
        for oid in oversized {
            let read = self.deps.git.read_blobs(m, &[oid], BLOB_SNIFF_MAX_BYTES).await?.into_iter().next();
            let (size, is_text, line_count, head) = match read {
                Some(read) => match read.bytes {
                    Some(bytes) => {
                        let is_text = files::is_text(&bytes);
                        let lines = if is_text { files::line_count(&bytes) } else { 0 };
                        (read.size, is_text, lines, Some(bytes[..bytes.len().min(HEAD_KEEP_BYTES)].to_vec()))
                    }
                    None => (read.size, false, 0, None),
                },
                None => return Err(AppError::NotFound(format!("blob {oid}"))),
            };
            if let Some(head) = head {
                heads.insert(oid, head);
            }
            let meta = BlobMeta { oid, size, line_count, is_text, content_stored: false };
            metas.insert(oid, meta);
            new_metas.push(meta);
        }
        for chunk in new_metas.chunks(STORE_CHUNK) {
            self.deps.knowledge.put_blob_meta(t.org, chunk).await?;
        }
        tracing::debug!(org = %t.org, sha = %t.sha, blobs = blob_oids.len(), new = new_metas.len(), "blob contents cached");

        let mut entries = Vec::with_capacity(listing.entries.len());
        for entry in &listing.entries {
            let manifest_entry = match entry.kind {
                TreeEntryKind::File { oid, executable } => {
                    let meta = metas.get(&oid).copied().ok_or_else(|| AppError::Internal(format!("no blob meta for {oid}")))?;
                    let head = heads.get(&oid).map(Vec::as_slice);
                    let lang_head = head.map_or(&[][..], |h| &h[..h.len().min(LANGUAGE_HEAD_BYTES)]);
                    ManifestEntry {
                        path: entry.path.clone(),
                        kind: EntryKind::File,
                        executable,
                        blob: oid,
                        lang: parser.detect_language(&entry.path, lang_head),
                        class: files::classify(&entry.path, meta.is_text, head),
                        size: entry.size.unwrap_or(meta.size),
                        is_text: meta.is_text,
                        line_count: meta.line_count,
                    }
                }
                TreeEntryKind::Symlink { oid } => {
                    let meta = metas.get(&oid).copied().ok_or_else(|| AppError::Internal(format!("no blob meta for {oid}")))?;
                    ManifestEntry {
                        path: entry.path.clone(),
                        kind: EntryKind::Symlink,
                        executable: false,
                        blob: oid,
                        lang: None,
                        class: files::classify(&entry.path, meta.is_text, None),
                        size: entry.size.unwrap_or(meta.size),
                        is_text: meta.is_text,
                        line_count: meta.line_count,
                    }
                }
                TreeEntryKind::Submodule { commit } => ManifestEntry {
                    path: entry.path.clone(),
                    kind: EntryKind::Submodule,
                    executable: false,
                    blob: BlobOid::from_bytes(commit.0),
                    lang: None,
                    class: FileClass::Vendored,
                    size: 0,
                    is_text: false,
                    line_count: 0,
                },
            };
            entries.push(manifest_entry);
        }
        Ok(entries)
    }

    /// Steps 7-8: batches of blobs without a `Parse(rev)` marker, or the Symbols successors
    /// directly when nothing needs parsing.
    async fn plan_parse(&self, t: &IndexTarget, entries: &[ManifestEntry]) -> Result<(), AppError> {
        let repos = self.deps.repos.as_ref();
        let (org, key) = (t.org, t.key());
        let rev = self.deps.parser.rev();
        let version = repos.version(org, key).await?;
        match version.layer(Layer::Symbols) {
            // Batches were begun by an earlier attempt of this job; their jobs are queued.
            Some(LayerState::Running | LayerState::Failed { .. } | LayerState::Skipped { .. }) => return Ok(()),
            Some(LayerState::Ready { .. }) if version.revs.map(|r| r.parser) == Some(rev) => return Ok(()),
            Some(LayerState::Ready { .. } | LayerState::Pending) | None => {}
        }

        let candidates = parse_candidates(entries, self.deps.parser.languages());
        let oids: Vec<BlobOid> = candidates.iter().map(|(oid, _)| *oid).collect();
        let mut missing: HashSet<BlobOid> = HashSet::with_capacity(oids.len());
        for chunk in oids.chunks(STORE_CHUNK) {
            missing.extend(self.deps.knowledge.missing(org, BlobStage::Parse(rev), chunk).await?);
        }
        let todo: Vec<(BlobOid, u64)> = candidates.into_iter().filter(|(oid, _)| missing.contains(oid)).collect();
        tracing::info!(org = %org, %key, candidates = oids.len(), to_parse = todo.len(), "parse plan (blob cache hits = candidates - to_parse)");

        let batches = parse_batches(&todo);
        if batches.is_empty() {
            if matches!(version.layer(Layer::Symbols), None | Some(LayerState::Pending)) {
                repos.apply_layer_event(org, key, Layer::Symbols, LayerEvent::Start, &[]).await?;
            }
            let now = self.deps.clock.now();
            repos.apply_layer_event(org, key, Layer::Symbols, LayerEvent::Complete { at: now }, &t.after_symbols()).await?;
            switch_rev(repos, org, key, self.current_revs(), |revs| revs.parser = rev).await?;
            return Ok(());
        }
        let n = u32::try_from(batches.len()).map_err(|_| AppError::Invalid("too many parse batches".into()))?;
        let jobs: Vec<_> = batches.into_iter().zip(1..).map(|(oids, batch)| t.parse_job(batch, oids)).collect();
        repos.begin_layer_batches(org, key, Layer::Symbols, n, &jobs).await?;
        Ok(())
    }
}

/// Step 7 candidates in path order, one per oid (first path wins): files in a parsed language,
/// class Source or Test, at most [`PARSE_MAX_BLOB_BYTES`].
pub fn parse_candidates(entries: &[ManifestEntry], languages: &[mage_core::domain::Language]) -> Vec<(BlobOid, u64)> {
    let mut seen = HashSet::new();
    entries
        .iter()
        .filter(|e| {
            e.kind == EntryKind::File
                && e.lang.is_some_and(|lang| languages.contains(&lang))
                && matches!(e.class, FileClass::Source | FileClass::Test)
                && e.size <= PARSE_MAX_BLOB_BYTES
        })
        .filter(|e| seen.insert(e.blob))
        .map(|e| (e.blob, e.size))
        .collect()
}

/// Consecutive batches of at most [`PARSE_BATCH_MAX_BLOBS`] blobs and [`PARSE_BATCH_MAX_BYTES`]
/// (a single larger blob still gets its own batch).
pub fn parse_batches(todo: &[(BlobOid, u64)]) -> Vec<Vec<BlobOid>> {
    let mut batches: Vec<Vec<BlobOid>> = Vec::new();
    let mut bytes = 0u64;
    for (oid, size) in todo {
        let full = batches.last().is_none_or(|b| b.len() >= PARSE_BATCH_MAX_BLOBS || (!b.is_empty() && bytes + size > PARSE_BATCH_MAX_BYTES));
        if full {
            batches.push(Vec::new());
            bytes = 0;
        }
        if let Some(batch) = batches.last_mut() {
            batch.push(*oid);
            bytes += size;
        }
    }
    batches
}

/// `read_blobs` chunks bounded by blob count and listed size.
fn read_chunks(need: &[(BlobOid, u64)]) -> Vec<Vec<BlobOid>> {
    let mut chunks: Vec<Vec<BlobOid>> = Vec::new();
    let mut bytes = 0u64;
    for (oid, size) in need {
        let size = (*size).min(BLOB_CONTENT_MAX_BYTES);
        let full = chunks.last().is_none_or(|c| c.len() >= READ_CHUNK_BLOBS || (!c.is_empty() && bytes + size > READ_CHUNK_BYTES));
        if full {
            chunks.push(Vec::new());
            bytes = 0;
        }
        if let Some(chunk) = chunks.last_mut() {
            chunk.push(*oid);
            bytes += size;
        }
    }
    chunks
}

#[cfg(test)]
mod tests {
    use mage_core::domain::{Language, RepoPath};

    use super::*;

    fn oid(n: u8) -> BlobOid {
        BlobOid::from_bytes([n; 20])
    }

    fn entry(path: &str, blob: u8, lang: Option<Language>, class: FileClass, size: u64) -> ManifestEntry {
        ManifestEntry {
            path: RepoPath::new(path).unwrap(),
            kind: EntryKind::File,
            executable: false,
            blob: oid(blob),
            lang,
            class,
            size,
            is_text: true,
            line_count: 1,
        }
    }

    #[test]
    fn candidates_are_source_or_test_files_in_parsed_languages_once_per_blob() {
        let langs = [Language::Go, Language::TypeScript];
        let entries = vec![
            entry("a.go", 1, Some(Language::Go), FileClass::Source, 10),
            entry("a_test.go", 2, Some(Language::Go), FileClass::Test, 10),
            entry("b/a.go", 1, Some(Language::Go), FileClass::Source, 10),
            entry("gen.pb.go", 3, Some(Language::Go), FileClass::Generated, 10),
            entry("vendor/x.go", 4, Some(Language::Go), FileClass::Vendored, 10),
            entry("big.go", 5, Some(Language::Go), FileClass::Source, PARSE_MAX_BLOB_BYTES + 1),
            entry("edge.go", 6, Some(Language::Go), FileClass::Source, PARSE_MAX_BLOB_BYTES),
            entry("README.md", 7, Some(Language::Markdown), FileClass::Doc, 10),
            entry("x.py", 8, Some(Language::Python), FileClass::Source, 10),
            entry("y.ts", 9, Some(Language::TypeScript), FileClass::Source, 10),
        ];
        let got: Vec<BlobOid> = parse_candidates(&entries, &langs).into_iter().map(|(o, _)| o).collect();
        assert_eq!(got, vec![oid(1), oid(2), oid(6), oid(9)]);
    }

    #[test]
    fn batches_respect_count_and_byte_caps_in_order() {
        let todo: Vec<(BlobOid, u64)> = (0..600u32).map(|i| (BlobOid::from_bytes([(i % 251) as u8; 20]), 10)).collect();
        let batches = parse_batches(&todo);
        assert_eq!(batches.iter().map(Vec::len).collect::<Vec<_>>(), vec![256, 256, 88]);

        let mib = 1024 * 1024;
        let todo = vec![(oid(1), 5 * mib), (oid(2), 3 * mib), (oid(3), 1), (oid(4), mib)];
        let batches = parse_batches(&todo);
        assert_eq!(batches, vec![vec![oid(1), oid(2)], vec![oid(3), oid(4)]]);

        assert!(parse_batches(&[]).is_empty());
        // A blob over the byte cap on its own is still one batch.
        assert_eq!(parse_batches(&[(oid(1), 9 * mib), (oid(2), 1)]), vec![vec![oid(1)], vec![oid(2)]]);
    }
}
