//! UC `index.error_sites` (layer `ErrorSites`; knowledge.md "Layer 3: Error sites"): classify the
//! literal sites of every parsed source blob without an `ErrorSites(rev)` marker, parse the i18n
//! catalogs, and write both with their per-blob markers.

use std::collections::{BTreeMap, BTreeSet};

use mage_core::domain::{BlobOid, EntryKind, FileClass, Language, Layer, LayerEvent, ManifestEntry, OrgId, RepoVersionKey};
use mage_core::ports::knowledge::{BlobInput, BlobStage, CatalogEntryRecord, PageReq};

use super::errors::{ERRORS_REV, catalog_records, error_site_records};
use super::layers::{Started, start, switch_rev};
use super::manifest::{BLOB_CONTENT_MAX_BYTES, parse_candidates};
use super::{IndexPipeline, IndexTarget, JobDone};
use crate::domain::files;
use crate::error::AppError;

/// Oids per `literal_sites` + `put_error_sites` round.
pub const ERROR_SITES_CHUNK: usize = 1000;
/// Catalog blobs per `read_blobs` round.
const CATALOG_CHUNK: usize = 64;
/// Manifest page size when listing the version's files.
const MANIFEST_PAGE: u32 = 1000;

impl IndexPipeline {
    pub async fn error_sites(&self, t: IndexTarget) -> Result<JobDone, AppError> {
        let repos = self.deps.repos.as_ref();
        let knowledge = self.deps.knowledge.as_ref();
        let (org, key) = (t.org, t.key());
        let version = repos.version(org, key).await?;
        if start(repos, org, key, Layer::ErrorSites, version.layer(Layer::ErrorSites)).await? == Started::Closed {
            return Ok(JobDone::done());
        }
        let parser_rev = version.revs.map_or_else(|| self.deps.parser.rev(), |revs| revs.parser);
        let entries = self.manifest_all(org, &key).await?;

        // Source blobs: those with a usable Parse marker come back from literal_sites.
        let sources: Vec<BlobOid> = parse_candidates(&entries, self.deps.parser.languages()).into_iter().map(|(oid, _)| oid).collect();
        let mut todo = BTreeSet::new();
        for chunk in sources.chunks(ERROR_SITES_CHUNK) {
            todo.extend(knowledge.missing(org, BlobStage::ErrorSites(ERRORS_REV), chunk).await?);
        }
        let todo: Vec<BlobOid> = todo.into_iter().collect();
        let mut sites = 0usize;
        for chunk in todo.chunks(ERROR_SITES_CHUNK) {
            let parsed = knowledge.literal_sites(org, parser_rev, chunk).await?;
            let blobs: Vec<_> = parsed.into_iter().map(|(oid, lang, literals)| (oid, error_site_records(lang, &literals))).collect();
            sites += blobs.iter().map(|(_, records)| records.len()).sum::<usize>();
            knowledge.put_error_sites(org, ERRORS_REV, &blobs).await?;
        }

        let catalogs = self.catalogs(&t, &entries).await?;
        let now = self.deps.clock.now();
        repos.apply_layer_event(org, key, Layer::ErrorSites, LayerEvent::Complete { at: now }, &[]).await?;
        switch_rev(repos, org, key, self.current_revs(), |revs| revs.errors = ERRORS_REV).await?;
        tracing::info!(org = %org, %key, blobs = todo.len(), sites, catalogs, "error sites indexed");
        Ok(JobDone::done())
    }

    /// Step 3: catalog blobs without a marker → `Parser::catalog` → `put_catalog` (a blob that is
    /// not a catalog, or fails to parse, still gets its marker). Returns the blobs written.
    async fn catalogs(&self, t: &IndexTarget, entries: &[ManifestEntry]) -> Result<usize, AppError> {
        let knowledge = self.deps.knowledge.as_ref();
        let mut paths: BTreeMap<BlobOid, &ManifestEntry> = BTreeMap::new();
        for entry in entries {
            let eligible = entry.kind == EntryKind::File
                && entry.is_text
                && entry.size <= BLOB_CONTENT_MAX_BYTES
                && !matches!(entry.class, FileClass::Vendored | FileClass::Generated)
                && files::is_catalog_path(&entry.path);
            if eligible {
                paths.entry(entry.blob).or_insert(entry);
            }
        }
        let oids: Vec<BlobOid> = paths.keys().copied().collect();
        let mut todo = Vec::new();
        for chunk in oids.chunks(ERROR_SITES_CHUNK) {
            todo.extend(knowledge.missing(t.org, BlobStage::ErrorSites(ERRORS_REV), chunk).await?);
        }
        for chunk in todo.chunks(CATALOG_CHUNK) {
            let reads = self.deps.git.read_blobs(&t.mirror(), chunk, BLOB_CONTENT_MAX_BYTES).await?;
            let mut blobs: Vec<(BlobOid, Vec<CatalogEntryRecord>)> = Vec::with_capacity(reads.len());
            for read in reads {
                let Some(entry) = paths.get(&read.oid) else { continue };
                let records = match read.bytes {
                    Some(bytes) => {
                        let lang = entry.lang.unwrap_or(Language::Other);
                        match self.deps.parser.catalog(BlobInput { oid: read.oid, path: entry.path.clone(), lang, bytes }).await {
                            Ok(Some(catalog)) => catalog_records(&catalog),
                            Ok(None) => Vec::new(),
                            Err(err) => {
                                tracing::warn!(org = %t.org, path = %entry.path, error = %err, "catalog not parsed");
                                Vec::new()
                            }
                        }
                    }
                    None => Vec::new(),
                };
                blobs.push((read.oid, records));
            }
            knowledge.put_catalog(t.org, ERRORS_REV, &blobs).await?;
        }
        Ok(todo.len())
    }

    /// Every manifest entry of the version, in path order.
    pub(crate) async fn manifest_all(&self, org: OrgId, key: &RepoVersionKey) -> Result<Vec<ManifestEntry>, AppError> {
        let mut out = Vec::new();
        let mut page = PageReq::first(MANIFEST_PAGE);
        loop {
            let got = self.deps.knowledge.manifest_all(org, key, &page).await?;
            out.extend(got.items);
            match got.next {
                Some(next) => page.after = Some(next),
                None => return Ok(out),
            }
        }
    }
}
