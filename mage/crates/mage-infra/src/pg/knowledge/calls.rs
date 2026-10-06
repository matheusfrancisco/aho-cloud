//! `call_edges`: one hop of the call graph at a version, by callee name (Symbols layer) or from a
//! SCIP index (Refs layer).

use std::collections::{BTreeMap, HashMap, HashSet};

use mage_core::domain::ids::{BlobOid, OrgId, RepoVersionKey};
use mage_core::domain::path::RepoPath;
use mage_core::ports::knowledge::{CallDirection, CallEdge, Precision, ScipRange, ScipRoles, SymbolDef, SymbolRef};
use sqlx::PgConnection;
use uuid::Uuid;

use super::codec::{to_path, to_range, to_rev, to_u32};
use super::scip::ready_index_at;
use super::view::{innermost, ord, resolve, revs};
use crate::pg::error::DbError;

type EdgeMap = BTreeMap<(SymbolRef, SymbolRef), Vec<(RepoPath, u32)>>;

pub(super) async fn call_edges(
    conn: &mut PgConnection,
    org: OrgId,
    key: &RepoVersionKey,
    from: &[SymbolRef],
    dir: CallDirection,
    precision: &Precision,
    limit: u32,
) -> Result<Vec<CallEdge>, DbError> {
    let revs = revs(conn, org, key).await?;
    let index = match precision {
        Precision::Name => None,
        Precision::Precise => Some(ready_index_at(conn, org, key.repo_id, key.sha).await?.ok_or(DbError::NotFound)?),
        Precision::PreciseStale { from_sha } => Some(ready_index_at(conn, org, key.repo_id, *from_sha).await?.ok_or(DbError::NotFound)?),
    };
    let Some(revs) = revs else { return Ok(Vec::new()) };
    let parser = to_rev(revs.parser.0)?;
    let mut seen = HashSet::new();
    let unique: Vec<SymbolRef> = from.iter().filter(|r| seen.insert(*r)).cloned().collect();
    let resolved = resolve(conn, org, key, parser, &unique).await?;
    let targets: Vec<(&SymbolRef, SymbolDef)> = resolved.into_iter().filter_map(|(i, def)| unique.get(i).map(|r| (r, def))).collect();
    let mut edges = EdgeMap::new();
    match index {
        None => name_edges(conn, org, key, parser, &targets, dir, &mut edges).await?,
        Some(index) => scip_edges(conn, org, key, parser, index, &targets, dir, &mut edges).await?,
    }
    Ok(edges
        .into_iter()
        .take(usize::try_from(limit).unwrap_or(usize::MAX))
        .map(|((from, to), mut sites)| {
            sites.sort();
            CallEdge { from, to, sites }
        })
        .collect())
}

