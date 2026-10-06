//! Layer 3 (ErrorSites): `error_sites`, `catalog_entries` and the `ErrorSites` marker; matching
//! (knowledge.md "Matching"): token containment / overlap in SQL (GIN on `tokens`), pattern
//! verification and scoring in Rust.

use std::collections::{BTreeSet, HashMap, HashSet};

use mage_core::domain::ids::{BlobOid, OrgId, RepoVersionKey};
use mage_core::ports::knowledge::{
    CatalogEntryRecord, CatalogMatch, ErrorMatch, ErrorQuery, ErrorSiteKind, ErrorSiteRecord, ErrorsRev, FALLBACK_OVERLAP, LOW_SPECIFICITY,
    LogLevel, Paged, SymbolRef,
};
use parking_lot::Mutex;
use regex::{Regex, RegexBuilder};
use sqlx::PgConnection;
use time::OffsetDateTime;

use super::codec::{CHUNK, DbEnum, Page, to_i32, to_path, to_rev, to_u32};
use super::view::{Marker, ord, put_markers, resolve};
use crate::pg::error::DbError;

/// Compiled-pattern size limit of the verification regexes.
const REGEX_SIZE_LIMIT: usize = 1 << 20;

/// Patterns per generation of [`PatternCache`].
const PATTERN_CACHE_GENERATION: usize = 4_096;

/// Compiled verification patterns shared by every match of the store (knowledge.md: an LRU of
/// compiled `Regex`es). Two generations approximate LRU in O(1): a hit in the old generation moves
/// the pattern to the young one; a full young generation retires the old one. `None` caches an
/// uncompilable pattern.
#[derive(Default)]
pub(super) struct PatternCache {
    generations: Mutex<Generations>,
}

#[derive(Default)]
struct Generations {
    young: HashMap<String, Option<Regex>>,
    old: HashMap<String, Option<Regex>>,
}

impl PatternCache {
    fn get(&self, pattern: &str) -> Option<Regex> {
        let promoted = {
            let mut generations = self.generations.lock();
            if let Some(regex) = generations.young.get(pattern) {
                return regex.clone();
            }
            generations.old.remove(pattern)
        };
        // Compiling happens outside the lock.
        let regex = promoted.unwrap_or_else(|| RegexBuilder::new(pattern).size_limit(REGEX_SIZE_LIMIT).build().ok());
        let mut generations = self.generations.lock();
        if generations.young.len() >= PATTERN_CACHE_GENERATION {
            generations.old = std::mem::take(&mut generations.young);
        }
        generations.young.insert(pattern.to_owned(), regex.clone());
        regex
    }
}

/// Per-row `text[]` columns through `UNNEST`: one flat array plus 1-based inclusive bounds per
/// row (`flat[lo:hi]`; `lo > hi` is the empty array).
#[derive(Default)]
pub(super) struct FlatArrays<'a> {
    pub flat: Vec<&'a str>,
    pub lo: Vec<i32>,
    pub hi: Vec<i32>,
}

impl<'a> FlatArrays<'a> {
    pub fn push(&mut self, items: &'a [String]) -> Result<(), DbError> {
        let too_many = || DbError::Invalid("too many array elements in one batch".into());
        self.lo.push(i32::try_from(self.flat.len() + 1).map_err(|_| too_many())?);
        self.flat.extend(items.iter().map(String::as_str));
        self.hi.push(i32::try_from(self.flat.len()).map_err(|_| too_many())?);
        Ok(())
    }
}

fn ok_markers(blobs: &[BlobOid]) -> Vec<Marker<'static>> {
    let mut seen = HashSet::new();
    blobs.iter().filter(|blob| seen.insert(**blob)).map(|blob| Marker { blob: *blob, lang: None, status: "ok", error: None }).collect()
}

