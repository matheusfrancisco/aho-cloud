//! `PgKnowledgeStore`: the Postgres `KnowledgeStore` (knowledge.md "`mage_infra::pg`
//! (KnowledgeStore)", storage.md "Knowledge"). One module per layer:
//!
//! | Module | Contents |
//! |---|---|
//! | [`files`] | manifests, blob rows, stage lookups, path reads, stats, manifest diffs |
//! | [`symbols`] | parser output, symbol and name-ref reads |
//! | [`scip`] | SCIP index ingest and reads |
//! | [`calls`] | `call_edges` by name or from SCIP |
//! | [`errors`] | error sites, catalogs, matching |
//! | [`appmap`] | app facts and the per-version app graph |
//! | [`glossary`] | glossary hits and top terms |
//! | [`history`] | commits, pull requests, ticket links, history cursor |
//! | [`gc`] | version fact deletion, unreferenced blob sweep |
//!
//! Every method runs in one tenant transaction (`begin_org`: RLS on `current_org()`) and filters
//! `org_id = $1` in every statement; writes rerun on serialization failures (`retry_tx`). A
//! version's view joins `manifests` to blob-keyed facts at `repo_versions.revs`, read in the same
//! transaction. Bulk writes go through `INSERT … SELECT … FROM UNNEST(…)` in chunks of
//! [`codec::CHUNK`] rows; write-once facts use `ON CONFLICT DO NOTHING`.

mod appmap;
mod calls;
mod codec;
mod errors;
mod files;
mod gc;
mod glossary;
mod history;
mod scip;
mod symbols;
mod view;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mage_core::domain::ids::{ArtifactId, BlobOid, CommitSha, OrgId, RepoId, RepoVersionKey};
use mage_core::domain::path::{Language, RepoPath};
use mage_core::ports::clock::Clock;
use mage_core::ports::ids::IdGen;
use mage_core::ports::knowledge::{
    AppFactAt, AppFactRecord, AppGraph, AppMapRev, AppNode, AppNodeKind, BlobMeta, BlobStage, CallDirection, CallEdge, CatalogEntryRecord,
    CatalogMatch, CommitRecord, DirEntry, ErrorMatch, ErrorQuery, ErrorSiteRecord, ErrorsRev, GlossaryHit, HistoryCursor, KnowledgeError,
    KnowledgeStore, LinkLookup, LiteralSite, ManifestChange, ManifestEntry, NameRefQuery, PageReq, Paged, ParsedBlob, ParserRev,
    PathMatch, Precision, PullRequestRecord, RefHit, ScipDocRows, ScipIndexInfo, ScipMetadata, ScipSymbolHit, ScipSymbolInfo, SymbolHit,
    SymbolQuery, SymbolRef, TicketLink, VersionStats,
};
use sqlx::PgConnection;

use self::codec::{Page, to_rev};
use crate::pg::error::DbError;
use crate::pg::pool::PgPools;
use crate::pg::tx::{Tx, begin_org, retry_tx};

pub struct PgKnowledgeStore {
    pools: PgPools,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGen>,
    patterns: errors::PatternCache,
}

impl PgKnowledgeStore {
    /// `ids` mints `scip_indexes.scip_index_id`.
    pub fn new(pools: PgPools, clock: Arc<dyn Clock>, ids: Arc<dyn IdGen>) -> Self {
        Self { pools, clock, ids, patterns: errors::PatternCache::default() }
    }

    async fn begin(&self, org: OrgId) -> Result<Tx, DbError> {
        begin_org(&self.pools.tenant, org).await
    }
}

/// Commit a read transaction after its body succeeded; roll back otherwise.
async fn finish<T>(tx: Tx, out: Result<T, DbError>) -> Result<T, KnowledgeError> {
    match out {
        Ok(value) => {
            tx.commit().await.map_err(DbError::from)?;
            Ok(value)
        }
        Err(err) => {
            // The body's error is the one to report; a failed rollback ends with the connection.
            let _ = tx.rollback().await;
            Err(err.into())
        }
    }
}

