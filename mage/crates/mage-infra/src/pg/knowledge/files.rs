//! Layer 1 (Files): manifests, blob rows, stage lookups, and the path reads of a version. Paths
//! sort bytewise (`COLLATE "C"`, `RepoPath`'s order) whatever the database collation.

use std::cmp::Reverse;
use std::collections::HashSet;

use mage_core::domain::fix::Glob;
use mage_core::domain::ids::{BlobOid, OrgId, RepoVersionKey};
use mage_core::domain::path::{Language, RepoPath};
use mage_core::domain::repo::IndexRevs;
use mage_core::ports::knowledge::{
    BlobMeta, BlobStage, DirEntry, DirEntryKind, EntryKind, FactCounts, ManifestChange, ManifestEntry, Paged, PathMatch, VersionStats,
};
use mage_core::wire::indexer::ChangeKind;
use sqlx::PgConnection;
use time::OffsetDateTime;

use super::codec::{CHUNK, DbEnum, Page, count, stage_columns, to_i32, to_i64, to_path, to_rev, to_u32, to_u64};
use super::view::{ENTRY_COLUMNS, ENTRY_FROM, EntryRow};
use crate::pg::error::DbError;

/// Manifest rows scanned per round when a glob filters in Rust.
const GLOB_SCAN: i64 = 1_000;