/// Rows (first of a `(blob, ord)` wins) plus the `ErrorSites(rev)` marker per blob.
pub(super) async fn put_error_sites(
    conn: &mut PgConnection,
    org: OrgId,
    rev: ErrorsRev,
    blobs: &[(BlobOid, Vec<ErrorSiteRecord>)],
    now: OffsetDateTime,
) -> Result<(), DbError> {
    let rev = to_rev(rev.0)?;
    let oids: Vec<BlobOid> = blobs.iter().map(|(blob, _)| *blob).collect();
    put_markers(conn, org, "error_sites", rev, &ok_markers(&oids), now).await?;
    let mut seen = HashSet::new();
    let rows: Vec<(BlobOid, &ErrorSiteRecord)> =
        blobs.iter().flat_map(|(blob, sites)| sites.iter().map(move |site| (*blob, site))).filter(|(blob, site)| seen.insert((*blob, site.ord))).collect();
    for chunk in rows.chunks(CHUNK) {
        let n = chunk.len();
        let mut tokens = FlatArrays::default();
        let (mut blobs, mut ords, mut literal_ords, mut kinds, mut levels, mut templates, mut patterns) =
            (Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n));
        let (mut specificity, mut lines, mut cols, mut enclosing, mut statuses, mut keys) =
            (Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n));
        for (blob, site) in chunk {
            blobs.push(blob.0);
            ords.push(ord(site.ord)?);
            literal_ords.push(ord(site.literal_ord)?);
            kinds.push(site.kind.as_db());
            levels.push(site.level.map(DbEnum::as_db));
            templates.push(site.template.as_str());
            patterns.push(site.pattern.as_str());
            tokens.push(&site.tokens)?;
            specificity.push(to_i32(site.specificity, "specificity")?);
            lines.push(to_i32(site.pos.0, "line")?);
            cols.push(to_i32(site.pos.1, "column")?);
            enclosing.push(site.enclosing.map(ord).transpose()?);
            statuses.push(
                site.http_status
                    .map(|status| i16::try_from(status).map_err(|_| DbError::Invalid(format!("http status {status} exceeds the stored range"))))
                    .transpose()?,
            );
            keys.push(site.i18n_key.as_deref());
        }
        sqlx::query(
            "INSERT INTO error_sites (org_id, blob_oid, errors_rev, ord, literal_ord, kind, level, template, pattern, tokens, \
                                      specificity, line, col, enclosing_ord, http_status, i18n_key) \
             SELECT $1, u.blob, $2, u.ord, u.literal_ord, u.kind, u.level, u.template, u.pattern, ($3::text[])[u.lo:u.hi], \
                    u.specificity, u.line, u.col, u.enclosing, u.http_status, u.i18n_key \
             FROM UNNEST($4::bytea[], $5::int4[], $6::int4[], $7::text[], $8::text[], $9::text[], $10::text[], $11::int4[], $12::int4[], \
                         $13::int4[], $14::int4[], $15::int4[], $16::int4[], $17::int2[], $18::text[]) \
                  AS u(blob, ord, literal_ord, kind, level, template, pattern, lo, hi, specificity, line, col, enclosing, http_status, i18n_key) \
             ON CONFLICT DO NOTHING",
        )
        .bind(org.as_uuid())
        .bind(rev)
        .bind(&tokens.flat)
        .bind(&blobs)
        .bind(&ords)
        .bind(&literal_ords)
        .bind(&kinds)
        .bind(&levels)
        .bind(&templates)
        .bind(&patterns)
        .bind(&tokens.lo)
        .bind(&tokens.hi)
        .bind(&specificity)
        .bind(&lines)
        .bind(&cols)
        .bind(&enclosing)
        .bind(&statuses)
        .bind(&keys)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Rows keyed by catalog key (first of a `(blob, key)` wins) plus the `ErrorSites(rev)` marker per blob.
