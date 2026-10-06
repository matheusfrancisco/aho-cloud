//! GC (knowledge.md "Reconcile and GC"): per-sha facts of a deleted version, and blob facts no
//! manifest of the org references any more.

use mage_core::domain::ids::{BlobOid, OrgId, RepoVersionKey};
use sqlx::PgConnection;
use time::OffsetDateTime;

use super::scip;
use crate::pg::error::DbError;

/// Manifest, app graph and SCIP indexes of `key`'s sha.
pub(super) async fn delete_version_facts(conn: &mut PgConnection, org: OrgId, key: &RepoVersionKey) -> Result<(), DbError> {
    for table in ["manifests", "app_edges", "app_nodes"] {
        sqlx::query(&format!("DELETE FROM {table} WHERE org_id = $1 AND repo_id = $2 AND sha = $3"))
            .bind(org.as_uuid())
            .bind(key.repo_id.as_uuid())
            .bind(key.sha.as_bytes().as_slice())
            .execute(&mut *conn)
            .await?;
    }
    let indexes = scip::indexes_of_sha(conn, org, key).await?;
    scip::drop_indexes(conn, org, &indexes).await
}

/// Up to `max` blob rows created before `cutoff` that no manifest of the org references (by oid),
/// deleted with every blob-keyed fact; returns the oids whose content object was stored.
pub(super) async fn sweep_unreferenced_blobs(conn: &mut PgConnection, org: OrgId, cutoff: OffsetDateTime, max: u32) -> Result<Vec<BlobOid>, DbError> {
    let doomed: Vec<([u8; 20], bool)> = sqlx::query_as(
        "SELECT b.blob_oid, b.content_stored FROM blobs b \
         WHERE b.org_id = $1 AND b.created_at < $2 \
           AND NOT EXISTS (SELECT 1 FROM manifests m WHERE m.org_id = $1 AND m.blob_oid = b.blob_oid) \
         ORDER BY b.blob_oid LIMIT $3 FOR UPDATE OF b SKIP LOCKED",
    )
    .bind(org.as_uuid())
    .bind(cutoff)
    .bind(i64::from(max))
    .fetch_all(&mut *conn)
    .await?;
    let oids: Vec<[u8; 20]> = doomed.iter().map(|(oid, _)| *oid).collect();
    if oids.is_empty() {
        return Ok(Vec::new());
    }
    for table in ["blob_stages", "symbols", "name_refs", "literal_sites", "error_sites", "catalog_entries", "app_facts", "blobs"] {
        sqlx::query(&format!("DELETE FROM {table} WHERE org_id = $1 AND blob_oid = ANY($2)"))
            .bind(org.as_uuid())
            .bind(&oids)
            .execute(&mut *conn)
            .await?;
    }
    Ok(doomed.into_iter().filter(|(_, stored)| *stored).map(|(oid, _)| BlobOid::from_bytes(oid)).collect())
}
