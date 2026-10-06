//! Glossary (v1: a query-time union): symbol names, tables, endpoint / page keys, events, catalog
//! keys and texts at a version.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

use mage_core::domain::ids::{OrgId, RepoVersionKey};
use mage_core::domain::path::RepoPath;
use mage_core::domain::repo::IndexRevs;
use mage_core::ports::knowledge::{AppNodeKind, GlossaryHit, GlossarySource};
use sqlx::PgConnection;

use super::codec::{DbEnum, to_path, to_rev, to_u32};
use crate::pg::error::DbError;

/// `strpos(lower({text}), w) > 0` for some word `w` of `$4`.
fn contains_a_word(text: &str) -> String {
    format!("EXISTS (SELECT 1 FROM unnest($4::text[]) AS w WHERE strpos(lower({text}), w) > 0)")
}

fn node_source(kind: AppNodeKind) -> Option<GlossarySource> {
    match kind {
        AppNodeKind::Table => Some(GlossarySource::Table),
        AppNodeKind::Endpoint => Some(GlossarySource::Endpoint),
        AppNodeKind::Page => Some(GlossarySource::Page),
        AppNodeKind::Event => Some(GlossarySource::Event),
        _ => None,
    }
}

/// Weight = distinct words contained (case-insensitively) / `words.len()`. SQL narrows the rows to
/// those containing some word; the weight is computed here.
pub(super) async fn glossary_hits(
    conn: &mut PgConnection,
    org: OrgId,
    key: &RepoVersionKey,
    revs: Option<IndexRevs>,
    words: &[String],
    limit: u32,
) -> Result<Vec<GlossaryHit>, DbError> {
    if words.is_empty() {
        return Ok(Vec::new());
    }
    let mut distinct: Vec<&str> = words.iter().map(String::as_str).collect();
    distinct.sort_unstable();
    distinct.dedup();
    let weigh = |text: &str| {
        let lower = text.to_lowercase();
        let contained = distinct.iter().filter(|word| lower.contains(**word)).count();
        (contained > 0).then(|| contained as f32 / words.len() as f32)
    };
    let mut hits = Vec::new();
    let mut push = |source: GlossarySource, text: String, at: (RepoPath, u32)| {
        if let Some(weight) = weigh(&text) {
            hits.push(GlossaryHit { source, text, at, weight });
        }
    };

    if let Some(revs) = revs {
        let symbols: Vec<(String, String, i32)> = sqlx::query_as(&format!(
            "SELECT m.path, s.name, s.name_line \
             FROM manifests m JOIN symbols s ON s.org_id = m.org_id AND s.blob_oid = m.blob_oid AND s.parser_rev = $5 \
             WHERE m.org_id = $1 AND m.repo_id = $2 AND m.sha = $3 AND {}",
            contains_a_word("s.name")
        ))
        .bind(org.as_uuid())
        .bind(key.repo_id.as_uuid())
        .bind(key.sha.as_bytes().as_slice())
        .bind(&distinct)
        .bind(to_rev(revs.parser.0)?)
        .fetch_all(&mut *conn)
        .await?;
        for (path, name, line) in symbols {
            push(GlossarySource::Symbol, name, (to_path(path)?, to_u32(line, "line")?));
        }

        let entries: Vec<(String, String, String, i32)> = sqlx::query_as(&format!(
            "SELECT m.path, c.key, c.template, c.line \
             FROM manifests m JOIN catalog_entries c ON c.org_id = m.org_id AND c.blob_oid = m.blob_oid AND c.errors_rev = $5 \
             WHERE m.org_id = $1 AND m.repo_id = $2 AND m.sha = $3 AND ({} OR {})",
            contains_a_word("c.key"),
            contains_a_word("c.template")
        ))
        .bind(org.as_uuid())
        .bind(key.repo_id.as_uuid())
        .bind(key.sha.as_bytes().as_slice())
        .bind(&distinct)
        .bind(to_rev(revs.errors.0)?)
        .fetch_all(&mut *conn)
        .await?;
        for (path, key, template, line) in entries {
            let at = (to_path(path)?, to_u32(line, "line")?);
            push(GlossarySource::CatalogKey, key, at.clone());
            push(GlossarySource::CatalogText, template, at);
        }
    }

    let nodes: Vec<(String, String, String, i32)> = sqlx::query_as(&format!(
        "SELECT n.kind, n.key, n.path, n.line FROM app_nodes n \
         WHERE n.org_id = $1 AND n.repo_id = $2 AND n.sha = $3 AND n.kind IN ('table', 'endpoint', 'page', 'event') \
           AND n.path IS NOT NULL AND n.line IS NOT NULL AND {}",
        contains_a_word("n.key")
    ))
    .bind(org.as_uuid())
    .bind(key.repo_id.as_uuid())
    .bind(key.sha.as_bytes().as_slice())
    .bind(&distinct)
    .fetch_all(&mut *conn)
    .await?;
    for (kind, text, path, line) in nodes {
        if let Some(source) = node_source(AppNodeKind::from_db(&kind)?) {
            push(source, text, (to_path(path)?, to_u32(line, "line")?));
        }
    }

    hits.sort_by(|a, b| {
        b.weight.total_cmp(&a.weight).then(a.source.cmp(&b.source)).then_with(|| a.text.cmp(&b.text)).then_with(|| a.at.cmp(&b.at))
    });
    hits.dedup_by(|a, b| a.source == b.source && a.text == b.text && a.at == b.at);
    hits.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    Ok(hits)
}

