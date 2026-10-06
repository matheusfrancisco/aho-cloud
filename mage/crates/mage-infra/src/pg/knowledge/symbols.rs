//! Layer 2 (Symbols): parser output (`symbols`, `name_refs`, `literal_sites` and the parse marker)
//! and the symbol / ref reads at a version's parser rev.

use std::cmp::Ordering;
use std::collections::HashSet;

use mage_core::domain::ids::{BlobOid, OrgId, RepoVersionKey};
use mage_core::domain::path::{FileClass, Language, RepoPath};
use mage_core::ports::knowledge::{
    FUZZY_THRESHOLD, LitPart, LiteralContext, LiteralSite, NameMatch, NameRef, NameRefQuery, Paged, ParsedBlob, ParserRev, RefHit,
    RefKind, SymbolDef, SymbolHit, SymbolQuery, SymbolRef, trigram_similarity,
};
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;
use sqlx::types::Json;
use time::OffsetDateTime;

use super::codec::{CHUNK, DbEnum, Page, status_columns, to_i32, to_path, to_rev, to_u32};
use super::view::{Marker, SYMBOL_COLUMNS, SymbolAtRow, ord, put_markers, resolve};
use crate::pg::error::DbError;

/// Rows of every `Ok` parse (first row of a `(blob, ord)` wins, across calls by `ON CONFLICT DO
/// NOTHING`) and one `Parse(rev)` marker per blob (the first entry of a blob decides it).
pub(super) async fn put_parsed(
    conn: &mut PgConnection,
    org: OrgId,
    rev: ParserRev,
    blobs: &[(BlobOid, Result<ParsedBlob, String>)],
    now: OffsetDateTime,
) -> Result<(), DbError> {
    let rev = to_rev(rev.0)?;
    let mut marked = HashSet::new();
    let mut markers = Vec::new();
    let (mut symbol_keys, mut ref_keys, mut literal_keys) = (HashSet::new(), HashSet::new(), HashSet::new());
    let (mut symbols, mut refs, mut literals) = (Vec::new(), Vec::new(), Vec::new());
    for (oid, parsed) in blobs {
        let (lang, (status, error)) = match parsed {
            Ok(parsed) => {
                symbols.extend(parsed.symbols.iter().filter(|s| symbol_keys.insert((*oid, s.ord))).map(|s| (*oid, s)));
                refs.extend(parsed.refs.iter().filter(|r| ref_keys.insert((*oid, r.ord))).map(|r| (*oid, r)));
                literals.extend(parsed.literals.iter().filter(|l| literal_keys.insert((*oid, l.ord))).map(|l| (*oid, l)));
                (Some(parsed.lang.as_db()), status_columns(&parsed.status))
            }
            Err(reason) => (None, ("failed", Some(reason.clone()))),
        };
        if marked.insert(*oid) {
            markers.push(Marker { blob: *oid, lang, status, error });
        }
    }
    put_markers(conn, org, "parse", rev, &markers, now).await?;
    insert_symbols(conn, org, rev, &symbols).await?;
    insert_refs(conn, org, rev, &refs).await?;
    insert_literals(conn, org, rev, &literals).await
}

