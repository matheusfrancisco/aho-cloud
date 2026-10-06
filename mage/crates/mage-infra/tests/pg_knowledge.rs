//! `PgKnowledgeStore` against Postgres 16: the shared `KnowledgeStore` contract with `PgRepoStore`
//! holding the versions and their revs. The contract's `tenants_never_see_each_other` runs through
//! the tenant role (RLS on), so tenancy needs no separate test here. `DATABASE_URL`-gated; see
//! `support` for the harness.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;

use mage_core::ports::knowledge::KnowledgeStore;
use mage_core::ports::repos::RepoStore;
use mage_core::testing::{KnowledgeFixture, SeqIdGen, contract_knowledge_store};
use mage_infra::pg::{PgKnowledgeStore, PgRepoStore};
use support::{TestDb, with_test_db};
use uuid::Uuid;

/// Orgs the contract suite uses (`org(n)` = 0x0a00 + n).
fn contract_orgs() -> Vec<Uuid> {
    (1..=2).map(|n| Uuid::from_u128(0x0a00 + n)).collect()
}

/// Knowledge tables plus what `PgRepoStore` writes; `repos` cascades to runs and artifacts.
const TABLES: &[&str] = &[
    "blobs",
    "blob_stages",
    "symbols",
    "name_refs",
    "literal_sites",
    "error_sites",
    "catalog_entries",
    "app_facts",
    "manifests",
    "app_nodes",
    "app_edges",
    "commits",
    "commit_files",
    "pull_requests",
    "ticket_links",
    "history_cursors",
    "scip_indexes",
    "scip_symbols",
    "scip_occurrences",
    "layer_batches",
    "layer_status",
    "repo_version_reasons",
    "repo_versions",
    "repos",
    "runs",
    "artifacts",
    "jobs",
];

/// `scip_indexes.artifact_id` references `artifacts`: the contract's SCIP artifacts (`art(n)` =
/// 0x0b00 + n, all of `org(1)`) as `scip_index` artifacts of one Learn run on a fixture repo.
async fn scip_artifacts(db: &TestDb) {
    let org = Uuid::from_u128(0x0a01);
    let (repo, run) = (Uuid::from_u128(0x0c01), Uuid::from_u128(0x0c02));
    sqlx::query(
        "INSERT INTO repos (org_id, id, external_id, owner, name, default_branch, installation_id, access, created_at) \
         VALUES ($1, $2, 1, 'fixture', 'scip-artifacts', 'main', 1, 'granted', now())",
    )
    .bind(org)
    .bind(repo)
    .execute(&db.owner)
    .await
    .expect("fixture repo");
    sqlx::query(
        "INSERT INTO runs (org_id, id, kind, repo_id, sha, status, attempt, version, spec, slot_class, priority, source_ready, \
                           budget, usage_reported, usage_metered, created_at) \
         VALUES ($1, $2, 'learn', $3, $4, 'succeeded', 1, 1, '{}', 'work', 0, true, '{}', '{}', '{}', now())",
    )
    .bind(org)
    .bind(run)
    .bind(repo)
    .bind([1u8; 20].as_slice())
    .execute(&db.owner)
    .await
    .expect("fixture learn run");
    for n in 1..=3u128 {
        sqlx::query(
            "INSERT INTO artifacts (org_id, id, run_id, attempt, kind, name, key, content_type, state, created_at) \
             VALUES ($1, $2, $3, 1, 'scip_index', $4, $5, 'application/octet-stream', 'pending', now())",
        )
        .bind(org)
        .bind(Uuid::from_u128(0x0b00 + n))
        .bind(run)
        .bind(format!("go-{n}.scip"))
        .bind(format!("org/{org}/runs/{run}/scip_index/go-{n}.scip"))
        .execute(&db.owner)
        .await
        .expect("fixture scip artifact");
    }
}

#[tokio::test]
async fn pg_knowledge_store_passes_the_contract() {
    with_test_db("pg_knowledge_store_passes_the_contract", |db| async move {
        db.orgs(&contract_orgs()).await;
        let db = &db;
        contract_knowledge_store(|clock| async move {
            db.truncate(TABLES).await;
            scip_artifacts(db).await;
            let ids = Arc::new(SeqIdGen::new());
            let repos = Arc::new(PgRepoStore::new(db.pools.clone(), clock.clone(), ids.clone()));
            let knowledge = Arc::new(PgKnowledgeStore::new(db.pools.clone(), clock, ids));
            KnowledgeFixture { knowledge: knowledge as Arc<dyn KnowledgeStore>, repos: repos as Arc<dyn RepoStore> }
        })
        .await;
    })
    .await;
}