/// The version's parser rev (`None` without revs: no blob-keyed facts are visible).
async fn parser_rev(conn: &mut PgConnection, org: OrgId, key: &RepoVersionKey) -> Result<Option<i16>, DbError> {
    view::revs(conn, org, key).await?.map(|revs| to_rev(revs.parser.0)).transpose()
}

fn empty_page<T>() -> Paged<T> {
    Paged { items: Vec::new(), next: None }
}

#[async_trait]
impl KnowledgeStore for PgKnowledgeStore {
    async fn put_manifest(&self, org: OrgId, key: &RepoVersionKey, entries: &[ManifestEntry]) -> Result<(), KnowledgeError> {
        let mut paths = HashSet::with_capacity(entries.len());
        if let Some(dup) = entries.iter().find(|entry| !paths.insert(entry.path.as_str())) {
            return Err(KnowledgeError::Invalid(format!("duplicate manifest path {}", dup.path)));
        }
        retry_tx(|| async {
            let mut tx = self.begin(org).await?;
            files::put_manifest(&mut tx, org, key, entries).await?;
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    async fn put_blob_meta(&self, org: OrgId, metas: &[BlobMeta]) -> Result<(), KnowledgeError> {
        let now = self.clock.now();
        retry_tx(|| async {
            let mut tx = self.begin(org).await?;
            files::put_blob_meta(&mut tx, org, metas, now).await?;
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    async fn blob_meta(&self, org: OrgId, oids: &[BlobOid]) -> Result<Vec<BlobMeta>, KnowledgeError> {
        let mut tx = self.begin(org).await?;
        let out = files::blob_meta(&mut tx, org, oids).await;
        finish(tx, out).await
    }

    async fn missing(&self, org: OrgId, stage: BlobStage, oids: &[BlobOid]) -> Result<Vec<BlobOid>, KnowledgeError> {
        let mut tx = self.begin(org).await?;
        let out = files::missing(&mut tx, org, stage, oids).await;
        finish(tx, out).await
    }

    async fn put_parsed(&self, org: OrgId, rev: ParserRev, blobs: &[(BlobOid, Result<ParsedBlob, String>)]) -> Result<(), KnowledgeError> {
        let now = self.clock.now();
        retry_tx(|| async {
            let mut tx = self.begin(org).await?;
            symbols::put_parsed(&mut tx, org, rev, blobs, now).await?;
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    async fn literal_sites(
        &self,
        org: OrgId,
        rev: ParserRev,
        oids: &[BlobOid],
    ) -> Result<Vec<(BlobOid, Language, Vec<LiteralSite>)>, KnowledgeError> {
        let mut tx = self.begin(org).await?;
        let out = symbols::literal_sites(&mut tx, org, rev, oids).await;
        finish(tx, out).await
    }

    async fn put_error_sites(&self, org: OrgId, rev: ErrorsRev, blobs: &[(BlobOid, Vec<ErrorSiteRecord>)]) -> Result<(), KnowledgeError> {
        let now = self.clock.now();
        retry_tx(|| async {
            let mut tx = self.begin(org).await?;
            errors::put_error_sites(&mut tx, org, rev, blobs, now).await?;
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    async fn put_catalog(&self, org: OrgId, rev: ErrorsRev, blobs: &[(BlobOid, Vec<CatalogEntryRecord>)]) -> Result<(), KnowledgeError> {
        let now = self.clock.now();
        retry_tx(|| async {
            let mut tx = self.begin(org).await?;
            errors::put_catalog(&mut tx, org, rev, blobs, now).await?;
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    async fn put_app_facts(&self, org: OrgId, rev: AppMapRev, blobs: &[(BlobOid, Vec<AppFactRecord>)]) -> Result<(), KnowledgeError> {
        let now = self.clock.now();
        retry_tx(|| async {
            let mut tx = self.begin(org).await?;
            appmap::put_app_facts(&mut tx, org, rev, blobs, now).await?;
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    async fn app_facts_at(&self, org: OrgId, key: &RepoVersionKey, rev: AppMapRev) -> Result<Vec<AppFactAt>, KnowledgeError> {
        let mut tx = self.begin(org).await?;
        let out = appmap::app_facts_at(&mut tx, org, key, rev).await;
        finish(tx, out).await
    }

    async fn replace_app_graph(&self, org: OrgId, key: &RepoVersionKey, graph: &AppGraph) -> Result<(), KnowledgeError> {
        let edges = appmap::checked_edges(graph)?;
        retry_tx(|| async {
            let mut tx = self.begin(org).await?;
            appmap::replace_app_graph(&mut tx, org, key, &graph.nodes, &edges).await?;
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    async fn begin_scip_index(
        &self,
        org: OrgId,
        key: &RepoVersionKey,
        artifact: ArtifactId,
        meta: &ScipMetadata,
    ) -> Result<(), KnowledgeError> {
        let (id, now) = (self.ids.uuid_v7(), self.clock.now());
        retry_tx(|| async {
            let mut tx = self.begin(org).await?;
            scip::begin(&mut tx, org, key, artifact, meta, id, now).await?;
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    async fn put_scip_documents(&self, org: OrgId, artifact: ArtifactId, docs: &[ScipDocRows]) -> Result<(), KnowledgeError> {
        retry_tx(|| async {
            let mut tx = self.begin(org).await?;
            scip::put_documents(&mut tx, org, artifact, docs).await?;
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    async fn put_scip_symbols(&self, org: OrgId, artifact: ArtifactId, symbols: &[ScipSymbolInfo]) -> Result<(), KnowledgeError> {
        retry_tx(|| async {
            let mut tx = self.begin(org).await?;
            scip::put_symbols(&mut tx, org, artifact, symbols).await?;
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    async fn finish_scip_index(&self, org: OrgId, artifact: ArtifactId, languages: &[String], documents: u32) -> Result<(), KnowledgeError> {
        retry_tx(|| async {
            let mut tx = self.begin(org).await?;
            scip::finish(&mut tx, org, artifact, languages, documents).await?;
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    async fn put_commits(&self, org: OrgId, repo: RepoId, commits: &[CommitRecord]) -> Result<(), KnowledgeError> {
        retry_tx(|| async {
            let mut tx = self.begin(org).await?;
            history::put_commits(&mut tx, org, repo, commits).await?;
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    async fn put_pull_requests(&self, org: OrgId, repo: RepoId, prs: &[PullRequestRecord]) -> Result<(), KnowledgeError> {
        retry_tx(|| async {
            let mut tx = self.begin(org).await?;
            history::put_pull_requests(&mut tx, org, repo, prs).await?;
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    async fn put_ticket_links(&self, org: OrgId, repo: RepoId, links: &[TicketLink]) -> Result<(), KnowledgeError> {
        retry_tx(|| async {
            let mut tx = self.begin(org).await?;
            history::put_ticket_links(&mut tx, org, repo, links).await?;
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    async fn history_cursor(&self, org: OrgId, repo: RepoId) -> Result<HistoryCursor, KnowledgeError> {
        let mut tx = self.begin(org).await?;
        let out = history::history_cursor(&mut tx, org, repo).await;
        finish(tx, out).await
    }

    async fn merge_history_cursor(&self, org: OrgId, repo: RepoId, cursor: &HistoryCursor) -> Result<(), KnowledgeError> {
        let now = self.clock.now();
        retry_tx(|| async {
            let mut tx = self.begin(org).await?;
            history::merge_history_cursor(&mut tx, org, repo, cursor, now).await?;
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    async fn manifest_entries(&self, org: OrgId, key: &RepoVersionKey, paths: &[RepoPath]) -> Result<Vec<ManifestEntry>, KnowledgeError> {
        let mut tx = self.begin(org).await?;
        let out = files::manifest_entries(&mut tx, org, key, paths).await;
        finish(tx, out).await
    }

    async fn manifest_all(&self, org: OrgId, key: &RepoVersionKey, page: &PageReq) -> Result<Paged<ManifestEntry>, KnowledgeError> {
        let page = Page::new(page)?;
        let mut tx = self.begin(org).await?;
        let out = files::manifest_all(&mut tx, org, key, &page).await;
        finish(tx, out).await
    }

    async fn list_dir(
        &self,
        org: OrgId,
        key: &RepoVersionKey,
        dir: Option<&RepoPath>,
        page: &PageReq,
    ) -> Result<Paged<DirEntry>, KnowledgeError> {
        let page = Page::new(page)?;
        let mut tx = self.begin(org).await?;
        let out = files::list_dir(&mut tx, org, key, dir, &page).await;
        finish(tx, out).await
    }

    async fn find_paths(
        &self,
        org: OrgId,
        key: &RepoVersionKey,
        m: &PathMatch,
        page: &PageReq,
    ) -> Result<Paged<ManifestEntry>, KnowledgeError> {
        let page = Page::new(page)?;
        let matcher = files::path_matcher(m)?;
        let mut tx = self.begin(org).await?;
        let out = files::find_paths(&mut tx, org, key, &matcher, &page).await;
        finish(tx, out).await
    }

    async fn paths_by_basename(
        &self,
        org: OrgId,
        key: &RepoVersionKey,
        basename: &str,
        limit: u16,
    ) -> Result<Vec<RepoPath>, KnowledgeError> {
        let mut tx = self.begin(org).await?;
        let out = files::paths_by_basename(&mut tx, org, key, basename, limit).await;
        finish(tx, out).await
    }

    async fn version_stats(&self, org: OrgId, key: &RepoVersionKey) -> Result<VersionStats, KnowledgeError> {
        let mut tx = self.begin(org).await?;
        let out = async {
            let revs = view::revs(&mut tx, org, key).await?;
            files::version_stats(&mut tx, org, key, revs).await
        }
        .await;
        finish(tx, out).await
    }

    async fn find_symbols(
        &self,
        org: OrgId,
        key: &RepoVersionKey,
        q: &SymbolQuery,
        page: &PageReq,
    ) -> Result<Paged<SymbolHit>, KnowledgeError> {
        let page = Page::new(page)?;
        let mut tx = self.begin(org).await?;
        let out = async {
            let Some(parser) = parser_rev(&mut tx, org, key).await? else { return Ok(empty_page()) };
            symbols::find_symbols(&mut tx, org, key, parser, q, &page).await
        }
        .await;
        finish(tx, out).await
    }

    async fn symbols_at(&self, org: OrgId, key: &RepoVersionKey, path: &RepoPath, line: u32) -> Result<Vec<SymbolHit>, KnowledgeError> {
        let mut tx = self.begin(org).await?;
        let out = async {
            let Some(parser) = parser_rev(&mut tx, org, key).await? else { return Ok(Vec::new()) };
            symbols::symbols_at(&mut tx, org, key, parser, path, line).await
        }
        .await;
        finish(tx, out).await
    }

    async fn symbols_in_file(&self, org: OrgId, key: &RepoVersionKey, path: &RepoPath) -> Result<Vec<SymbolHit>, KnowledgeError> {
        let mut tx = self.begin(org).await?;
        let out = async {
            let Some(parser) = parser_rev(&mut tx, org, key).await? else { return Ok(Vec::new()) };
            symbols::symbols_in_file(&mut tx, org, key, parser, path).await
        }
        .await;
        finish(tx, out).await
    }

    async fn symbols_by_ref(&self, org: OrgId, key: &RepoVersionKey, refs: &[SymbolRef]) -> Result<Vec<SymbolHit>, KnowledgeError> {
        let mut tx = self.begin(org).await?;
        let out = async {
            let Some(parser) = parser_rev(&mut tx, org, key).await? else { return Ok(Vec::new()) };
            symbols::symbols_by_ref(&mut tx, org, key, parser, refs).await
        }
        .await;
        finish(tx, out).await
    }

    async fn name_refs(&self, org: OrgId, key: &RepoVersionKey, q: &NameRefQuery, page: &PageReq) -> Result<Paged<RefHit>, KnowledgeError> {
        let page = Page::new(page)?;
        let mut tx = self.begin(org).await?;
        let out = async {
            let Some(parser) = parser_rev(&mut tx, org, key).await? else { return Ok(empty_page()) };
            symbols::name_refs(&mut tx, org, key, parser, q, &page).await
        }
        .await;
        finish(tx, out).await
    }

    async fn call_edges(
        &self,
        org: OrgId,
        key: &RepoVersionKey,
        from: &[SymbolRef],
        dir: CallDirection,
        precision: &Precision,
        limit: u32,
    ) -> Result<Vec<CallEdge>, KnowledgeError> {
        let mut tx = self.begin(org).await?;
        let out = calls::call_edges(&mut tx, org, key, from, dir, precision, limit).await;
        finish(tx, out).await
    }

    async fn scip_index_for(&self, org: OrgId, key: &RepoVersionKey) -> Result<Option<ScipIndexInfo>, KnowledgeError> {
        let mut tx = self.begin(org).await?;
        let out = scip::index_for(&mut tx, org, key).await;
        finish(tx, out).await
    }

    async fn scip_symbol_at(
        &self,
        org: OrgId,
        index: ArtifactId,
        path: &RepoPath,
        line: u32,
        col: Option<u32>,
    ) -> Result<Option<ScipSymbolHit>, KnowledgeError> {
        let mut tx = self.begin(org).await?;
        let out = scip::symbol_at(&mut tx, org, index, path, line, col).await;
        finish(tx, out).await
    }

    async fn scip_occurrences(
        &self,
        org: OrgId,
        index: ArtifactId,
        key: &RepoVersionKey,
        symbol: &str,
        defs: bool,
        page: &PageReq,
    ) -> Result<Paged<RefHit>, KnowledgeError> {
        let page = Page::new(page)?;
        let mut tx = self.begin(org).await?;
        let out = async {
            let parser = parser_rev(&mut tx, org, key).await?;
            scip::occurrences(&mut tx, org, index, key, parser, symbol, defs, &page).await
        }
        .await;
        finish(tx, out).await
    }

    async fn match_errors(
        &self,
        org: OrgId,
        key: &RepoVersionKey,
        rev: ErrorsRev,
        q: &ErrorQuery,
    ) -> Result<Vec<ErrorMatch>, KnowledgeError> {
        let mut tx = self.begin(org).await?;
        let out = errors::match_errors(&mut tx, org, key, rev, q, &self.patterns).await;
        finish(tx, out).await
    }

    async fn match_catalog(
        &self,
        org: OrgId,
        key: &RepoVersionKey,
        rev: ErrorsRev,
        q: &ErrorQuery,
    ) -> Result<Vec<CatalogMatch>, KnowledgeError> {
        let mut tx = self.begin(org).await?;
        let out = errors::match_catalog(&mut tx, org, key, rev, q, &self.patterns).await;
        finish(tx, out).await
    }

    async fn sites_for_i18n_key(
        &self,
        org: OrgId,
        key: &RepoVersionKey,
        rev: ErrorsRev,
        i18n_key: &str,
        page: &PageReq,
    ) -> Result<Paged<ErrorMatch>, KnowledgeError> {
        let page = Page::new(page)?;
        let mut tx = self.begin(org).await?;
        let out = errors::sites_for_i18n_key(&mut tx, org, key, rev, i18n_key, &page).await;
        finish(tx, out).await
    }

    async fn error_sites_in(
        &self,
        org: OrgId,
        key: &RepoVersionKey,
        rev: ErrorsRev,
        symbol: &SymbolRef,
        limit: u32,
    ) -> Result<Vec<ErrorMatch>, KnowledgeError> {
        let mut tx = self.begin(org).await?;
        let out = async {
            let Some(parser) = parser_rev(&mut tx, org, key).await? else { return Ok(Vec::new()) };
            errors::error_sites_in(&mut tx, org, key, parser, rev, symbol, limit).await
        }
        .await;
        finish(tx, out).await
    }

    async fn app_nodes(
        &self,
        org: OrgId,
        key: &RepoVersionKey,
        kinds: &[AppNodeKind],
        page: &PageReq,
    ) -> Result<Paged<AppNode>, KnowledgeError> {
        let page = Page::new(page)?;
        let mut tx = self.begin(org).await?;
        let out = appmap::app_nodes(&mut tx, org, key, kinds, &page).await;
        finish(tx, out).await
    }

    async fn app_neighborhood(
        &self,
        org: OrgId,
        key: &RepoVersionKey,
        focus: &[u32],
        depth: u8,
        max_nodes: u32,
    ) -> Result<AppGraph, KnowledgeError> {
        let mut tx = self.begin(org).await?;
        let out = appmap::app_neighborhood(&mut tx, org, key, focus, depth, max_nodes).await;
        finish(tx, out).await
    }

    async fn app_nodes_for_symbols(&self, org: OrgId, key: &RepoVersionKey, symbols: &[SymbolRef]) -> Result<Vec<AppNode>, KnowledgeError> {
        let mut tx = self.begin(org).await?;
        let out = appmap::app_nodes_for_symbols(&mut tx, org, key, symbols).await;
        finish(tx, out).await
    }

    async fn glossary_hits(
        &self,
        org: OrgId,
        key: &RepoVersionKey,
        words: &[String],
        limit: u32,
    ) -> Result<Vec<GlossaryHit>, KnowledgeError> {
        if words.is_empty() {
            return Ok(Vec::new());
        }
        let mut tx = self.begin(org).await?;
        let out = async {
            let revs = view::revs(&mut tx, org, key).await?;
            glossary::glossary_hits(&mut tx, org, key, revs, words, limit).await
        }
        .await;
        finish(tx, out).await
    }

    async fn glossary_top(&self, org: OrgId, key: &RepoVersionKey, limit: u32) -> Result<Vec<GlossaryHit>, KnowledgeError> {
        let mut tx = self.begin(org).await?;
        let out = async {
            let revs = view::revs(&mut tx, org, key).await?;
            glossary::glossary_top(&mut tx, org, key, revs, limit).await
        }
        .await;
        finish(tx, out).await
    }

    async fn manifest_diff(
        &self,
        org: OrgId,
        from: &RepoVersionKey,
        to: &RepoVersionKey,
        prefix: Option<&RepoPath>,
        page: &PageReq,
    ) -> Result<Paged<ManifestChange>, KnowledgeError> {
        let page = Page::new(page)?;
        if from.repo_id != to.repo_id {
            return Err(KnowledgeError::Invalid("manifest_diff across repos".into()));
        }
        let mut tx = self.begin(org).await?;
        let out = files::manifest_diff(&mut tx, org, from, to, prefix, &page).await;
        finish(tx, out).await
    }

    async fn has_commit(&self, org: OrgId, repo: RepoId, sha: CommitSha) -> Result<bool, KnowledgeError> {
        let mut tx = self.begin(org).await?;
        let out = history::has_commit(&mut tx, org, repo, sha).await;
        finish(tx, out).await
    }

    async fn commits(&self, org: OrgId, repo: RepoId, shas: &[CommitSha]) -> Result<Vec<CommitRecord>, KnowledgeError> {
        let mut tx = self.begin(org).await?;
        let out = history::commits(&mut tx, org, repo, shas).await;
        finish(tx, out).await
    }

    async fn pull_requests_for_commits(
        &self,
        org: OrgId,
        repo: RepoId,
        shas: &[CommitSha],
    ) -> Result<Vec<(CommitSha, PullRequestRecord)>, KnowledgeError> {
        let mut tx = self.begin(org).await?;
        let out = history::pull_requests_for_commits(&mut tx, org, repo, shas).await;
        finish(tx, out).await
    }

    async fn ticket_links(&self, org: OrgId, repo: RepoId, by: &LinkLookup) -> Result<Vec<TicketLink>, KnowledgeError> {
        let mut tx = self.begin(org).await?;
        let out = history::ticket_links(&mut tx, org, repo, by).await;
        finish(tx, out).await
    }

    async fn delete_version_facts(&self, org: OrgId, key: &RepoVersionKey) -> Result<(), KnowledgeError> {
        retry_tx(|| async {
            let mut tx = self.begin(org).await?;
            gc::delete_version_facts(&mut tx, org, key).await?;
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    async fn sweep_unreferenced_blobs(&self, org: OrgId, grace: Duration, max: u32) -> Result<Vec<BlobOid>, KnowledgeError> {
        let now = self.clock.now();
        let Some(cutoff) = time::Duration::try_from(grace).ok().and_then(|grace| now.checked_sub(grace)) else {
            return Ok(Vec::new());
        };
        retry_tx(|| async {
            let mut tx = self.begin(org).await?;
            let swept = gc::sweep_unreferenced_blobs(&mut tx, org, cutoff, max).await?;
            tx.commit().await?;
            Ok(swept)
        })
        .await
    }
}
