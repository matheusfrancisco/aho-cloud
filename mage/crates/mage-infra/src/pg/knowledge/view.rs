//! A version's view (storage.md "A commit's view is a join"): its revs, its manifest rows, symbol
//! rows, ref resolution, and the stage markers every blob-keyed put writes.

use mage_core::domain::ids::{BlobOid, OrgId, RepoVersionKey};
use mage_core::domain::path::{FileClass, Language, LineRange};
use mage_core::domain::repo::IndexRevs;
use mage_core::ports::knowledge::{EntryKind, ManifestEntry, SymbolDef, SymbolKind, SymbolRef};
use sqlx::PgConnection;
use sqlx::types::Json;
use time::OffsetDateTime;

use super::codec::{CHUNK, DbEnum, to_i32, to_path, to_u32, to_u64};
use crate::pg::error::DbError;

/// `repo_versions.revs` of `key`: `None` without a version or without revs.
pub(super) async fn revs(conn: &mut PgConnection, org: OrgId, key: &RepoVersionKey) -> Result<Option<IndexRevs>, DbError> {
    let revs: Option<Option<Json<IndexRevs>>> =
        sqlx::query_scalar("SELECT revs FROM repo_versions WHERE org_id = $1 AND repo_id = $2 AND sha = $3")
            .bind(org.as_uuid())
            .bind(key.repo_id.as_uuid())
            .bind(key.sha.as_bytes().as_slice())
            .fetch_optional(&mut *conn)
            .await?;
    Ok(revs.flatten().map(|Json(revs)| revs))
}

/// Manifest columns joined to the blob row (`is_text` / `line_count`; absent for submodules).
pub(super) const ENTRY_COLUMNS: &str = "m.path, m.kind, m.executable, m.blob_oid, m.lang, m.class, m.size, b.is_text, b.line_count";
pub(super) const ENTRY_FROM: &str = "manifests m LEFT JOIN blobs b ON b.org_id = m.org_id AND b.blob_oid = m.blob_oid";

#[derive(sqlx::FromRow)]
pub(super) struct EntryRow {
    path: String,
    kind: String,
    executable: bool,
    blob_oid: [u8; 20],
    lang: Option<String>,
    class: String,
    size: i64,
    is_text: Option<bool>,
    line_count: Option<i32>,
}

impl EntryRow {
    pub fn into_entry(self) -> Result<ManifestEntry, DbError> {
        Ok(ManifestEntry {
            path: to_path(self.path)?,
            kind: EntryKind::from_db(&self.kind)?,
            executable: self.executable,
            blob: BlobOid::from_bytes(self.blob_oid),
            lang: self.lang.as_deref().map(Language::from_db).transpose()?,
            class: FileClass::from_db(&self.class)?,
            size: to_u64(self.size, "size")?,
            is_text: self.is_text.unwrap_or(false),
            line_count: self.line_count.map(|n| to_u32(n, "line count")).transpose()?.unwrap_or(0),
        })
    }
}

/// `symbols` columns (alias `s`) decoded by [`SymbolRow`].
pub(super) const SYMBOL_COLUMNS: &str =
    "s.ord, s.name, s.qualified, s.kind, s.container, s.start_line, s.end_line, s.name_line, s.name_col, s.signature, s.doc, s.exported";

#[derive(sqlx::FromRow)]
pub(super) struct SymbolRow {
    ord: i32,
    name: String,
    qualified: String,
    kind: String,
    container: Option<i32>,
    start_line: i32,
    end_line: i32,
    name_line: i32,
    name_col: i32,
    signature: Option<String>,
    doc: Option<String>,
    exported: bool,
}

impl SymbolRow {
    pub fn into_def(self) -> Result<SymbolDef, DbError> {
        let span = LineRange::new(to_u32(self.start_line, "line")?, to_u32(self.end_line, "line")?)
            .map_err(|e| DbError::Invalid(format!("stored symbol span: {e}")))?;
        Ok(SymbolDef {
            ord: to_u32(self.ord, "ord")?,
            name: self.name,
            qualified: self.qualified,
            kind: SymbolKind::from_db(&self.kind)?,
            container: self.container.map(|n| to_u32(n, "ord")).transpose()?,
            span,
            name_pos: (to_u32(self.name_line, "line")?, to_u32(self.name_col, "column")?),
            signature: self.signature,
            doc: self.doc,
            exported: self.exported,
        })
    }
}

/// A symbol row with the manifest path and blob it is seen at.
#[derive(sqlx::FromRow)]
pub(super) struct SymbolAtRow {
    pub path: String,
    pub blob_oid: [u8; 20],
    pub class: String,
    #[sqlx(flatten)]
    pub def: SymbolRow,
}

