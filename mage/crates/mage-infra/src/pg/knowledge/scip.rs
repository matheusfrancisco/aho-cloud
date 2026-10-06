//! Layer 2 (Refs): SCIP index ingest (`scip_indexes`, `scip_occurrences`, `scip_symbols`) and its
//! reads. Only `ready` indexes are read; a document is usable at a version when its blob is the
//! manifest blob of its path.

use std::collections::HashSet;

use mage_core::domain::ids::{ArtifactId, BlobOid, CommitSha, OrgId, RepoId, RepoVersionKey};
use mage_core::domain::path::RepoPath;
use mage_core::ports::knowledge::{
    Paged, RefHit, RefKind, ScipDocRows, ScipIndexInfo, ScipMetadata, ScipRange, ScipRoles, ScipSymbolHit, ScipSymbolInfo, SymbolRef,
};
use sqlx::PgConnection;
use sqlx::types::Json;
use time::OffsetDateTime;
use uuid::Uuid;

use super::codec::{CHUNK, Page, range_columns, to_i32, to_path, to_range, to_u32};
use super::view::innermost;
use crate::pg::error::DbError;

const INGESTING: &str = "ingesting";
const READY: &str = "ready";

/// Newest ready index of `repo` at `sha` (by creation, then artifact id).
pub(super) async fn ready_index_at(conn: &mut PgConnection, org: OrgId, repo: RepoId, sha: CommitSha) -> Result<Option<Uuid>, DbError> {
    Ok(sqlx::query_scalar(
        "SELECT scip_index_id FROM scip_indexes WHERE org_id = $1 AND repo_id = $2 AND sha = $3 AND status = $4 \
         ORDER BY created_at DESC, artifact_id DESC LIMIT 1",
    )
    .bind(org.as_uuid())
    .bind(repo.as_uuid())
    .bind(sha.as_bytes().as_slice())
    .bind(READY)
    .fetch_optional(&mut *conn)
    .await?)
}

async fn ready_index(conn: &mut PgConnection, org: OrgId, artifact: ArtifactId) -> Result<Option<Uuid>, DbError> {
    Ok(sqlx::query_scalar("SELECT scip_index_id FROM scip_indexes WHERE org_id = $1 AND artifact_id = $2 AND status = $3")
        .bind(org.as_uuid())
        .bind(artifact.as_uuid())
        .bind(READY)
        .fetch_optional(&mut *conn)
        .await?)
}

/// The index of `artifact` in any state; `NotFound` before `begin_scip_index`.
async fn index_of(conn: &mut PgConnection, org: OrgId, artifact: ArtifactId) -> Result<Uuid, DbError> {
    sqlx::query_scalar("SELECT scip_index_id FROM scip_indexes WHERE org_id = $1 AND artifact_id = $2")
        .bind(org.as_uuid())
        .bind(artifact.as_uuid())
        .fetch_optional(&mut *conn)
        .await?
        .ok_or(DbError::NotFound)
}