/// Replace the manifest of `key` (paths already checked unique).
pub(super) async fn put_manifest(conn: &mut PgConnection, org: OrgId, key: &RepoVersionKey, entries: &[ManifestEntry]) -> Result<(), DbError> {
    sqlx::query("DELETE FROM manifests WHERE org_id = $1 AND repo_id = $2 AND sha = $3")
        .bind(org.as_uuid())
        .bind(key.repo_id.as_uuid())
        .bind(key.sha.as_bytes().as_slice())
        .execute(&mut *conn)
        .await?;
    for chunk in entries.chunks(CHUNK) {
        let paths: Vec<&str> = chunk.iter().map(|e| e.path.as_str()).collect();
        let blobs: Vec<[u8; 20]> = chunk.iter().map(|e| e.blob.0).collect();
        let kinds: Vec<&str> = chunk.iter().map(|e| e.kind.as_db()).collect();
        let executable: Vec<bool> = chunk.iter().map(|e| e.executable).collect();
        let langs: Vec<Option<&str>> = chunk.iter().map(|e| e.lang.map(DbEnum::as_db)).collect();
        let classes: Vec<&str> = chunk.iter().map(|e| e.class.as_db()).collect();
        let sizes = chunk.iter().map(|e| to_i64(e.size, "size")).collect::<Result<Vec<_>, _>>()?;
        sqlx::query(
            "INSERT INTO manifests (org_id, repo_id, sha, path, blob_oid, kind, executable, lang, class, size) \
             SELECT $1, $2, $3, u.* FROM UNNEST($4::text[], $5::bytea[], $6::text[], $7::bool[], $8::text[], $9::text[], $10::int8[]) AS u",
        )
        .bind(org.as_uuid())
        .bind(key.repo_id.as_uuid())
        .bind(key.sha.as_bytes().as_slice())
        .bind(&paths)
        .bind(&blobs)
        .bind(&kinds)
        .bind(&executable)
        .bind(&langs)
        .bind(&classes)
        .bind(&sizes)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Insert-if-absent per oid (the first row of an oid wins, within the call too).
pub(super) async fn put_blob_meta(conn: &mut PgConnection, org: OrgId, metas: &[BlobMeta], now: OffsetDateTime) -> Result<(), DbError> {
    let mut seen = HashSet::new();
    let metas: Vec<&BlobMeta> = metas.iter().filter(|m| seen.insert(m.oid)).collect();
    for chunk in metas.chunks(CHUNK) {
        let oids: Vec<[u8; 20]> = chunk.iter().map(|m| m.oid.0).collect();
        let sizes = chunk.iter().map(|m| to_i64(m.size, "size")).collect::<Result<Vec<_>, _>>()?;
        let lines = chunk.iter().map(|m| to_i32(m.line_count, "line count")).collect::<Result<Vec<_>, _>>()?;
        let text: Vec<bool> = chunk.iter().map(|m| m.is_text).collect();
        let stored: Vec<bool> = chunk.iter().map(|m| m.content_stored).collect();
        sqlx::query(
            "INSERT INTO blobs (org_id, blob_oid, size, line_count, is_text, content_stored, created_at) \
             SELECT $1, u.*, $2 FROM UNNEST($3::bytea[], $4::int8[], $5::int4[], $6::bool[], $7::bool[]) AS u \
             ON CONFLICT DO NOTHING",
        )
        .bind(org.as_uuid())
        .bind(now)
        .bind(&oids)
        .bind(&sizes)
        .bind(&lines)
        .bind(&text)
        .bind(&stored)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

pub(super) async fn blob_meta(conn: &mut PgConnection, org: OrgId, oids: &[BlobOid]) -> Result<Vec<BlobMeta>, DbError> {
    let oids: Vec<[u8; 20]> = oids.iter().map(|oid| oid.0).collect();
    let rows: Vec<([u8; 20], i64, i32, bool, bool)> = sqlx::query_as(
        "SELECT blob_oid, size, line_count, is_text, content_stored FROM blobs WHERE org_id = $1 AND blob_oid = ANY($2) ORDER BY blob_oid",
    )
    .bind(org.as_uuid())
    .bind(&oids)
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter()
        .map(|(oid, size, line_count, is_text, content_stored)| {
            Ok(BlobMeta {
                oid: BlobOid::from_bytes(oid),
                size: to_u64(size, "size")?,
                line_count: to_u32(line_count, "line count")?,
                is_text,
                content_stored,
            })
        })
        .collect()
}

pub(super) async fn missing(conn: &mut PgConnection, org: OrgId, stage: BlobStage, oids: &[BlobOid]) -> Result<Vec<BlobOid>, DbError> {
    let (stage, rev) = stage_columns(stage)?;
    let oids: Vec<[u8; 20]> = oids.iter().map(|oid| oid.0).collect();
    let rows: Vec<[u8; 20]> = sqlx::query_scalar(
        "SELECT DISTINCT o.oid FROM UNNEST($2::bytea[]) AS o(oid) \
         WHERE NOT EXISTS (SELECT 1 FROM blob_stages s WHERE s.org_id = $1 AND s.blob_oid = o.oid AND s.stage = $3 AND s.rev = $4) \
         ORDER BY o.oid",
    )
    .bind(org.as_uuid())
    .bind(&oids)
    .bind(stage)
    .bind(rev)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows.into_iter().map(BlobOid::from_bytes).collect())
}

pub(super) async fn manifest_entries(
    conn: &mut PgConnection,
    org: OrgId,
    key: &RepoVersionKey,
    paths: &[RepoPath],
) -> Result<Vec<ManifestEntry>, DbError> {
    let paths: Vec<&str> = paths.iter().map(RepoPath::as_str).collect();
    let rows: Vec<EntryRow> = sqlx::query_as(&format!(
        "SELECT {ENTRY_COLUMNS} FROM {ENTRY_FROM} \
         WHERE m.org_id = $1 AND m.repo_id = $2 AND m.sha = $3 AND m.path = ANY($4) ORDER BY m.path COLLATE \"C\""
    ))
    .bind(org.as_uuid())
    .bind(key.repo_id.as_uuid())
    .bind(key.sha.as_bytes().as_slice())
    .bind(&paths)
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter().map(EntryRow::into_entry).collect()
}

/// Entries after path `after`, by path, optionally only those whose ASCII-lowercased path contains
/// the (lowercase) `needle`.
async fn entries_after(
    conn: &mut PgConnection,
    org: OrgId,
    key: &RepoVersionKey,
    after: Option<&str>,
    needle: Option<&str>,
    limit: i64,
) -> Result<Vec<ManifestEntry>, DbError> {
    let rows: Vec<EntryRow> = sqlx::query_as(&format!(
        "SELECT {ENTRY_COLUMNS} FROM {ENTRY_FROM} \
         WHERE m.org_id = $1 AND m.repo_id = $2 AND m.sha = $3 \
           AND ($4::text IS NULL OR m.path COLLATE \"C\" > $4) \
           AND ($5::text IS NULL OR strpos(translate(m.path, 'ABCDEFGHIJKLMNOPQRSTUVWXYZ', 'abcdefghijklmnopqrstuvwxyz'), $5) > 0) \
         ORDER BY m.path COLLATE \"C\" LIMIT $6"
    ))
    .bind(org.as_uuid())
    .bind(key.repo_id.as_uuid())
    .bind(key.sha.as_bytes().as_slice())
    .bind(after)
    .bind(needle)
    .bind(limit)
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter().map(EntryRow::into_entry).collect()
}

pub(super) async fn manifest_all(
    conn: &mut PgConnection,
    org: OrgId,
    key: &RepoVersionKey,
    page: &Page<String>,
) -> Result<Paged<ManifestEntry>, DbError> {
    let entries = entries_after(conn, org, key, page.after.as_deref(), None, page.fetch()).await?;
    page.finish(entries, |e| e.path.as_str().to_owned())
}

/// An unparsable glob is rejected before any query (`Invalid`).
pub(super) fn path_matcher(m: &PathMatch) -> Result<PathMatcher, DbError> {
    Ok(match m {
        PathMatch::Substring(needle) => PathMatcher::Substring(needle.to_ascii_lowercase()),
        PathMatch::Glob(pattern) => {
            PathMatcher::Glob(Glob::new(pattern.clone()).map_err(|e| DbError::Invalid(format!("glob {pattern:?}: {e}")))?)
        }
    })
}

pub(super) enum PathMatcher {
    Substring(String),
    Glob(Glob),
}

pub(super) async fn find_paths(
    conn: &mut PgConnection,
    org: OrgId,
    key: &RepoVersionKey,
    matcher: &PathMatcher,
    page: &Page<String>,
) -> Result<Paged<ManifestEntry>, DbError> {
    let found = match matcher {
        PathMatcher::Substring(needle) => entries_after(conn, org, key, page.after.as_deref(), Some(needle), page.fetch()).await?,
        PathMatcher::Glob(glob) => {
            let mut found = Vec::new();
            let mut scanned = page.after.clone();
            loop {
                let batch = entries_after(conn, org, key, scanned.as_deref(), None, GLOB_SCAN).await?;
                let exhausted = batch.len() < GLOB_SCAN as usize;
                scanned = batch.last().map(|e| e.path.as_str().to_owned());
                found.extend(batch.into_iter().filter(|e| glob.matches(e.path.as_str())));
                if exhausted || found.len() > page.limit {
                    break found;
                }
            }
        }
    };
    page.finish(found, |e| e.path.as_str().to_owned())
}

pub(super) async fn list_dir(
    conn: &mut PgConnection,
    org: OrgId,
    key: &RepoVersionKey,
    dir: Option<&RepoPath>,
    page: &Page<String>,
) -> Result<Paged<DirEntry>, DbError> {
    let prefix = dir.map(|dir| format!("{dir}/")).unwrap_or_default();
    // A name is a file or a directory, never both (git trees); the file row sorts first.
    let rows: Vec<(String, bool, String, i64, Option<String>)> = sqlx::query_as(
        "SELECT DISTINCT ON (c.name COLLATE \"C\") c.name, c.is_dir, c.kind, c.size, c.lang FROM ( \
           SELECT split_part(r.rest, '/', 1) AS name, strpos(r.rest, '/') > 0 AS is_dir, r.kind, r.size, r.lang FROM ( \
             SELECT substr(m.path, char_length($4::text) + 1) AS rest, m.kind, m.size, m.lang FROM manifests m \
             WHERE m.org_id = $1 AND m.repo_id = $2 AND m.sha = $3 AND starts_with(m.path, $4::text) \
           ) r \
         ) c \
         WHERE ($5::text IS NULL OR c.name COLLATE \"C\" > $5) \
         ORDER BY c.name COLLATE \"C\", c.is_dir LIMIT $6",
    )
    .bind(org.as_uuid())
    .bind(key.repo_id.as_uuid())
    .bind(key.sha.as_bytes().as_slice())
    .bind(&prefix)
    .bind(page.after.as_deref())
    .bind(page.fetch())
    .fetch_all(&mut *conn)
    .await?;
    let children = rows
        .into_iter()
        .map(|(name, is_dir, kind, size, lang)| {
            if is_dir {
                return Ok(DirEntry { name, kind: DirEntryKind::Dir, size: None, lang: None });
            }
            let kind = match EntryKind::from_db(&kind)? {
                EntryKind::File => DirEntryKind::File,
                EntryKind::Symlink => DirEntryKind::Symlink,
                EntryKind::Submodule => DirEntryKind::Submodule,
            };
            Ok(DirEntry { name, kind, size: Some(to_u64(size, "size")?), lang: lang.as_deref().map(Language::from_db).transpose()? })
        })
        .collect::<Result<Vec<_>, DbError>>()?;
    page.finish(children, |child| child.name.clone())
}

pub(super) async fn paths_by_basename(
    conn: &mut PgConnection,
    org: OrgId,
    key: &RepoVersionKey,
    basename: &str,
    limit: u16,
) -> Result<Vec<RepoPath>, DbError> {
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT m.path FROM manifests m \
         WHERE m.org_id = $1 AND m.repo_id = $2 AND m.sha = $3 AND split_part(m.path, '/', -1) = $4 \
         ORDER BY octet_length(m.path), m.path COLLATE \"C\" LIMIT $5",
    )
    .bind(org.as_uuid())
    .bind(key.repo_id.as_uuid())
    .bind(key.sha.as_bytes().as_slice())
    .bind(basename)
    .bind(i64::from(limit))
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter().map(to_path).collect()
}

pub(super) async fn version_stats(
    conn: &mut PgConnection,
    org: OrgId,
    key: &RepoVersionKey,
    revs: Option<IndexRevs>,
) -> Result<VersionStats, DbError> {
    let (files, bytes): (i64, i64) = sqlx::query_as(
        "SELECT count(*)::int8, COALESCE(sum(m.size), 0)::int8 FROM manifests m \
         WHERE m.org_id = $1 AND m.repo_id = $2 AND m.sha = $3 AND m.kind = 'file'",
    )
    .bind(org.as_uuid())
    .bind(key.repo_id.as_uuid())
    .bind(key.sha.as_bytes().as_slice())
    .fetch_one(&mut *conn)
    .await?;

    let langs: Vec<(String, i64, i64)> = sqlx::query_as(
        "SELECT m.lang, count(*)::int8, COALESCE(sum(b.line_count), 0)::int8 \
         FROM manifests m LEFT JOIN blobs b ON b.org_id = m.org_id AND b.blob_oid = m.blob_oid \
         WHERE m.org_id = $1 AND m.repo_id = $2 AND m.sha = $3 AND m.kind = 'file' AND m.lang IS NOT NULL \
         GROUP BY m.lang",
    )
    .bind(org.as_uuid())
    .bind(key.repo_id.as_uuid())
    .bind(key.sha.as_bytes().as_slice())
    .fetch_all(&mut *conn)
    .await?;
    let mut by_lang = langs
        .into_iter()
        .map(|(lang, files, lines)| Ok((Language::from_db(&lang)?, count(files), to_u64(lines, "line count")?)))
        .collect::<Result<Vec<_>, DbError>>()?;
    by_lang.sort_by_key(|(lang, files, _)| (Reverse(*files), *lang));

    let dirs: Vec<(String, i64)> = sqlx::query_as(
        "SELECT split_part(m.path, '/', 1), count(*)::int8 FROM manifests m \
         WHERE m.org_id = $1 AND m.repo_id = $2 AND m.sha = $3 AND m.kind = 'file' AND strpos(m.path, '/') > 0 \
         GROUP BY 1 ORDER BY 2 DESC, split_part(m.path, '/', 1) COLLATE \"C\" LIMIT $4",
    )
    .bind(org.as_uuid())
    .bind(key.repo_id.as_uuid())
    .bind(key.sha.as_bytes().as_slice())
    .bind(i64::try_from(VersionStats::TOP_DIRS).unwrap_or(i64::MAX))
    .fetch_all(&mut *conn)
    .await?;
    let top_dirs = dirs.into_iter().map(|(dir, files)| Ok((to_path(dir)?, count(files)))).collect::<Result<Vec<_>, DbError>>()?;

    let parser = revs.map(|revs| to_rev(revs.parser.0)).transpose()?;
    let errors = revs.map(|revs| to_rev(revs.errors.0)).transpose()?;
    let (symbols, error_sites, endpoints, pages, tables): (i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT \
           (SELECT count(*) FROM manifests m JOIN symbols s ON s.org_id = m.org_id AND s.blob_oid = m.blob_oid AND s.parser_rev = $4 \
            WHERE m.org_id = $1 AND m.repo_id = $2 AND m.sha = $3)::int8, \
           (SELECT count(*) FROM manifests m JOIN error_sites e ON e.org_id = m.org_id AND e.blob_oid = m.blob_oid AND e.errors_rev = $5 \
            WHERE m.org_id = $1 AND m.repo_id = $2 AND m.sha = $3)::int8, \
           (SELECT count(*) FROM app_nodes n WHERE n.org_id = $1 AND n.repo_id = $2 AND n.sha = $3 AND n.kind = 'endpoint')::int8, \
           (SELECT count(*) FROM app_nodes n WHERE n.org_id = $1 AND n.repo_id = $2 AND n.sha = $3 AND n.kind = 'page')::int8, \
           (SELECT count(*) FROM app_nodes n WHERE n.org_id = $1 AND n.repo_id = $2 AND n.sha = $3 AND n.kind = 'table')::int8",
    )
    .bind(org.as_uuid())
    .bind(key.repo_id.as_uuid())
    .bind(key.sha.as_bytes().as_slice())
    .bind(parser)
    .bind(errors)
    .fetch_one(&mut *conn)
    .await?;

    Ok(VersionStats {
        files: count(files),
        bytes: to_u64(bytes, "size")?,
        by_lang,
        top_dirs,
        counts: FactCounts {
            symbols: count(symbols),
            error_sites: count(error_sites),
            endpoints: count(endpoints),
            pages: count(pages),
            tables: count(tables),
        },
    })
}

pub(super) async fn manifest_diff(
    conn: &mut PgConnection,
    org: OrgId,
    from: &RepoVersionKey,
    to: &RepoVersionKey,
    prefix: Option<&RepoPath>,
    page: &Page<String>,
) -> Result<Paged<ManifestChange>, DbError> {
    let side = |sha: &str| {
        format!(
            "SELECT path, kind, blob_oid FROM manifests WHERE org_id = $1 AND repo_id = $2 AND sha = {sha} \
             AND ($5::text IS NULL OR path = $5 OR starts_with(path, $5 || '/')) \
             AND ($6::text IS NULL OR path COLLATE \"C\" > $6)"
        )
    };
    let rows: Vec<(String, Option<String>, Option<[u8; 20]>, Option<String>, Option<[u8; 20]>)> = sqlx::query_as(&format!(
        "SELECT COALESCE(a.path, b.path), a.kind, a.blob_oid, b.kind, b.blob_oid \
         FROM ({}) a FULL JOIN ({}) b ON a.path = b.path \
         WHERE a.path IS NULL OR b.path IS NULL OR a.kind <> b.kind OR a.blob_oid <> b.blob_oid \
         ORDER BY COALESCE(a.path, b.path) COLLATE \"C\" LIMIT $7",
        side("$3"),
        side("$4")
    ))
    .bind(org.as_uuid())
    .bind(from.repo_id.as_uuid())
    .bind(from.sha.as_bytes().as_slice())
    .bind(to.sha.as_bytes().as_slice())
    .bind(prefix.map(RepoPath::as_str))
    .bind(page.after.as_deref())
    .bind(page.fetch())
    .fetch_all(&mut *conn)
    .await?;
    let changes = rows
        .into_iter()
        .map(|(path, old_kind, old, new_kind, new)| {
            let change = match (&old_kind, &new_kind) {
                (None, _) => ChangeKind::Added,
                (_, None) => ChangeKind::Deleted,
                (Some(a), Some(b)) if a != b => ChangeKind::TypeChanged,
                _ => ChangeKind::Modified,
            };
            Ok(ManifestChange { path: to_path(path)?, change, old: old.map(BlobOid::from_bytes), new: new.map(BlobOid::from_bytes) })
        })
        .collect::<Result<Vec<_>, DbError>>()?;
    page.finish(changes, |change| change.path.as_str().to_owned())
}