/// Innermost symbol of `blob` (at parser rev `parser`) whose span holds `line`: smallest span,
/// then highest ord. `LEFT JOIN LATERAL ({INNERMOST}) enc ON true` with `{blob}`, `{line}`,
/// `{parser}` substituted; `enc.ord` is NULL when no symbol encloses the line.
pub(super) fn innermost(blob: &str, line: &str, parser: &str) -> String {
    format!(
        "SELECT s.ord FROM symbols s WHERE s.org_id = $1 AND s.blob_oid = {blob} AND s.parser_rev = {parser} \
         AND s.start_line <= {line} AND s.end_line >= {line} ORDER BY s.end_line - s.start_line, s.ord DESC LIMIT 1"
    )
}

/// Symbols of `refs` valid at the version (the path holds the blob, the ord exists at parser rev
/// `parser`), as `(index into refs, def)` in input order.
pub(super) async fn resolve(
    conn: &mut PgConnection,
    org: OrgId,
    key: &RepoVersionKey,
    parser: i16,
    refs: &[SymbolRef],
) -> Result<Vec<(usize, SymbolDef)>, DbError> {
    let mut at = Vec::with_capacity(refs.len());
    let (mut paths, mut blobs, mut ords) = (Vec::with_capacity(refs.len()), Vec::with_capacity(refs.len()), Vec::with_capacity(refs.len()));
    for (i, r) in refs.iter().enumerate() {
        // An ord past the stored range names no symbol.
        let Ok(ord) = i32::try_from(r.ord) else { continue };
        at.push(i64::try_from(i).unwrap_or(i64::MAX));
        paths.push(r.path.as_str());
        blobs.push(r.blob.0);
        ords.push(ord);
    }
    if at.is_empty() {
        return Ok(Vec::new());
    }
    let found: Vec<IndexedSymbol> = sqlx::query_as(&format!(
        "SELECT r.i, {SYMBOL_COLUMNS} \
         FROM UNNEST($4::int8[], $5::text[], $6::bytea[], $7::int4[]) AS r(i, path, blob, ord) \
         JOIN manifests m ON m.org_id = $1 AND m.repo_id = $2 AND m.sha = $3 AND m.path = r.path AND m.blob_oid = r.blob \
         JOIN symbols s ON s.org_id = $1 AND s.blob_oid = r.blob AND s.parser_rev = $8 AND s.ord = r.ord \
         ORDER BY r.i"
    ))
    .bind(org.as_uuid())
    .bind(key.repo_id.as_uuid())
    .bind(key.sha.as_bytes().as_slice())
    .bind(&at)
    .bind(&paths)
    .bind(&blobs)
    .bind(&ords)
    .bind(parser)
    .fetch_all(&mut *conn)
    .await?;
    found.into_iter().map(|row| Ok((usize::try_from(row.i).unwrap_or(usize::MAX), row.def.into_def()?))).collect()
}

#[derive(sqlx::FromRow)]
struct IndexedSymbol {
    i: i64,
    #[sqlx(flatten)]
    def: SymbolRow,
}

/// One `blob_stages` row to write: blob, language, status, error.
pub(super) struct Marker<'a> {
    pub blob: BlobOid,
    pub lang: Option<&'a str>,
    pub status: &'static str,
    pub error: Option<String>,
}

/// Insert-if-absent stage markers (the first marker of a blob, stage and rev wins).
pub(super) async fn put_markers(
    conn: &mut PgConnection,
    org: OrgId,
    stage: &str,
    rev: i16,
    markers: &[Marker<'_>],
    now: OffsetDateTime,
) -> Result<(), DbError> {
    for chunk in markers.chunks(CHUNK) {
        let blobs: Vec<[u8; 20]> = chunk.iter().map(|m| m.blob.0).collect();
        let langs: Vec<Option<&str>> = chunk.iter().map(|m| m.lang).collect();
        let statuses: Vec<&str> = chunk.iter().map(|m| m.status).collect();
        let errors: Vec<Option<&str>> = chunk.iter().map(|m| m.error.as_deref()).collect();
        sqlx::query(
            "INSERT INTO blob_stages (org_id, blob_oid, stage, rev, lang, status, error, at) \
             SELECT $1, u.blob, $2, $3, u.lang, u.status, u.error, $4 \
             FROM UNNEST($5::bytea[], $6::text[], $7::text[], $8::text[]) AS u(blob, lang, status, error) \
             ON CONFLICT DO NOTHING",
        )
        .bind(org.as_uuid())
        .bind(stage)
        .bind(rev)
        .bind(now)
        .bind(&blobs)
        .bind(&langs)
        .bind(&statuses)
        .bind(&errors)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// `ord` of a row as `int4`.
pub(super) fn ord(n: u32) -> Result<i32, DbError> {
    to_i32(n, "ord")
}