/// Drop the indexes `ids` with their occurrence and symbol rows.
pub(super) async fn drop_indexes(conn: &mut PgConnection, org: OrgId, ids: &[Uuid]) -> Result<(), DbError> {
    if ids.is_empty() {
        return Ok(());
    }
    for table in ["scip_occurrences", "scip_symbols", "scip_indexes"] {
        sqlx::query(&format!("DELETE FROM {table} WHERE org_id = $1 AND scip_index_id = ANY($2)"))
            .bind(org.as_uuid())
            .bind(ids)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

/// (Re)start the ingest of `artifact` as index `id`: a previous index of the artifact and its rows
/// are dropped.
pub(super) async fn begin(
    conn: &mut PgConnection,
    org: OrgId,
    key: &RepoVersionKey,
    artifact: ArtifactId,
    meta: &ScipMetadata,
    id: Uuid,
    now: OffsetDateTime,
) -> Result<(), DbError> {
    let previous: Vec<Uuid> = sqlx::query_scalar("SELECT scip_index_id FROM scip_indexes WHERE org_id = $1 AND artifact_id = $2 FOR UPDATE")
        .bind(org.as_uuid())
        .bind(artifact.as_uuid())
        .fetch_all(&mut *conn)
        .await?;
    drop_indexes(conn, org, &previous).await?;
    sqlx::query(
        "INSERT INTO scip_indexes (org_id, scip_index_id, repo_id, sha, artifact_id, tool, languages, documents, status, created_at) \
         VALUES ($1, $2, $3, $4, $5, $6, '{}', 0, $7, $8)",
    )
    .bind(org.as_uuid())
    .bind(id)
    .bind(key.repo_id.as_uuid())
    .bind(key.sha.as_bytes().as_slice())
    .bind(artifact.as_uuid())
    .bind(&meta.tool_name)
    .bind(INGESTING)
    .bind(now)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Occurrence `ord` = position in its document; the first row of a `(path, ord)` wins.
pub(super) async fn put_documents(conn: &mut PgConnection, org: OrgId, artifact: ArtifactId, docs: &[ScipDocRows]) -> Result<(), DbError> {
    let index = index_of(conn, org, artifact).await?;
    let mut seen = HashSet::new();
    let mut rows = Vec::new();
    for doc in docs {
        for (ord, occ) in doc.occurrences.iter().enumerate() {
            let ord = i32::try_from(ord).map_err(|_| DbError::Invalid(format!("document {} has too many occurrences", doc.path)))?;
            if seen.insert((doc.path.as_str(), ord)) {
                rows.push((doc, ord, occ));
            }
        }
    }
    for chunk in rows.chunks(CHUNK) {
        let n = chunk.len();
        let (mut paths, mut ords, mut blobs, mut symbols, mut roles) =
            (Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n));
        let mut range: [Vec<i32>; 4] = Default::default();
        let mut enclosing: [Vec<Option<i32>>; 4] = Default::default();
        for (doc, ord, occ) in chunk {
            paths.push(doc.path.as_str());
            ords.push(*ord);
            blobs.push(doc.blob.0);
            symbols.push(occ.symbol.as_str());
            roles.push(i32::try_from(occ.roles.0).map_err(|_| DbError::Invalid(format!("scip roles {} exceed the stored range", occ.roles.0)))?);
            for (column, value) in range.iter_mut().zip(range_columns(&occ.range)?) {
                column.push(value);
            }
            let outer = occ.enclosing.as_ref().map(range_columns).transpose()?;
            for (i, column) in enclosing.iter_mut().enumerate() {
                column.push(outer.map(|outer| outer[i]));
            }
        }
        let [sl, sc, el, ec] = &range;
        let [esl, esc, eel, eec] = &enclosing;
        sqlx::query(
            "INSERT INTO scip_occurrences (org_id, scip_index_id, path, ord, blob_oid, range, symbol, roles, enclosing_range) \
             SELECT $1, $2, u.path, u.ord, u.blob, ARRAY[u.sl, u.sc, u.el, u.ec], u.symbol, u.roles, \
                    CASE WHEN u.esl IS NULL THEN NULL ELSE ARRAY[u.esl, u.esc, u.eel, u.eec] END \
             FROM UNNEST($3::text[], $4::int4[], $5::bytea[], $6::text[], $7::int4[], $8::int4[], $9::int4[], $10::int4[], $11::int4[], \
                         $12::int4[], $13::int4[], $14::int4[], $15::int4[]) \
                  AS u(path, ord, blob, symbol, roles, sl, sc, el, ec, esl, esc, eel, eec) \
             ON CONFLICT DO NOTHING",
        )
        .bind(org.as_uuid())
        .bind(index)
        .bind(&paths)
        .bind(&ords)
        .bind(&blobs)
        .bind(&symbols)
        .bind(&roles)
        .bind(sl)
        .bind(sc)
        .bind(el)
        .bind(ec)
        .bind(esl)
        .bind(esc)
        .bind(eel)
        .bind(eec)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// First row of a symbol wins.
pub(super) async fn put_symbols(conn: &mut PgConnection, org: OrgId, artifact: ArtifactId, symbols: &[ScipSymbolInfo]) -> Result<(), DbError> {
    let index = index_of(conn, org, artifact).await?;
    let mut seen = HashSet::new();
    let symbols: Vec<&ScipSymbolInfo> = symbols.iter().filter(|s| seen.insert(s.symbol.as_str())).collect();
    for chunk in symbols.chunks(CHUNK) {
        let names: Vec<&str> = chunk.iter().map(|s| s.symbol.as_str()).collect();
        let kinds: Vec<Option<&str>> = chunk.iter().map(|s| s.kind.as_deref()).collect();
        let display: Vec<Option<&str>> = chunk.iter().map(|s| s.display_name.as_deref()).collect();
        let docs: Vec<Option<&str>> = chunk.iter().map(|s| s.documentation.as_deref()).collect();
        let relationships: Vec<Json<&Vec<_>>> = chunk.iter().map(|s| Json(&s.relationships)).collect();
        sqlx::query(
            "INSERT INTO scip_symbols (org_id, scip_index_id, symbol, kind, display_name, documentation, relationships) \
             SELECT $1, $2, u.* FROM UNNEST($3::text[], $4::text[], $5::text[], $6::text[], $7::jsonb[]) AS u \
             ON CONFLICT DO NOTHING",
        )
        .bind(org.as_uuid())
        .bind(index)
        .bind(&names)
        .bind(&kinds)
        .bind(&display)
        .bind(&docs)
        .bind(&relationships)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

pub(super) async fn finish(conn: &mut PgConnection, org: OrgId, artifact: ArtifactId, languages: &[String], documents: u32) -> Result<(), DbError> {
    let done = sqlx::query("UPDATE scip_indexes SET status = $3, languages = $4, documents = $5 WHERE org_id = $1 AND artifact_id = $2")
        .bind(org.as_uuid())
        .bind(artifact.as_uuid())
        .bind(READY)
        .bind(languages)
        .bind(to_i32(documents, "documents")?)
        .execute(&mut *conn)
        .await?;
    if done.rows_affected() == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

/// The newest ready index of exactly this sha, else the newest ready index of the repo.
pub(super) async fn index_for(conn: &mut PgConnection, org: OrgId, key: &RepoVersionKey) -> Result<Option<ScipIndexInfo>, DbError> {
    let row: Option<(Uuid, [u8; 20], String, Vec<String>, i32)> = sqlx::query_as(
        "SELECT artifact_id, sha, tool, languages, documents FROM scip_indexes \
         WHERE org_id = $1 AND repo_id = $2 AND status = $4 \
         ORDER BY (sha = $3) DESC, created_at DESC, artifact_id DESC LIMIT 1",
    )
    .bind(org.as_uuid())
    .bind(key.repo_id.as_uuid())
    .bind(key.sha.as_bytes().as_slice())
    .bind(READY)
    .fetch_optional(&mut *conn)
    .await?;
    row.map(|(artifact, sha, tool, languages, documents)| {
        Ok(ScipIndexInfo {
            artifact: ArtifactId::from_uuid(artifact),
            sha: CommitSha::from_bytes(sha),
            tool,
            languages,
            documents: to_u32(documents, "documents")?,
        })
    })
    .transpose()
}

/// Nested ranges sort innermost first.
fn range_size(range: &ScipRange) -> (u32, i64) {
    (range.end_line.saturating_sub(range.start_line), i64::from(range.end_col) - i64::from(range.start_col))
}

/// The smallest occurrence covering `(line, col)` in the document at `path`, its display name, and
/// the first definition occurrence of its symbol in the index (by path, then ord).
pub(super) async fn symbol_at(
    conn: &mut PgConnection,
    org: OrgId,
    artifact: ArtifactId,
    path: &RepoPath,
    line: u32,
    col: Option<u32>,
) -> Result<Option<ScipSymbolHit>, DbError> {
    let Some(index) = ready_index(conn, org, artifact).await? else { return Ok(None) };
    let rows: Vec<(i32, Vec<i32>, String)> = sqlx::query_as(
        "SELECT ord, range, symbol FROM scip_occurrences \
         WHERE org_id = $1 AND scip_index_id = $2 AND path = $3 AND range[1] <= $4 AND range[3] >= $4",
    )
    .bind(org.as_uuid())
    .bind(index)
    .bind(path.as_str())
    .bind(i64::from(line))
    .fetch_all(&mut *conn)
    .await?;
    let mut best: Option<((u32, i64), i32, String)> = None;
    for (ord, range, symbol) in rows {
        let range = to_range(&range)?;
        if !range.covers(line, col) {
            continue;
        }
        let rank = (range_size(&range), ord);
        if best.as_ref().is_none_or(|(size, best_ord, _)| rank < (*size, *best_ord)) {
            best = Some((rank.0, ord, symbol));
        }
    }
    let Some((_, _, symbol)) = best else { return Ok(None) };
    let display_name: Option<Option<String>> =
        sqlx::query_scalar("SELECT display_name FROM scip_symbols WHERE org_id = $1 AND scip_index_id = $2 AND symbol = $3")
            .bind(org.as_uuid())
            .bind(index)
            .bind(&symbol)
            .fetch_optional(&mut *conn)
            .await?;
    let def: Option<(String, Vec<i32>)> = sqlx::query_as(
        "SELECT path, range FROM scip_occurrences WHERE org_id = $1 AND scip_index_id = $2 AND symbol = $3 AND roles & 1 = 1 \
         ORDER BY path COLLATE \"C\", ord LIMIT 1",
    )
    .bind(org.as_uuid())
    .bind(index)
    .bind(&symbol)
    .fetch_optional(&mut *conn)
    .await?;
    let def = def.map(|(path, range)| Ok::<_, DbError>((to_path(path)?, to_range(&range)?))).transpose()?;
    Ok(Some(ScipSymbolHit { symbol, display_name: display_name.flatten(), def }))
}

/// Keyset of `scip_occurrences` pages: path, line, column, ord.
pub(super) type OccurrenceKey = (String, i32, i32, i32);

#[allow(clippy::too_many_arguments)]
pub(super) async fn occurrences(
    conn: &mut PgConnection,
    org: OrgId,
    artifact: ArtifactId,
    key: &RepoVersionKey,
    parser: Option<i16>,
    symbol: &str,
    defs: bool,
    page: &Page<OccurrenceKey>,
) -> Result<Paged<RefHit>, DbError> {
    let Some(index) = ready_index(conn, org, artifact).await? else { return Ok(Paged { items: Vec::new(), next: None }) };
    let (after_path, after_line, after_col, after_ord) = match &page.after {
        Some((path, line, col, ord)) => (Some(path.as_str()), *line, *col, *ord),
        None => (None, 0, 0, 0),
    };
    let rows: Vec<(String, [u8; 20], i32, i32, i32, i32, Option<i32>)> = sqlx::query_as(&format!(
        "SELECT o.path, o.blob_oid, o.range[1], o.range[2], o.ord, o.roles, enc.ord \
         FROM scip_occurrences o \
         JOIN manifests m ON m.org_id = o.org_id AND m.repo_id = $3 AND m.sha = $4 AND m.path = o.path AND m.blob_oid = o.blob_oid \
         LEFT JOIN LATERAL ({}) enc ON true \
         WHERE o.org_id = $1 AND o.scip_index_id = $2 AND o.symbol = $5 AND ($6 OR o.roles & 1 = 0) \
           AND ($8::text IS NULL OR (o.path COLLATE \"C\", o.range[1], o.range[2], o.ord) > ($8, $9, $10, $11)) \
         ORDER BY o.path COLLATE \"C\", o.range[1], o.range[2], o.ord LIMIT $12",
        innermost("o.blob_oid", "o.range[1]", "$7")
    ))
    .bind(org.as_uuid())
    .bind(index)
    .bind(key.repo_id.as_uuid())
    .bind(key.sha.as_bytes().as_slice())
    .bind(symbol)
    .bind(defs)
    .bind(parser)
    .bind(after_path)
    .bind(after_line)
    .bind(after_col)
    .bind(after_ord)
    .bind(page.fetch())
    .fetch_all(&mut *conn)
    .await?;
    let hits = rows
        .into_iter()
        .map(|(path, blob, line, col, ord, roles, enclosing)| {
            let path = to_path(path)?;
            let blob = BlobOid::from_bytes(blob);
            let kind = if ScipRoles(to_u32(roles, "scip roles")?).contains(ScipRoles::IMPORT) { RefKind::Import } else { RefKind::Read };
            let enclosing = enclosing.map(|n| to_u32(n, "ord")).transpose()?.map(|ord| SymbolRef { path: path.clone(), blob, ord });
            Ok((RefHit { path, pos: (to_u32(line, "scip line")?, to_u32(col, "scip column")?), kind, enclosing }, ord))
        })
        .collect::<Result<Vec<_>, DbError>>()?;
    let Paged { items, next } = page.finish(hits, |(hit, ord)| {
        (hit.path.as_str().to_owned(), i32::try_from(hit.pos.0).unwrap_or(i32::MAX), i32::try_from(hit.pos.1).unwrap_or(i32::MAX), *ord)
    })?;
    Ok(Paged { items: items.into_iter().map(|(hit, _)| hit).collect(), next })
}

/// Indexes of `key`'s sha (any state), for `delete_version_facts`.
pub(super) async fn indexes_of_sha(conn: &mut PgConnection, org: OrgId, key: &RepoVersionKey) -> Result<Vec<Uuid>, DbError> {
    Ok(sqlx::query_scalar("SELECT scip_index_id FROM scip_indexes WHERE org_id = $1 AND repo_id = $2 AND sha = $3 FOR UPDATE")
        .bind(org.as_uuid())
        .bind(key.repo_id.as_uuid())
        .bind(key.sha.as_bytes().as_slice())
        .fetch_all(&mut *conn)
        .await?)
}