async fn insert_symbols(conn: &mut PgConnection, org: OrgId, rev: i16, rows: &[(BlobOid, &SymbolDef)]) -> Result<(), DbError> {
    for chunk in rows.chunks(CHUNK) {
        let n = chunk.len();
        let (mut blobs, mut ords, mut names, mut qualified, mut kinds, mut containers) =
            (Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n));
        let (mut starts, mut ends, mut name_lines, mut name_cols, mut signatures, mut docs, mut exported) =
            (Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n));
        for (blob, s) in chunk {
            blobs.push(blob.0);
            ords.push(ord(s.ord)?);
            names.push(s.name.as_str());
            qualified.push(s.qualified.as_str());
            kinds.push(s.kind.as_db());
            containers.push(s.container.map(ord).transpose()?);
            starts.push(to_i32(s.span.start(), "line")?);
            ends.push(to_i32(s.span.end(), "line")?);
            name_lines.push(to_i32(s.name_pos.0, "line")?);
            name_cols.push(to_i32(s.name_pos.1, "column")?);
            signatures.push(s.signature.as_deref());
            docs.push(s.doc.as_deref());
            exported.push(s.exported);
        }
        sqlx::query(
            "INSERT INTO symbols (org_id, blob_oid, parser_rev, ord, name, qualified, kind, container, start_line, end_line, \
                                  name_line, name_col, signature, doc, exported) \
             SELECT $1, u.blob, $2, u.ord, u.name, u.qualified, u.kind, u.container, u.start_line, u.end_line, \
                    u.name_line, u.name_col, u.signature, u.doc, u.exported \
             FROM UNNEST($3::bytea[], $4::int4[], $5::text[], $6::text[], $7::text[], $8::int4[], $9::int4[], $10::int4[], \
                         $11::int4[], $12::int4[], $13::text[], $14::text[], $15::bool[]) \
                  AS u(blob, ord, name, qualified, kind, container, start_line, end_line, name_line, name_col, signature, doc, exported) \
             ON CONFLICT DO NOTHING",
        )
        .bind(org.as_uuid())
        .bind(rev)
        .bind(&blobs)
        .bind(&ords)
        .bind(&names)
        .bind(&qualified)
        .bind(&kinds)
        .bind(&containers)
        .bind(&starts)
        .bind(&ends)
        .bind(&name_lines)
        .bind(&name_cols)
        .bind(&signatures)
        .bind(&docs)
        .bind(&exported)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