/// `Call` / `Dispatch` refs named like the target (callers, grouped by enclosing symbol), or
/// inside the source resolved to every def of the callee name at the version (callees).
async fn name_edges(
    conn: &mut PgConnection,
    org: OrgId,
    key: &RepoVersionKey,
    parser: i16,
    targets: &[(&SymbolRef, SymbolDef)],
    dir: CallDirection,
    edges: &mut EdgeMap,
) -> Result<(), DbError> {
    match dir {
        CallDirection::Callers => {
            let names: Vec<&str> = targets.iter().map(|(_, def)| def.name.as_str()).collect();
            let refs: Vec<(String, [u8; 20], String, i32, i32)> = sqlx::query_as(
                "SELECT m.path, m.blob_oid, r.name, r.enclosing, r.line \
                 FROM manifests m JOIN name_refs r ON r.org_id = m.org_id AND r.blob_oid = m.blob_oid AND r.parser_rev = $4 \
                 WHERE m.org_id = $1 AND m.repo_id = $2 AND m.sha = $3 AND r.kind IN ('call', 'dispatch') \
                   AND r.enclosing IS NOT NULL AND r.name = ANY($5)",
            )
            .bind(org.as_uuid())
            .bind(key.repo_id.as_uuid())
            .bind(key.sha.as_bytes().as_slice())
            .bind(parser)
            .bind(&names)
            .fetch_all(&mut *conn)
            .await?;
            let refs = refs
                .into_iter()
                .map(|(path, blob, name, enclosing, line)| {
                    Ok((to_path(path)?, BlobOid::from_bytes(blob), name, to_u32(enclosing, "ord")?, to_u32(line, "line")?))
                })
                .collect::<Result<Vec<_>, DbError>>()?;
            for (target, def) in targets {
                for (path, blob, name, enclosing, line) in &refs {
                    if *name == def.name {
                        let from = SymbolRef { path: path.clone(), blob: *blob, ord: *enclosing };
                        edges.entry((from, (*target).clone())).or_default().push((path.clone(), *line));
                    }
                }
            }
        }
        CallDirection::Callees => {
            let blobs: Vec<[u8; 20]> = targets.iter().map(|(source, _)| source.blob.0).collect();
            let ords = targets.iter().map(|(source, _)| ord(source.ord)).collect::<Result<Vec<_>, _>>()?;
            let calls: Vec<([u8; 20], i32, String, i32)> = sqlx::query_as(
                "SELECT r.blob_oid, r.enclosing, r.name, r.line FROM name_refs r \
                 WHERE r.org_id = $1 AND r.parser_rev = $2 AND r.kind IN ('call', 'dispatch') \
                   AND (r.blob_oid, r.enclosing) IN (SELECT * FROM UNNEST($3::bytea[], $4::int4[]))",
            )
            .bind(org.as_uuid())
            .bind(parser)
            .bind(&blobs)
            .bind(&ords)
            .fetch_all(&mut *conn)
            .await?;
            let names: Vec<&str> = calls.iter().map(|(_, _, name, _)| name.as_str()).collect::<HashSet<_>>().into_iter().collect();
            let callees: Vec<(String, [u8; 20], i32, String)> = sqlx::query_as(
                "SELECT m.path, m.blob_oid, s.ord, s.name \
                 FROM manifests m JOIN symbols s ON s.org_id = m.org_id AND s.blob_oid = m.blob_oid AND s.parser_rev = $4 \
                 WHERE m.org_id = $1 AND m.repo_id = $2 AND m.sha = $3 AND s.name = ANY($5)",
            )
            .bind(org.as_uuid())
            .bind(key.repo_id.as_uuid())
            .bind(key.sha.as_bytes().as_slice())
            .bind(parser)
            .bind(&names)
            .fetch_all(&mut *conn)
            .await?;
            let mut by_name: HashMap<&str, Vec<SymbolRef>> = HashMap::new();
            for (path, blob, ord, name) in &callees {
                by_name.entry(name.as_str()).or_default().push(SymbolRef {
                    path: to_path(path.clone())?,
                    blob: BlobOid::from_bytes(*blob),
                    ord: to_u32(*ord, "ord")?,
                });
            }
            for (source, _) in targets {
                for (blob, enclosing, name, line) in &calls {
                    if *blob != source.blob.0 || i64::from(*enclosing) != i64::from(source.ord) {
                        continue;
                    }
                    let line = to_u32(*line, "line")?;
                    for to in by_name.get(name.as_str()).into_iter().flatten() {
                        edges.entry(((*source).clone(), to.clone())).or_default().push((source.path.clone(), line));
                    }
                }
            }
        }
    }
    Ok(())
}

/// One occurrence of the target's document.
struct DocOccurrence {
    range: ScipRange,
    symbol: String,
    roles: ScipRoles,
}

impl DocOccurrence {
    fn line(&self) -> u32 {
        self.range.start_line
    }
}

/// SCIP's document-local symbols (`local <n>`) never name another symbol of the version.
fn is_local(symbol: &str) -> bool {
    symbol.starts_with("local ")
}

/// Symbol defined at `name_pos` (1-based line and column): the definition occurrence on that line
/// covering the column, else the first non-local definition on that line.
fn definition_at(doc: &[DocOccurrence], (line, col): (u32, u32)) -> Option<&str> {
    let defs = || doc.iter().filter(move |occ| occ.roles.is_definition() && occ.line() == line);
    defs()
        .find(|occ| occ.range.covers(line, Some(col.saturating_sub(1))))
        .or_else(|| defs().find(|occ| !is_local(&occ.symbol)))
        .map(|occ| occ.symbol.as_str())
}