/// `glossary_top` groups: `(source, text)` → (count, first location).
type TopGroups = BTreeMap<(GlossarySource, String), (u32, (RepoPath, u32))>;

fn add_top(groups: &mut TopGroups, source: GlossarySource, text: String, at: (RepoPath, u32)) {
    match groups.entry((source, text)) {
        Entry::Occupied(mut group) => {
            let (count, first) = group.get_mut();
            *count += 1;
            if at < *first {
                *first = at;
            }
        }
        Entry::Vacant(group) => {
            group.insert((1, at));
        }
    }
}

/// Last literal path segment of an endpoint key (`"GET /api/connections/{}"` → `connections`).
fn endpoint_resource(key: &str) -> Option<&str> {
    let path = key.split_once(' ').map_or(key, |(_, path)| path);
    path.split('/').rfind(|segment| !segment.is_empty() && *segment != "{}")
}

/// Tables, endpoint resources and exported type names by occurrence count, then text, then source.
pub(super) async fn glossary_top(
    conn: &mut PgConnection,
    org: OrgId,
    key: &RepoVersionKey,
    revs: Option<IndexRevs>,
    limit: u32,
) -> Result<Vec<GlossaryHit>, DbError> {
    let mut groups = TopGroups::new();
    let nodes: Vec<(String, String, String, i32)> = sqlx::query_as(
        "SELECT n.kind, n.key, n.path, n.line FROM app_nodes n \
         WHERE n.org_id = $1 AND n.repo_id = $2 AND n.sha = $3 AND n.kind IN ('table', 'endpoint') \
           AND n.path IS NOT NULL AND n.line IS NOT NULL",
    )
    .bind(org.as_uuid())
    .bind(key.repo_id.as_uuid())
    .bind(key.sha.as_bytes().as_slice())
    .fetch_all(&mut *conn)
    .await?;
    for (kind, text, path, line) in nodes {
        let at = (to_path(path)?, to_u32(line, "line")?);
        match AppNodeKind::from_db(&kind)? {
            AppNodeKind::Table => add_top(&mut groups, GlossarySource::Table, text, at),
            AppNodeKind::Endpoint => {
                if let Some(resource) = endpoint_resource(&text) {
                    add_top(&mut groups, GlossarySource::Endpoint, resource.to_owned(), at);
                }
            }
            _ => {}
        }
    }
    if let Some(revs) = revs {
        let types: Vec<(String, String, i32)> = sqlx::query_as(
            "SELECT m.path, s.name, s.name_line \
             FROM manifests m JOIN symbols s ON s.org_id = m.org_id AND s.blob_oid = m.blob_oid AND s.parser_rev = $4 \
             WHERE m.org_id = $1 AND m.repo_id = $2 AND m.sha = $3 AND s.exported \
               AND s.kind IN ('struct', 'class', 'interface', 'trait', 'enum', 'type_alias')",
        )
        .bind(org.as_uuid())
        .bind(key.repo_id.as_uuid())
        .bind(key.sha.as_bytes().as_slice())
        .bind(to_rev(revs.parser.0)?)
        .fetch_all(&mut *conn)
        .await?;
        for (path, name, line) in types {
            add_top(&mut groups, GlossarySource::Symbol, name, (to_path(path)?, to_u32(line, "line")?));
        }
    }
    let mut top: Vec<GlossaryHit> =
        groups.into_iter().map(|((source, text), (count, at))| GlossaryHit { source, text, at, weight: count as f32 }).collect();
    top.sort_by(|a, b| b.weight.total_cmp(&a.weight).then_with(|| a.text.cmp(&b.text)).then(a.source.cmp(&b.source)));
    top.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    Ok(top)
}
