//! Layer 4 (AppMap): blob-keyed `app_facts` with the `AppFacts` marker, and the per-version graph
//! (`app_nodes`, `app_edges`, replaced in one transaction).

use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};

use mage_core::domain::ids::{BlobOid, OrgId, RepoVersionKey};
use mage_core::ports::knowledge::{
    AppEdge, AppEdgeKind, AppFact, AppFactAt, AppFactRecord, AppGraph, AppMapRev, AppNode, AppNodeKind, FrameworkId, Paged, SymbolRef,
};
use sqlx::PgConnection;
use sqlx::types::Json;
use time::OffsetDateTime;

use super::codec::{CHUNK, DbEnum, Page, fact_kind, to_i32, to_path, to_rev, to_u32};
use super::view::{Marker, ord, put_markers};
use crate::pg::error::DbError;

/// Rows (first of a `(blob, ord)` wins) plus the `AppFacts(rev)` marker per blob.
pub(super) async fn put_app_facts(
    conn: &mut PgConnection,
    org: OrgId,
    rev: AppMapRev,
    blobs: &[(BlobOid, Vec<AppFactRecord>)],
    now: OffsetDateTime,
) -> Result<(), DbError> {
    let rev = to_rev(rev.0)?;
    let mut marked = HashSet::new();
    let markers: Vec<Marker<'static>> = blobs
        .iter()
        .filter(|(blob, _)| marked.insert(*blob))
        .map(|(blob, _)| Marker { blob: *blob, lang: None, status: "ok", error: None })
        .collect();
    put_markers(conn, org, "app_facts", rev, &markers, now).await?;
    let mut seen = HashSet::new();
    let rows: Vec<(BlobOid, &AppFactRecord)> =
        blobs.iter().flat_map(|(blob, facts)| facts.iter().map(move |fact| (*blob, fact))).filter(|(blob, fact)| seen.insert((*blob, fact.ord))).collect();
    for chunk in rows.chunks(CHUNK) {
        let blobs: Vec<[u8; 20]> = chunk.iter().map(|(blob, _)| blob.0).collect();
        let ords = chunk.iter().map(|(_, f)| ord(f.ord)).collect::<Result<Vec<_>, _>>()?;
        let extractors: Vec<&str> = chunk.iter().map(|(_, f)| f.framework.as_db()).collect();
        let kinds: Vec<&str> = chunk.iter().map(|(_, f)| fact_kind(&f.fact)).collect();
        let data: Vec<Json<&AppFact>> = chunk.iter().map(|(_, f)| Json(&f.fact)).collect();
        let lines = chunk.iter().map(|(_, f)| to_i32(f.line, "line")).collect::<Result<Vec<_>, _>>()?;
        let enclosing = chunk.iter().map(|(_, f)| f.enclosing.map(ord).transpose()).collect::<Result<Vec<_>, _>>()?;
        sqlx::query(
            "INSERT INTO app_facts (org_id, blob_oid, appmap_rev, ord, extractor, kind, data, line, enclosing_ord) \
             SELECT $1, u.blob, $2, u.ord, u.extractor, u.kind, u.data, u.line, u.enclosing \
             FROM UNNEST($3::bytea[], $4::int4[], $5::text[], $6::text[], $7::jsonb[], $8::int4[], $9::int4[]) \
                  AS u(blob, ord, extractor, kind, data, line, enclosing) \
             ON CONFLICT DO NOTHING",
        )
        .bind(org.as_uuid())
        .bind(rev)
        .bind(&blobs)
        .bind(&ords)
        .bind(&extractors)
        .bind(&kinds)
        .bind(&data)
        .bind(&lines)
        .bind(&enclosing)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Facts of the manifest blobs at `rev`, by path, then ord.
pub(super) async fn app_facts_at(conn: &mut PgConnection, org: OrgId, key: &RepoVersionKey, rev: AppMapRev) -> Result<Vec<AppFactAt>, DbError> {
    let rows: Vec<(String, [u8; 20], i32, String, Json<AppFact>, i32, Option<i32>)> = sqlx::query_as(
        "SELECT m.path, m.blob_oid, f.ord, f.extractor, f.data, f.line, f.enclosing_ord \
         FROM manifests m JOIN app_facts f ON f.org_id = m.org_id AND f.blob_oid = m.blob_oid AND f.appmap_rev = $4 \
         WHERE m.org_id = $1 AND m.repo_id = $2 AND m.sha = $3 ORDER BY m.path COLLATE \"C\", f.ord",
    )
    .bind(org.as_uuid())
    .bind(key.repo_id.as_uuid())
    .bind(key.sha.as_bytes().as_slice())
    .bind(to_rev(rev.0)?)
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter()
        .map(|(path, blob, ord, extractor, Json(fact), line, enclosing)| {
            Ok(AppFactAt {
                path: to_path(path)?,
                blob: BlobOid::from_bytes(blob),
                fact: AppFactRecord {
                    ord: to_u32(ord, "ord")?,
                    framework: FrameworkId::from_db(&extractor)?,
                    fact,
                    line: to_u32(line, "line")?,
                    enclosing: enclosing.map(|n| to_u32(n, "ord")).transpose()?,
                },
            })
        })
        .collect()
}

/// Edges of `graph` with repeats of a `(from, to, kind)` dropped (the first wins); duplicate node
/// ids and edges to unknown nodes are `Invalid`.
pub(super) fn checked_edges(graph: &AppGraph) -> Result<Vec<&AppEdge>, DbError> {
    let mut ids = HashSet::new();
    for node in &graph.nodes {
        if !ids.insert(node.id) {
            return Err(DbError::Invalid(format!("duplicate app node id {}", node.id)));
        }
    }
    let mut keys = HashSet::new();
    let mut edges = Vec::with_capacity(graph.edges.len());
    for edge in &graph.edges {
        if !ids.contains(&edge.from) || !ids.contains(&edge.to) {
            return Err(DbError::Invalid(format!("app edge {} -> {} names an unknown node", edge.from, edge.to)));
        }
        if keys.insert((edge.from, edge.to, edge.kind)) {
            edges.push(edge);
        }
    }
    Ok(edges)
}

pub(super) async fn replace_app_graph(
    conn: &mut PgConnection,
    org: OrgId,
    key: &RepoVersionKey,
    nodes: &[AppNode],
    edges: &[&AppEdge],
) -> Result<(), DbError> {
    for table in ["app_edges", "app_nodes"] {
        sqlx::query(&format!("DELETE FROM {table} WHERE org_id = $1 AND repo_id = $2 AND sha = $3"))
            .bind(org.as_uuid())
            .bind(key.repo_id.as_uuid())
            .bind(key.sha.as_bytes().as_slice())
            .execute(&mut *conn)
            .await?;
    }
    for chunk in nodes.chunks(CHUNK) {
        let ids = chunk.iter().map(|n| to_i32(n.id, "app node id")).collect::<Result<Vec<_>, _>>()?;
        let kinds: Vec<&str> = chunk.iter().map(|n| n.kind.as_db()).collect();
        let keys: Vec<&str> = chunk.iter().map(|n| n.key.as_str()).collect();
        let labels: Vec<&str> = chunk.iter().map(|n| n.label.as_str()).collect();
        let frameworks: Vec<Option<&str>> = chunk.iter().map(|n| n.framework.map(DbEnum::as_db)).collect();
        let paths: Vec<Option<&str>> = chunk.iter().map(|n| n.at.as_ref().map(|(path, _)| path.as_str())).collect();
        let lines = chunk.iter().map(|n| n.at.as_ref().map(|(_, line)| to_i32(*line, "line")).transpose()).collect::<Result<Vec<_>, _>>()?;
        let symbol_paths: Vec<Option<&str>> = chunk.iter().map(|n| n.symbol.as_ref().map(|s| s.path.as_str())).collect();
        let symbol_blobs: Vec<Option<[u8; 20]>> = chunk.iter().map(|n| n.symbol.as_ref().map(|s| s.blob.0)).collect();
        let symbol_ords = chunk.iter().map(|n| n.symbol.as_ref().map(|s| ord(s.ord)).transpose()).collect::<Result<Vec<_>, _>>()?;
        sqlx::query(
            "INSERT INTO app_nodes (org_id, repo_id, sha, id, kind, key, label, framework, path, line, symbol_path, symbol_blob, symbol_ord) \
             SELECT $1, $2, $3, u.* FROM UNNEST($4::int4[], $5::text[], $6::text[], $7::text[], $8::text[], $9::text[], $10::int4[], \
                                                $11::text[], $12::bytea[], $13::int4[]) AS u",
        )
        .bind(org.as_uuid())
        .bind(key.repo_id.as_uuid())
        .bind(key.sha.as_bytes().as_slice())
        .bind(&ids)
        .bind(&kinds)
        .bind(&keys)
        .bind(&labels)
        .bind(&frameworks)
        .bind(&paths)
        .bind(&lines)
        .bind(&symbol_paths)
        .bind(&symbol_blobs)
        .bind(&symbol_ords)
        .execute(&mut *conn)
        .await?;
    }
    for chunk in edges.chunks(CHUNK) {
        let from = chunk.iter().map(|e| to_i32(e.from, "app node id")).collect::<Result<Vec<_>, _>>()?;
        let to = chunk.iter().map(|e| to_i32(e.to, "app node id")).collect::<Result<Vec<_>, _>>()?;
        let kinds: Vec<&str> = chunk.iter().map(|e| e.kind.as_db()).collect();
        let confidence: Vec<f32> = chunk.iter().map(|e| e.confidence).collect();
        let paths: Vec<Option<&str>> = chunk.iter().map(|e| e.evidence.as_ref().map(|(path, _)| path.as_str())).collect();
        let lines =
            chunk.iter().map(|e| e.evidence.as_ref().map(|(_, line)| to_i32(*line, "line")).transpose()).collect::<Result<Vec<_>, _>>()?;
        sqlx::query(
            "INSERT INTO app_edges (org_id, repo_id, sha, src, dst, kind, confidence, evidence_path, evidence_line) \
             SELECT $1, $2, $3, u.* FROM UNNEST($4::int4[], $5::int4[], $6::text[], $7::float4[], $8::text[], $9::int4[]) AS u",
        )
        .bind(org.as_uuid())
        .bind(key.repo_id.as_uuid())
        .bind(key.sha.as_bytes().as_slice())
        .bind(&from)
        .bind(&to)
        .bind(&kinds)
        .bind(&confidence)
        .bind(&paths)
        .bind(&lines)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

const NODE_COLUMNS: &str = "n.id, n.kind, n.key, n.label, n.framework, n.path, n.line, n.symbol_path, n.symbol_blob, n.symbol_ord";

#[derive(sqlx::FromRow)]
struct NodeRow {
    id: i32,
    kind: String,
    key: String,
    label: String,
    framework: Option<String>,
    path: Option<String>,
    line: Option<i32>,
    symbol_path: Option<String>,
    symbol_blob: Option<[u8; 20]>,
    symbol_ord: Option<i32>,
}

impl NodeRow {
    fn into_node(self) -> Result<AppNode, DbError> {
        let at = match (self.path, self.line) {
            (Some(path), Some(line)) => Some((to_path(path)?, to_u32(line, "line")?)),
            (None, None) => None,
            _ => return Err(DbError::Invalid(format!("stored app node {} has half a location", self.id))),
        };
        let symbol = match (self.symbol_path, self.symbol_blob, self.symbol_ord) {
            (Some(path), Some(blob), Some(ord)) => Some(SymbolRef { path: to_path(path)?, blob: BlobOid::from_bytes(blob), ord: to_u32(ord, "ord")? }),
            (None, None, None) => None,
            _ => return Err(DbError::Invalid(format!("stored app node {} has half a symbol", self.id))),
        };
        Ok(AppNode {
            id: to_u32(self.id, "app node id")?,
            kind: AppNodeKind::from_db(&self.kind)?,
            key: self.key,
            label: self.label,
            framework: self.framework.as_deref().map(FrameworkId::from_db).transpose()?,
            at,
            symbol,
        })
    }
}

/// Nodes of `kinds` (empty = all) by id, after node id `page.after`.
pub(super) async fn app_nodes(
    conn: &mut PgConnection,
    org: OrgId,
    key: &RepoVersionKey,
    kinds: &[AppNodeKind],
    page: &Page<u32>,
) -> Result<Paged<AppNode>, DbError> {
    let kinds: Vec<&str> = kinds.iter().map(|kind| kind.as_db()).collect();
    let rows: Vec<NodeRow> = sqlx::query_as(&format!(
        "SELECT {NODE_COLUMNS} FROM app_nodes n \
         WHERE n.org_id = $1 AND n.repo_id = $2 AND n.sha = $3 AND (cardinality($4::text[]) = 0 OR n.kind = ANY($4)) \
           AND ($5::int8 IS NULL OR n.id > $5) \
         ORDER BY n.id LIMIT $6"
    ))
    .bind(org.as_uuid())
    .bind(key.repo_id.as_uuid())
    .bind(key.sha.as_bytes().as_slice())
    .bind(&kinds)
    .bind(page.after.map(i64::from))
    .bind(page.fetch())
    .fetch_all(&mut *conn)
    .await?;
    let nodes = rows.into_iter().map(NodeRow::into_node).collect::<Result<Vec<_>, DbError>>()?;
    page.finish(nodes, |node| node.id)
}

/// The whole graph of `key`: nodes by id, edges by (from, to, kind).
async fn graph(conn: &mut PgConnection, org: OrgId, key: &RepoVersionKey) -> Result<AppGraph, DbError> {
    let nodes: Vec<NodeRow> = sqlx::query_as(&format!(
        "SELECT {NODE_COLUMNS} FROM app_nodes n WHERE n.org_id = $1 AND n.repo_id = $2 AND n.sha = $3 ORDER BY n.id"
    ))
    .bind(org.as_uuid())
    .bind(key.repo_id.as_uuid())
    .bind(key.sha.as_bytes().as_slice())
    .fetch_all(&mut *conn)
    .await?;
    let edges: Vec<(i32, i32, String, f32, Option<String>, Option<i32>)> = sqlx::query_as(
        "SELECT src, dst, kind, confidence, evidence_path, evidence_line FROM app_edges WHERE org_id = $1 AND repo_id = $2 AND sha = $3",
    )
    .bind(org.as_uuid())
    .bind(key.repo_id.as_uuid())
    .bind(key.sha.as_bytes().as_slice())
    .fetch_all(&mut *conn)
    .await?;
    let nodes = nodes.into_iter().map(NodeRow::into_node).collect::<Result<Vec<_>, DbError>>()?;
    let mut edges = edges
        .into_iter()
        .map(|(src, dst, kind, confidence, path, line)| {
            let evidence = match (path, line) {
                (Some(path), Some(line)) => Some((to_path(path)?, to_u32(line, "line")?)),
                (None, None) => None,
                _ => return Err(DbError::Invalid(format!("stored app edge {src} -> {dst} has half an evidence location"))),
            };
            Ok(AppEdge { from: to_u32(src, "app node id")?, to: to_u32(dst, "app node id")?, kind: AppEdgeKind::from_db(&kind)?, confidence, evidence })
        })
        .collect::<Result<Vec<_>, DbError>>()?;
    edges.sort_by_key(|edge| (edge.from, edge.to, edge.kind));
    Ok(AppGraph { nodes, edges })
}

/// Breadth-first from the known `focus` ids over edges in both directions, at most `depth` hops
/// and `max_nodes` nodes (nearest first); edges among the visited nodes.
pub(super) async fn app_neighborhood(
    conn: &mut PgConnection,
    org: OrgId,
    key: &RepoVersionKey,
    focus: &[u32],
    depth: u8,
    max_nodes: u32,
) -> Result<AppGraph, DbError> {
    let graph = graph(conn, org, key).await?;
    let known: BTreeSet<u32> = graph.nodes.iter().map(|node| node.id).collect();
    let mut neighbors: BTreeMap<u32, BTreeSet<u32>> = BTreeMap::new();
    for edge in &graph.edges {
        neighbors.entry(edge.from).or_default().insert(edge.to);
        neighbors.entry(edge.to).or_default().insert(edge.from);
    }
    let max_nodes = usize::try_from(max_nodes).unwrap_or(usize::MAX);
    let mut visited: BTreeSet<u32> = BTreeSet::new();
    let mut queue: VecDeque<(u32, u8)> = VecDeque::new();
    for id in focus.iter().filter(|id| known.contains(id)) {
        if visited.len() < max_nodes && visited.insert(*id) {
            queue.push_back((*id, 0));
        }
    }
    while let Some((id, distance)) = queue.pop_front() {
        if distance >= depth {
            continue;
        }
        for next in neighbors.get(&id).into_iter().flatten() {
            if visited.len() >= max_nodes {
                break;
            }
            if visited.insert(*next) {
                queue.push_back((*next, distance + 1));
            }
        }
    }
    Ok(AppGraph {
        nodes: graph.nodes.into_iter().filter(|node| visited.contains(&node.id)).collect(),
        edges: graph.edges.into_iter().filter(|edge| visited.contains(&edge.from) && visited.contains(&edge.to)).collect(),
    })
}

/// Nodes bound to any of `symbols`, by id.
pub(super) async fn app_nodes_for_symbols(
    conn: &mut PgConnection,
    org: OrgId,
    key: &RepoVersionKey,
    symbols: &[SymbolRef],
) -> Result<Vec<AppNode>, DbError> {
    // An ord past the stored range names no node.
    let symbols: Vec<(&str, [u8; 20], i32)> =
        symbols.iter().filter_map(|s| i32::try_from(s.ord).ok().map(|ord| (s.path.as_str(), s.blob.0, ord))).collect();
    let paths: Vec<&str> = symbols.iter().map(|(path, _, _)| *path).collect();
    let blobs: Vec<[u8; 20]> = symbols.iter().map(|(_, blob, _)| *blob).collect();
    let ords: Vec<i32> = symbols.iter().map(|(_, _, ord)| *ord).collect();
    let rows: Vec<NodeRow> = sqlx::query_as(&format!(
        "SELECT {NODE_COLUMNS} FROM app_nodes n \
         WHERE n.org_id = $1 AND n.repo_id = $2 AND n.sha = $3 \
           AND (n.symbol_path, n.symbol_blob, n.symbol_ord) IN (SELECT * FROM UNNEST($4::text[], $5::bytea[], $6::int4[])) \
         ORDER BY n.id"
    ))
    .bind(org.as_uuid())
    .bind(key.repo_id.as_uuid())
    .bind(key.sha.as_bytes().as_slice())
    .bind(&paths)
    .bind(&blobs)
    .bind(&ords)
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter().map(NodeRow::into_node).collect()
}