/// Callers: references of the target's SCIP symbol in usable documents, attributed to their
/// innermost enclosing symbol. Callees: references inside the target's span, resolved to the
/// innermost symbol at their first usable definition.
#[allow(clippy::too_many_arguments)]
async fn scip_edges(
    conn: &mut PgConnection,
    org: OrgId,
    key: &RepoVersionKey,
    parser: i16,
    index: Uuid,
    targets: &[(&SymbolRef, SymbolDef)],
    dir: CallDirection,
    edges: &mut EdgeMap,
) -> Result<(), DbError> {
    // Each resolved target's path holds its blob, so its document is usable when its rows carry
    // that blob.
    let paths: Vec<&str> = targets.iter().map(|(target, _)| target.path.as_str()).collect();
    let rows: Vec<(String, [u8; 20], Vec<i32>, String, i32)> = sqlx::query_as(
        "SELECT path, blob_oid, range, symbol, roles FROM scip_occurrences \
         WHERE org_id = $1 AND scip_index_id = $2 AND path = ANY($3) ORDER BY path, ord",
    )
    .bind(org.as_uuid())
    .bind(index)
    .bind(&paths)
    .fetch_all(&mut *conn)
    .await?;
    let mut docs: HashMap<String, HashMap<[u8; 20], Vec<DocOccurrence>>> = HashMap::new();
    for (path, blob, range, symbol, roles) in rows {
        let range = to_range(&range)?;
        docs.entry(path).or_default().entry(blob).or_default().push(DocOccurrence {
            range,
            symbol,
            roles: ScipRoles(to_u32(roles, "scip roles")?),
        });
    }
    let doc_of = |target: &SymbolRef| docs.get(target.path.as_str()).and_then(|by_blob| by_blob.get(&target.blob.0));
    match dir {
        CallDirection::Callers => {
            let wanted: Vec<(&SymbolRef, &str)> = targets
                .iter()
                .filter_map(|(target, def)| doc_of(target).and_then(|doc| definition_at(doc, def.name_pos)).map(|symbol| (*target, symbol)))
                .collect();
            let symbols: Vec<&str> = wanted.iter().map(|(_, symbol)| *symbol).collect();
            let refs: Vec<(String, [u8; 20], i32, String, Option<i32>)> = sqlx::query_as(&format!(
                "SELECT o.path, o.blob_oid, o.range[1], o.symbol, enc.ord \
                 FROM scip_occurrences o \
                 JOIN manifests m ON m.org_id = o.org_id AND m.repo_id = $3 AND m.sha = $4 AND m.path = o.path AND m.blob_oid = o.blob_oid \
                 LEFT JOIN LATERAL ({}) enc ON true \
                 WHERE o.org_id = $1 AND o.scip_index_id = $2 AND o.symbol = ANY($5) AND o.roles & 1 = 0",
                innermost("o.blob_oid", "o.range[1]", "$6")
            ))
            .bind(org.as_uuid())
            .bind(index)
            .bind(key.repo_id.as_uuid())
            .bind(key.sha.as_bytes().as_slice())
            .bind(&symbols)
            .bind(parser)
            .fetch_all(&mut *conn)
            .await?;
            for (target, symbol) in &wanted {
                for (path, blob, line, occ_symbol, caller) in &refs {
                    let Some(caller) = caller else { continue };
                    if occ_symbol != symbol {
                        continue;
                    }
                    let path = to_path(path.clone())?;
                    let from = SymbolRef { path: path.clone(), blob: BlobOid::from_bytes(*blob), ord: to_u32(*caller, "ord")? };
                    edges.entry((from, (*target).clone())).or_default().push((path, to_u32(*line, "scip line")?));
                }
            }
        }
        CallDirection::Callees => {
            let calls: Vec<(&SymbolRef, &DocOccurrence)> = targets
                .iter()
                .flat_map(|(target, def)| {
                    doc_of(target)
                        .into_iter()
                        .flatten()
                        .filter(|occ| !occ.roles.is_definition() && !is_local(&occ.symbol) && def.span.contains(occ.line()))
                        .map(move |occ| (*target, occ))
                })
                .collect();
            let symbols: Vec<&str> = calls.iter().map(|(_, occ)| occ.symbol.as_str()).collect::<HashSet<_>>().into_iter().collect();
            let defs: Vec<(String, String, [u8; 20], Option<i32>)> = sqlx::query_as(&format!(
                "SELECT DISTINCT ON (o.symbol) o.symbol, o.path, o.blob_oid, enc.ord \
                 FROM scip_occurrences o \
                 JOIN manifests m ON m.org_id = o.org_id AND m.repo_id = $3 AND m.sha = $4 AND m.path = o.path AND m.blob_oid = o.blob_oid \
                 LEFT JOIN LATERAL ({}) enc ON true \
                 WHERE o.org_id = $1 AND o.scip_index_id = $2 AND o.symbol = ANY($5) AND o.roles & 1 = 1 \
                 ORDER BY o.symbol, o.path COLLATE \"C\", o.ord",
                innermost("o.blob_oid", "o.range[1]", "$6")
            ))
            .bind(org.as_uuid())
            .bind(index)
            .bind(key.repo_id.as_uuid())
            .bind(key.sha.as_bytes().as_slice())
            .bind(&symbols)
            .bind(parser)
            .fetch_all(&mut *conn)
            .await?;
            let mut callee_of: HashMap<String, SymbolRef> = HashMap::new();
            for (symbol, path, blob, callee) in defs {
                // The first usable definition decides; without an enclosing symbol there is no callee.
                let Some(callee) = callee else { continue };
                callee_of.insert(symbol, SymbolRef { path: to_path(path)?, blob: BlobOid::from_bytes(blob), ord: to_u32(callee, "ord")? });
            }
            for (target, occ) in calls {
                if let Some(to) = callee_of.get(&occ.symbol) {
                    edges.entry((target.clone(), to.clone())).or_default().push((target.path.clone(), occ.line()));
                }
            }
        }
    }
    Ok(())
}