async fn insert_refs(conn: &mut PgConnection, org: OrgId, rev: i16, rows: &[(BlobOid, &NameRef)]) -> Result<(), DbError> {
    for chunk in rows.chunks(CHUNK) {
        let blobs: Vec<[u8; 20]> = chunk.iter().map(|(blob, _)| blob.0).collect();
        let ords = chunk.iter().map(|(_, r)| ord(r.ord)).collect::<Result<Vec<_>, _>>()?;
        let names: Vec<&str> = chunk.iter().map(|(_, r)| r.name.as_str()).collect();
        let qualifiers: Vec<Option<&str>> = chunk.iter().map(|(_, r)| r.qualifier.as_deref()).collect();
        let kinds: Vec<&str> = chunk.iter().map(|(_, r)| r.kind.as_db()).collect();
        let lines = chunk.iter().map(|(_, r)| to_i32(r.pos.0, "line")).collect::<Result<Vec<_>, _>>()?;
        let cols = chunk.iter().map(|(_, r)| to_i32(r.pos.1, "column")).collect::<Result<Vec<_>, _>>()?;
        let enclosing = chunk.iter().map(|(_, r)| r.enclosing.map(ord).transpose()).collect::<Result<Vec<_>, _>>()?;
        sqlx::query(
            "INSERT INTO name_refs (org_id, blob_oid, parser_rev, ord, name, qualifier, kind, line, col, enclosing) \
             SELECT $1, u.blob, $2, u.ord, u.name, u.qualifier, u.kind, u.line, u.col, u.enclosing \
             FROM UNNEST($3::bytea[], $4::int4[], $5::text[], $6::text[], $7::text[], $8::int4[], $9::int4[], $10::int4[]) \
                  AS u(blob, ord, name, qualifier, kind, line, col, enclosing) \
             ON CONFLICT DO NOTHING",
        )
        .bind(org.as_uuid())
        .bind(rev)
        .bind(&blobs)
        .bind(&ords)
        .bind(&names)
        .bind(&qualifiers)
        .bind(&kinds)
        .bind(&lines)
        .bind(&cols)
        .bind(&enclosing)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

async fn insert_literals(conn: &mut PgConnection, org: OrgId, rev: i16, rows: &[(BlobOid, &LiteralSite)]) -> Result<(), DbError> {
    for chunk in rows.chunks(CHUNK) {
        let blobs: Vec<[u8; 20]> = chunk.iter().map(|(blob, _)| blob.0).collect();
        let ords = chunk.iter().map(|(_, l)| ord(l.ord)).collect::<Result<Vec<_>, _>>()?;
        let parts: Vec<Json<&Vec<LitPart>>> = chunk.iter().map(|(_, l)| Json(&l.parts)).collect();
        let contexts: Vec<Json<&LiteralContext>> = chunk.iter().map(|(_, l)| Json(&l.context)).collect();
        let lines = chunk.iter().map(|(_, l)| to_i32(l.pos.0, "line")).collect::<Result<Vec<_>, _>>()?;
        let cols = chunk.iter().map(|(_, l)| to_i32(l.pos.1, "column")).collect::<Result<Vec<_>, _>>()?;
        let enclosing = chunk.iter().map(|(_, l)| l.enclosing.map(ord).transpose()).collect::<Result<Vec<_>, _>>()?;
        sqlx::query(
            "INSERT INTO literal_sites (org_id, blob_oid, parser_rev, ord, parts, context, line, col, enclosing_ord) \
             SELECT $1, u.blob, $2, u.ord, u.parts, u.context, u.line, u.col, u.enclosing \
             FROM UNNEST($3::bytea[], $4::int4[], $5::jsonb[], $6::jsonb[], $7::int4[], $8::int4[], $9::int4[]) \
                  AS u(blob, ord, parts, context, line, col, enclosing) \
             ON CONFLICT DO NOTHING",
        )
        .bind(org.as_uuid())
        .bind(rev)
        .bind(&blobs)
        .bind(&ords)
        .bind(&parts)
        .bind(&contexts)
        .bind(&lines)
        .bind(&cols)
        .bind(&enclosing)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Oids with a non-failed `Parse(rev)` marker, by oid, each with its literal sites by ord.
pub(super) async fn literal_sites(
    conn: &mut PgConnection,
    org: OrgId,
    rev: ParserRev,
    oids: &[BlobOid],
) -> Result<Vec<(BlobOid, Language, Vec<LiteralSite>)>, DbError> {
    let rev = to_rev(rev.0)?;
    let oids: Vec<[u8; 20]> = oids.iter().map(|oid| oid.0).collect();
    let parsed: Vec<([u8; 20], String)> = sqlx::query_as(
        "SELECT blob_oid, lang FROM blob_stages \
         WHERE org_id = $1 AND stage = 'parse' AND rev = $2 AND blob_oid = ANY($3) AND status <> 'failed' AND lang IS NOT NULL \
         ORDER BY blob_oid",
    )
    .bind(org.as_uuid())
    .bind(rev)
    .bind(&oids)
    .fetch_all(&mut *conn)
    .await?;
    let parsed_oids: Vec<[u8; 20]> = parsed.iter().map(|(oid, _)| *oid).collect();
    let rows: Vec<LiteralRow> = sqlx::query_as(
        "SELECT blob_oid, ord, parts, context, line, col, enclosing_ord FROM literal_sites \
         WHERE org_id = $1 AND parser_rev = $2 AND blob_oid = ANY($3) ORDER BY blob_oid, ord",
    )
    .bind(org.as_uuid())
    .bind(rev)
    .bind(&parsed_oids)
    .fetch_all(&mut *conn)
    .await?;
    let mut rows = rows.into_iter().peekable();
    let mut out = Vec::with_capacity(parsed.len());
    for (oid, lang) in parsed {
        let mut sites = Vec::new();
        while let Some(row) = rows.next_if(|row| row.blob_oid == oid) {
            sites.push(row.into_site()?);
        }
        out.push((BlobOid::from_bytes(oid), Language::from_db(&lang)?, sites));
    }
    Ok(out)
}

#[derive(sqlx::FromRow)]
struct LiteralRow {
    blob_oid: [u8; 20],
    ord: i32,
    parts: Json<Vec<LitPart>>,
    context: Json<LiteralContext>,
    line: i32,
    col: i32,
    enclosing_ord: Option<i32>,
}

impl LiteralRow {
    fn into_site(self) -> Result<LiteralSite, DbError> {
        Ok(LiteralSite {
            ord: to_u32(self.ord, "ord")?,
            parts: self.parts.0,
            pos: (to_u32(self.line, "line")?, to_u32(self.col, "column")?),
            enclosing: self.enclosing_ord.map(|n| to_u32(n, "ord")).transpose()?,
            context: self.context.0,
        })
    }
}

/// `find_symbols` order: score, qualifier match, exported, non-test (all descending), then path
/// length, path, ord. Also the keyset of its pages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct RankKey {
    score: f32,
    qualified: bool,
    exported: bool,
    non_test: bool,
    path_len: usize,
    path: String,
    ord: u32,
}

impl RankKey {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .score
            .total_cmp(&self.score)
            .then(other.qualified.cmp(&self.qualified))
            .then(other.exported.cmp(&self.exported))
            .then(other.non_test.cmp(&self.non_test))
            .then(self.path_len.cmp(&other.path_len))
            .then_with(|| self.path.cmp(&other.path))
            .then(self.ord.cmp(&other.ord))
    }
}

/// `SymbolQuery.qualifier` rule: a segment of `qualified` (split on `.`, `/`, `::`), or the name of
/// the hit's directory.
fn qualifier_matches(qualifier: &str, qualified: &str, path: &RepoPath) -> bool {
    qualified.replace("::", ".").split(['.', '/']).any(|segment| segment == qualifier)
        || path.parent().is_some_and(|dir| dir.rsplit('/').next() == Some(qualifier))
}

fn name_matches(mode: NameMatch, name: &str, query: &str) -> bool {
    match mode {
        NameMatch::Exact => name == query,
        NameMatch::CaseInsensitive => name.to_lowercase() == query.to_lowercase(),
        NameMatch::Prefix => name.to_lowercase().starts_with(&query.to_lowercase()),
        NameMatch::Fuzzy => trigram_similarity(name, query) >= FUZZY_THRESHOLD,
    }
}

/// Candidates come from SQL (the name predicate uses the `(org_id, name)` / trigram indexes); the
/// exact match rule, score and ranking are applied here, so they equal the port's definitions.
pub(super) async fn find_symbols(
    conn: &mut PgConnection,
    org: OrgId,
    key: &RepoVersionKey,
    parser: i16,
    q: &SymbolQuery,
    page: &Page<RankKey>,
) -> Result<Paged<SymbolHit>, DbError> {
    let name_clause = match q.mode {
        NameMatch::Exact => "s.name = $5",
        NameMatch::CaseInsensitive => "lower(s.name) = lower($5)",
        NameMatch::Prefix => "starts_with(lower(s.name), lower($5))",
        NameMatch::Fuzzy => "s.name % $5",
    };
    if q.mode == NameMatch::Fuzzy {
        sqlx::query("SELECT set_config('pg_trgm.similarity_threshold', $1, true)")
            .bind(FUZZY_THRESHOLD.to_string())
            .execute(&mut *conn)
            .await?;
    }
    let kinds: Vec<&str> = q.kinds.iter().map(|kind| kind.as_db()).collect();
    let rows: Vec<SymbolAtRow> = sqlx::query_as(&format!(
        "SELECT m.path, m.blob_oid, m.class, {SYMBOL_COLUMNS} \
         FROM manifests m JOIN symbols s ON s.org_id = m.org_id AND s.blob_oid = m.blob_oid AND s.parser_rev = $4 \
         WHERE m.org_id = $1 AND m.repo_id = $2 AND m.sha = $3 AND {name_clause} \
           AND (cardinality($6::text[]) = 0 OR s.kind = ANY($6)) \
           AND ($7::text IS NULL OR m.lang = $7) \
           AND ($8::text IS NULL OR m.path = $8 OR starts_with(m.path, $8 || '/')) \
           AND ($9 OR m.class <> 'test')"
    ))
    .bind(org.as_uuid())
    .bind(key.repo_id.as_uuid())
    .bind(key.sha.as_bytes().as_slice())
    .bind(parser)
    .bind(&q.name)
    .bind(&kinds)
    .bind(q.lang.map(DbEnum::as_db))
    .bind(q.path_prefix.as_ref().map(RepoPath::as_str))
    .bind(q.include_tests)
    .fetch_all(&mut *conn)
    .await?;
    let mut ranked = Vec::with_capacity(rows.len());
    for row in rows {
        let def = row.def.into_def()?;
        if !name_matches(q.mode, &def.name, &q.name) {
            continue;
        }
        let path = to_path(row.path)?;
        let score = if def.name == q.name { 1.0 } else { trigram_similarity(&def.name, &q.name) };
        let rank = RankKey {
            score,
            qualified: q.qualifier.as_deref().is_some_and(|qualifier| qualifier_matches(qualifier, &def.qualified, &path)),
            exported: def.exported,
            non_test: FileClass::from_db(&row.class)? != FileClass::Test,
            path_len: path.as_str().len(),
            path: path.as_str().to_owned(),
            ord: def.ord,
        };
        let at = SymbolRef { path, blob: BlobOid::from_bytes(row.blob_oid), ord: def.ord };
        ranked.push((rank, SymbolHit { at, def, score }));
    }
    ranked.sort_by(|a, b| a.0.cmp(&b.0));
    let after = page.after.as_ref();
    let rest: Vec<(RankKey, SymbolHit)> =
        ranked.into_iter().filter(|(rank, _)| after.is_none_or(|after| rank.cmp(after) == Ordering::Greater)).take(page.limit.saturating_add(1)).collect();
    let Paged { items, next } = page.finish(rest, |(rank, _)| rank.clone())?;
    Ok(Paged { items: items.into_iter().map(|(_, hit)| hit).collect(), next })
}

/// Symbols of the blob at `path`, optionally only those whose span holds `line`.
async fn symbols_of_path(
    conn: &mut PgConnection,
    org: OrgId,
    key: &RepoVersionKey,
    parser: i16,
    path: &RepoPath,
    line: Option<u32>,
) -> Result<Vec<SymbolHit>, DbError> {
    let order = if line.is_some() { "s.end_line - s.start_line, s.ord DESC" } else { "s.ord" };
    let rows: Vec<SymbolAtRow> = sqlx::query_as(&format!(
        "SELECT m.path, m.blob_oid, m.class, {SYMBOL_COLUMNS} \
         FROM manifests m JOIN symbols s ON s.org_id = m.org_id AND s.blob_oid = m.blob_oid AND s.parser_rev = $4 \
         WHERE m.org_id = $1 AND m.repo_id = $2 AND m.sha = $3 AND m.path = $5 \
           AND ($6::int8 IS NULL OR (s.start_line <= $6 AND s.end_line >= $6)) \
         ORDER BY {order}"
    ))
    .bind(org.as_uuid())
    .bind(key.repo_id.as_uuid())
    .bind(key.sha.as_bytes().as_slice())
    .bind(parser)
    .bind(path.as_str())
    .bind(line.map(i64::from))
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter()
        .map(|row| {
            let def = row.def.into_def()?;
            let at = SymbolRef { path: path.clone(), blob: BlobOid::from_bytes(row.blob_oid), ord: def.ord };
            Ok(SymbolHit { at, def, score: 1.0 })
        })
        .collect()
}

/// Enclosing chain at `(path, line)`: smallest span first, equal spans by descending ord.
pub(super) async fn symbols_at(
    conn: &mut PgConnection,
    org: OrgId,
    key: &RepoVersionKey,
    parser: i16,
    path: &RepoPath,
    line: u32,
) -> Result<Vec<SymbolHit>, DbError> {
    symbols_of_path(conn, org, key, parser, path, Some(line)).await
}

pub(super) async fn symbols_in_file(
    conn: &mut PgConnection,
    org: OrgId,
    key: &RepoVersionKey,
    parser: i16,
    path: &RepoPath,
) -> Result<Vec<SymbolHit>, DbError> {
    symbols_of_path(conn, org, key, parser, path, None).await
}

pub(super) async fn symbols_by_ref(
    conn: &mut PgConnection,
    org: OrgId,
    key: &RepoVersionKey,
    parser: i16,
    refs: &[SymbolRef],
) -> Result<Vec<SymbolHit>, DbError> {
    let found = resolve(conn, org, key, parser, refs).await?;
    Ok(found.into_iter().filter_map(|(i, def)| refs.get(i).map(|at| SymbolHit { at: at.clone(), def, score: 1.0 })).collect())
}

/// Keyset of `name_refs` pages: path, line, column, ord.
pub(super) type RefKey = (String, i32, i32, i32);

pub(super) async fn name_refs(
    conn: &mut PgConnection,
    org: OrgId,
    key: &RepoVersionKey,
    parser: i16,
    q: &NameRefQuery,
    page: &Page<RefKey>,
) -> Result<Paged<RefHit>, DbError> {
    let kinds: Vec<&str> = q.kinds.iter().map(|kind| kind.as_db()).collect();
    let (after_path, after_line, after_col, after_ord) = match &page.after {
        Some((path, line, col, ord)) => (Some(path.as_str()), *line, *col, *ord),
        None => (None, 0, 0, 0),
    };
    let rows: Vec<(String, [u8; 20], String, i32, i32, i32, Option<i32>)> = sqlx::query_as(
        "SELECT m.path, m.blob_oid, r.kind, r.line, r.col, r.ord, r.enclosing \
         FROM manifests m JOIN name_refs r ON r.org_id = m.org_id AND r.blob_oid = m.blob_oid AND r.parser_rev = $4 \
         WHERE m.org_id = $1 AND m.repo_id = $2 AND m.sha = $3 AND r.name = $5 \
           AND ($6::text IS NULL OR r.qualifier = $6) \
           AND (cardinality($7::text[]) = 0 OR r.kind = ANY($7)) \
           AND ($8 OR m.class <> 'test') \
           AND ($9::text IS NULL OR (m.path COLLATE \"C\", r.line, r.col, r.ord) > ($9, $10, $11, $12)) \
         ORDER BY m.path COLLATE \"C\", r.line, r.col, r.ord LIMIT $13",
    )
    .bind(org.as_uuid())
    .bind(key.repo_id.as_uuid())
    .bind(key.sha.as_bytes().as_slice())
    .bind(parser)
    .bind(&q.name)
    .bind(q.qualifier.as_deref())
    .bind(&kinds)
    .bind(q.include_tests)
    .bind(after_path)
    .bind(after_line)
    .bind(after_col)
    .bind(after_ord)
    .bind(page.fetch())
    .fetch_all(&mut *conn)
    .await?;
    let hits = rows
        .into_iter()
        .map(|(path, blob, kind, line, col, ord, enclosing)| {
            let path = to_path(path)?;
            let blob = BlobOid::from_bytes(blob);
            let enclosing = enclosing.map(|n| to_u32(n, "ord")).transpose()?.map(|ord| SymbolRef { path: path.clone(), blob, ord });
            let hit = RefHit { path, pos: (to_u32(line, "line")?, to_u32(col, "column")?), kind: RefKind::from_db(&kind)?, enclosing };
            Ok((hit, ord))
        })
        .collect::<Result<Vec<_>, DbError>>()?;
    let Paged { items, next } = page.finish(hits, |(hit, ord)| {
        (hit.path.as_str().to_owned(), i32::try_from(hit.pos.0).unwrap_or(i32::MAX), i32::try_from(hit.pos.1).unwrap_or(i32::MAX), *ord)
    })?;
    Ok(Paged { items: items.into_iter().map(|(hit, _)| hit).collect(), next })
}