pub(super) async fn put_catalog(
    conn: &mut PgConnection,
    org: OrgId,
    rev: ErrorsRev,
    blobs: &[(BlobOid, Vec<CatalogEntryRecord>)],
    now: OffsetDateTime,
) -> Result<(), DbError> {
    let rev = to_rev(rev.0)?;
    let oids: Vec<BlobOid> = blobs.iter().map(|(blob, _)| *blob).collect();
    put_markers(conn, org, "error_sites", rev, &ok_markers(&oids), now).await?;
    let mut seen = HashSet::new();
    let rows: Vec<(BlobOid, &CatalogEntryRecord)> = blobs
        .iter()
        .flat_map(|(blob, entries)| entries.iter().map(move |entry| (*blob, entry)))
        .filter(|(blob, entry)| seen.insert((*blob, entry.key.as_str())))
        .collect();
    for chunk in rows.chunks(CHUNK) {
        let mut tokens = FlatArrays::default();
        let blobs: Vec<[u8; 20]> = chunk.iter().map(|(blob, _)| blob.0).collect();
        let keys: Vec<&str> = chunk.iter().map(|(_, e)| e.key.as_str()).collect();
        let locales: Vec<Option<&str>> = chunk.iter().map(|(_, e)| e.locale.as_deref()).collect();
        let templates: Vec<&str> = chunk.iter().map(|(_, e)| e.template.as_str()).collect();
        let patterns: Vec<&str> = chunk.iter().map(|(_, e)| e.pattern.as_str()).collect();
        for (_, entry) in chunk {
            tokens.push(&entry.tokens)?;
        }
        let specificity = chunk.iter().map(|(_, e)| to_i32(e.specificity, "specificity")).collect::<Result<Vec<_>, _>>()?;
        let lines = chunk.iter().map(|(_, e)| to_i32(e.line, "line")).collect::<Result<Vec<_>, _>>()?;
        sqlx::query(
            "INSERT INTO catalog_entries (org_id, blob_oid, errors_rev, key, locale, template, pattern, tokens, specificity, line) \
             SELECT $1, u.blob, $2, u.key, u.locale, u.template, u.pattern, ($3::text[])[u.lo:u.hi], u.specificity, u.line \
             FROM UNNEST($4::bytea[], $5::text[], $6::text[], $7::text[], $8::text[], $9::int4[], $10::int4[], $11::int4[], $12::int4[]) \
                  AS u(blob, key, locale, template, pattern, lo, hi, specificity, line) \
             ON CONFLICT DO NOTHING",
        )
        .bind(org.as_uuid())
        .bind(rev)
        .bind(&tokens.flat)
        .bind(&blobs)
        .bind(&keys)
        .bind(&locales)
        .bind(&templates)
        .bind(&patterns)
        .bind(&tokens.lo)
        .bind(&tokens.hi)
        .bind(&specificity)
        .bind(&lines)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// `error_sites` columns (alias `e`) decoded by [`SiteRow`].
const SITE_COLUMNS: &str = "e.ord, e.literal_ord, e.kind, e.level, e.template, e.pattern, e.tokens, e.specificity, e.line, e.col, \
                            e.enclosing_ord, e.http_status, e.i18n_key";

#[derive(sqlx::FromRow)]
struct SiteRow {
    ord: i32,
    literal_ord: i32,
    kind: String,
    level: Option<String>,
    template: String,
    pattern: String,
    tokens: Vec<String>,
    specificity: i32,
    line: i32,
    col: i32,
    enclosing_ord: Option<i32>,
    http_status: Option<i16>,
    i18n_key: Option<String>,
}

impl SiteRow {
    fn into_record(self) -> Result<ErrorSiteRecord, DbError> {
        Ok(ErrorSiteRecord {
            ord: to_u32(self.ord, "ord")?,
            literal_ord: to_u32(self.literal_ord, "ord")?,
            kind: ErrorSiteKind::from_db(&self.kind)?,
            level: self.level.as_deref().map(LogLevel::from_db).transpose()?,
            template: self.template,
            pattern: self.pattern,
            tokens: self.tokens,
            specificity: to_u32(self.specificity, "specificity")?,
            pos: (to_u32(self.line, "line")?, to_u32(self.col, "column")?),
            enclosing: self.enclosing_ord.map(|n| to_u32(n, "ord")).transpose()?,
            http_status: self
                .http_status
                .map(|status| u16::try_from(status).map_err(|_| DbError::Invalid(format!("stored http status {status} is negative"))))
                .transpose()?,
            i18n_key: self.i18n_key,
        })
    }
}

/// An error site with the manifest path and blob it is seen at.
#[derive(sqlx::FromRow)]
struct SiteAtRow {
    path: String,
    blob_oid: [u8; 20],
    #[sqlx(flatten)]
    site: SiteRow,
}

impl SiteAtRow {
    /// A match that needs no verification (`I18nUse` lookups, sites in a symbol).
    fn into_exact_match(self) -> Result<ErrorMatch, DbError> {
        Ok(ErrorMatch {
            path: to_path(self.path)?,
            blob: BlobOid::from_bytes(self.blob_oid),
            site: self.site.into_record()?,
            segment: 0,
            span: (0, 0),
            verified: true,
            score: 1.0,
        })
    }
}

/// A stored pattern row (`error_sites`, `catalog_entries`).
trait PatternRow {
    fn tokens(&self) -> &[String];
    fn specificity(&self) -> u32;
    fn template(&self) -> &str;
    fn pattern(&self) -> &str;
}

impl PatternRow for ErrorSiteRecord {
    fn tokens(&self) -> &[String] {
        &self.tokens
    }
    fn specificity(&self) -> u32 {
        self.specificity
    }
    fn template(&self) -> &str {
        &self.template
    }
    fn pattern(&self) -> &str {
        &self.pattern
    }
}

impl PatternRow for CatalogEntryRecord {
    fn tokens(&self) -> &[String] {
        &self.tokens
    }
    fn specificity(&self) -> u32 {
        self.specificity
    }
    fn template(&self) -> &str {
        &self.template
    }
    fn pattern(&self) -> &str {
        &self.pattern
    }
}

/// A match of `rows[row]`.
struct PatternHit {
    row: usize,
    segment: u16,
    span: (u32, u32),
    verified: bool,
    score: f32,
}

fn to_usize(n: u32) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

/// knowledge.md "Matching" steps 2-5 over `rows` in the method's (path, ord | key) order: the first
/// `max_candidates` rows whose tokens all appear in the input are verified by their pattern;
/// below `limit` verified, rows with token overlap >= [`FALLBACK_OVERLAP`] follow unverified.
/// `rows` must hold every row that shares a token with the input (or has none).
fn match_rows<R: PatternRow>(rows: &[&R], q: &ErrorQuery, patterns: &PatternCache) -> Vec<PatternHit> {
    let input: BTreeSet<&str> = q.tokens.iter().map(String::as_str).collect();
    let limit = to_usize(q.limit);
    let specific_enough = |row: &R| {
        let low = row.specificity() < LOW_SPECIFICITY || row.tokens().len() < 2;
        !low || q.include_low_specificity || q.segments.iter().any(|segment| segment.trim() == row.template())
    };
    let mut hits = Vec::new();
    let mut verified = BTreeSet::new();
    let candidates = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| row.tokens().iter().all(|token| input.contains(token.as_str())))
        .take(to_usize(q.max_candidates));
    for (i, row) in candidates {
        if !specific_enough(row) {
            continue;
        }
        if let Some(hit) = verify(i, *row, &q.segments, patterns) {
            verified.insert(i);
            hits.push(hit);
        }
    }
    if verified.len() < limit {
        for (i, row) in rows.iter().enumerate() {
            let tokens = row.tokens();
            if verified.contains(&i) || tokens.is_empty() || !specific_enough(row) {
                continue;
            }
            let common = tokens.iter().filter(|token| input.contains(token.as_str())).count();
            let overlap = common as f32 / tokens.len() as f32;
            if overlap >= FALLBACK_OVERLAP {
                hits.push(PatternHit { row: i, segment: 0, span: (0, 0), verified: false, score: 0.4 * overlap });
            }
        }
    }
    hits.sort_by(|a, b| b.verified.cmp(&a.verified).then(b.score.total_cmp(&a.score)).then(a.row.cmp(&b.row)));
    hits.truncate(limit);
    hits
}

/// The row's pattern on the first segment it matches (an uncompilable pattern never matches).
fn verify<R: PatternRow>(row_index: usize, row: &R, segments: &[String], patterns: &PatternCache) -> Option<PatternHit> {
    let regex = patterns.get(row.pattern())?;
    segments.iter().enumerate().find_map(|(i, segment)| {
        let found = regex.find(segment)?;
        let ratio = (row.specificity() as f32 / segment.len() as f32).min(1.0);
        Some(PatternHit {
            row: row_index,
            segment: u16::try_from(i).unwrap_or(u16::MAX),
            span: (u32::try_from(found.start()).unwrap_or(u32::MAX), u32::try_from(found.end()).unwrap_or(u32::MAX)),
            verified: true,
            score: 0.5 + 0.5 * ratio,
        })
    })
}

pub(super) async fn match_errors(
    conn: &mut PgConnection,
    org: OrgId,
    key: &RepoVersionKey,
    rev: ErrorsRev,
    q: &ErrorQuery,
    patterns: &PatternCache,
) -> Result<Vec<ErrorMatch>, DbError> {
    let rows: Vec<SiteAtRow> = sqlx::query_as(&format!(
        "SELECT m.path, m.blob_oid, {SITE_COLUMNS} \
         FROM manifests m JOIN error_sites e ON e.org_id = m.org_id AND e.blob_oid = m.blob_oid AND e.errors_rev = $4 \
         WHERE m.org_id = $1 AND m.repo_id = $2 AND m.sha = $3 AND (e.tokens <@ $5 OR e.tokens && $5) \
         ORDER BY m.path COLLATE \"C\", e.ord"
    ))
    .bind(org.as_uuid())
    .bind(key.repo_id.as_uuid())
    .bind(key.sha.as_bytes().as_slice())
    .bind(to_rev(rev.0)?)
    .bind(&q.tokens)
    .fetch_all(&mut *conn)
    .await?;
    let rows = rows
        .into_iter()
        .map(|row| Ok((row.path, BlobOid::from_bytes(row.blob_oid), row.site.into_record()?)))
        .collect::<Result<Vec<_>, DbError>>()?;
    let records: Vec<&ErrorSiteRecord> = rows.iter().map(|(_, _, site)| site).collect();
    match_rows(&records, q, patterns)
        .into_iter()
        .map(|hit| {
            let (path, blob, site) = &rows[hit.row];
            Ok(ErrorMatch {
                path: to_path(path.clone())?,
                blob: *blob,
                site: site.clone(),
                segment: hit.segment,
                span: hit.span,
                verified: hit.verified,
                score: hit.score,
            })
        })
        .collect()
}

pub(super) async fn match_catalog(
    conn: &mut PgConnection,
    org: OrgId,
    key: &RepoVersionKey,
    rev: ErrorsRev,
    q: &ErrorQuery,
    patterns: &PatternCache,
) -> Result<Vec<CatalogMatch>, DbError> {
    let rows: Vec<(String, String, Option<String>, String, String, Vec<String>, i32, i32)> = sqlx::query_as(
        "SELECT m.path, c.key, c.locale, c.template, c.pattern, c.tokens, c.specificity, c.line \
         FROM manifests m JOIN catalog_entries c ON c.org_id = m.org_id AND c.blob_oid = m.blob_oid AND c.errors_rev = $4 \
         WHERE m.org_id = $1 AND m.repo_id = $2 AND m.sha = $3 AND (c.tokens <@ $5 OR c.tokens && $5) \
         ORDER BY m.path COLLATE \"C\", c.key COLLATE \"C\"",
    )
    .bind(org.as_uuid())
    .bind(key.repo_id.as_uuid())
    .bind(key.sha.as_bytes().as_slice())
    .bind(to_rev(rev.0)?)
    .bind(&q.tokens)
    .fetch_all(&mut *conn)
    .await?;
    let rows = rows
        .into_iter()
        .map(|(path, key, locale, template, pattern, tokens, specificity, line)| {
            let entry = CatalogEntryRecord {
                key,
                locale,
                template,
                pattern,
                tokens,
                specificity: to_u32(specificity, "specificity")?,
                line: to_u32(line, "line")?,
            };
            Ok((path, entry))
        })
        .collect::<Result<Vec<_>, DbError>>()?;
    let records: Vec<&CatalogEntryRecord> = rows.iter().map(|(_, entry)| entry).collect();
    match_rows(&records, q, patterns)
        .into_iter()
        .map(|hit| {
            let (path, entry) = &rows[hit.row];
            Ok(CatalogMatch {
                path: to_path(path.clone())?,
                entry: entry.clone(),
                segment: hit.segment,
                span: hit.span,
                verified: hit.verified,
                score: hit.score,
            })
        })
        .collect()
}

/// Keyset of `sites_for_i18n_key` pages: path, line, column, ord.
pub(super) type SiteKey = (String, i32, i32, i32);

pub(super) async fn sites_for_i18n_key(
    conn: &mut PgConnection,
    org: OrgId,
    key: &RepoVersionKey,
    rev: ErrorsRev,
    i18n_key: &str,
    page: &Page<SiteKey>,
) -> Result<Paged<ErrorMatch>, DbError> {
    let (after_path, after_line, after_col, after_ord) = match &page.after {
        Some((path, line, col, ord)) => (Some(path.as_str()), *line, *col, *ord),
        None => (None, 0, 0, 0),
    };
    let rows: Vec<SiteAtRow> = sqlx::query_as(&format!(
        "SELECT m.path, m.blob_oid, {SITE_COLUMNS} \
         FROM manifests m JOIN error_sites e ON e.org_id = m.org_id AND e.blob_oid = m.blob_oid AND e.errors_rev = $4 \
         WHERE m.org_id = $1 AND m.repo_id = $2 AND m.sha = $3 AND e.kind = 'i18n_use' AND e.i18n_key = $5 \
           AND ($6::text IS NULL OR (m.path COLLATE \"C\", e.line, e.col, e.ord) > ($6, $7, $8, $9)) \
         ORDER BY m.path COLLATE \"C\", e.line, e.col, e.ord LIMIT $10"
    ))
    .bind(org.as_uuid())
    .bind(key.repo_id.as_uuid())
    .bind(key.sha.as_bytes().as_slice())
    .bind(to_rev(rev.0)?)
    .bind(i18n_key)
    .bind(after_path)
    .bind(after_line)
    .bind(after_col)
    .bind(after_ord)
    .bind(page.fetch())
    .fetch_all(&mut *conn)
    .await?;
    let matches = rows.into_iter().map(SiteAtRow::into_exact_match).collect::<Result<Vec<_>, DbError>>()?;
    page.finish(matches, |m| {
        let int = |n: u32| i32::try_from(n).unwrap_or(i32::MAX);
        (m.path.as_str().to_owned(), int(m.site.pos.0), int(m.site.pos.1), int(m.site.ord))
    })
}

/// Sites of the symbol's blob inside its span, by position (then ord).
pub(super) async fn error_sites_in(
    conn: &mut PgConnection,
    org: OrgId,
    key: &RepoVersionKey,
    parser: i16,
    rev: ErrorsRev,
    symbol: &SymbolRef,
    limit: u32,
) -> Result<Vec<ErrorMatch>, DbError> {
    let Some((_, def)) = resolve(conn, org, key, parser, std::slice::from_ref(symbol)).await?.into_iter().next() else {
        return Ok(Vec::new());
    };
    let rows: Vec<SiteAtRow> = sqlx::query_as(&format!(
        "SELECT $5::text AS path, e.blob_oid, {SITE_COLUMNS} FROM error_sites e \
         WHERE e.org_id = $1 AND e.blob_oid = $2 AND e.errors_rev = $3 AND e.line BETWEEN $6 AND $7 \
         ORDER BY e.line, e.col, e.ord LIMIT $4"
    ))
    .bind(org.as_uuid())
    .bind(symbol.blob.0)
    .bind(to_rev(rev.0)?)
    .bind(i64::from(limit))
    .bind(symbol.path.as_str())
    .bind(i64::from(def.span.start()))
    .bind(i64::from(def.span.end()))
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter().map(SiteAtRow::into_exact_match).collect()
}
