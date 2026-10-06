//! Applies every migration to a fresh database and checks the schema invariants storage.md relies
//! on: the table set, tenancy (org_id-leading keys, composite FKs, RLS), job dedupe, webhook inbox
//! uniqueness, the org feed NOTIFY trigger, and that a second run is a no-op.
//!
//! Needs `DATABASE_URL` pointing at a PostgreSQL >= 16 server whose role may create databases and
//! roles; the test creates `mage_migrations_<uuid>` and drops it afterwards. Without
//! `DATABASE_URL` the test prints why it skipped and passes.

use std::collections::BTreeSet;
use std::str::FromStr;
use std::time::Duration;

use mage_infra::pg::schema::{MIGRATOR, migrate};
use sqlx::postgres::{PgConnectOptions, PgListener, PgPoolOptions};
use sqlx::{Connection, PgConnection, PgPool};
use uuid::Uuid;

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

/// Every v1 table (storage.md "Schema" minus knowledge layer 5).
const TABLES: &[&str] = &[
    // core: orgs, users, settings, integrations, customers
    "orgs", "users", "user_identities", "memberships", "invites", "org_settings", "integrations",
    "org_intake_config", "customers", "customer_refs",
    // core: repos
    "repos", "repo_versions", "repo_version_reasons", "layer_status", "layer_batches", "recipes",
    "deployments",
    // runs
    "runners", "runs", "run_attempts", "run_events", "run_commands", "gates", "artifacts",
    "idempotency_replies",
    // tickets
    "tickets", "ticket_thread_snapshots", "causes", "cause_citations", "fix_briefs",
    "fix_pull_requests", "resolutions", "outbound_messages",
    // knowledge
    "blobs", "blob_stages", "symbols", "name_refs", "literal_sites", "error_sites",
    "catalog_entries", "app_facts", "manifests", "app_nodes", "app_edges", "commits",
    "commit_files", "pull_requests", "ticket_links", "history_cursors", "scip_indexes",
    "scip_symbols", "scip_occurrences",
    // ops
    "jobs", "webhook_inbox", "sessions", "oauth_states", "runner_bootstrap_tokens",
    "token_revocations", "secrets", "data_keys", "org_feed_heads", "org_feed", "audit_log",
    "triage_evals", "triage_eval_cases", "triage_labels",
];

/// storage.md "Conventions": platform tables have no RLS; every other table is a tenant table.
const PLATFORM_TABLES: &[&str] = &[
    "orgs", "users", "user_identities", "runners", "runner_bootstrap_tokens", "sessions",
    "oauth_states", "token_revocations", "jobs", "webhook_inbox", "idempotency_replies",
    "audit_log", "secrets",
];

/// Knowledge layer 5 (Semantic) is not part of v1.
const EXCLUDED_TABLES: &[&str] = &["chunks", "summaries", "glossary_terms"];

#[tokio::test]
async fn migrations_build_the_v1_schema_and_rerun_as_noop() -> TestResult {
    let Some(url) = std::env::var("DATABASE_URL").ok().filter(|u| !u.is_empty()) else {
        eprintln!(
            "skipping migrations test: DATABASE_URL is not set (point it at a PostgreSQL >= 16 \
             server; the test creates and drops its own database)"
        );
        return Ok(());
    };
    let admin_opts = PgConnectOptions::from_str(&url)?;
    let mut admin = PgConnection::connect_with(&admin_opts).await?;
    let db = format!("mage_migrations_{}", Uuid::new_v4().simple());
    sqlx::raw_sql(&format!(r#"CREATE DATABASE "{db}""#)).execute(&mut admin).await?;

    let outcome = exercise(admin_opts.clone().database(&db)).await;

    sqlx::raw_sql(&format!(r#"DROP DATABASE "{db}" WITH (FORCE)"#)).execute(&mut admin).await?;
    admin.close().await?;
    outcome
}

async fn exercise(opts: PgConnectOptions) -> TestResult {
    let pool = PgPoolOptions::new().max_connections(4).connect_with(opts).await?;
    let result = checks(&pool).await;
    pool.close().await;
    result
}

async fn checks(pool: &PgPool) -> TestResult {
    migrate(pool).await?;
    let expected_versions = MIGRATOR.iter().count();
    assert!(expected_versions > 0, "no migrations embedded");
    assert_eq!(applied_versions(pool).await?, expected_versions);

    table_set(pool).await?;
    tenancy_catalog(pool).await?;

    let org_a = new_org(pool, "a").await?;
    let org_b = new_org(pool, "b").await?;
    job_dedupe(pool, org_a, org_b).await?;
    webhook_inbox_uniques(pool).await?;
    cross_org_reference_rejected(pool, org_a, org_b).await?;
    row_level_security(pool, org_a, org_b).await?;
    org_feed_notifies(pool, org_a).await?;

    // second run: nothing pending, checksums match, nothing re-applied
    migrate(pool).await?;
    assert_eq!(applied_versions(pool).await?, expected_versions);
    Ok(())
}

async fn applied_versions(pool: &PgPool) -> Result<usize, sqlx::Error> {
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations WHERE success")
        .fetch_one(pool)
        .await?;
    Ok(usize::try_from(n).unwrap_or(usize::MAX))
}

async fn table_set(pool: &PgPool) -> TestResult {
    let actual: BTreeSet<String> = sqlx::query_scalar(
        "SELECT tablename::text FROM pg_tables WHERE schemaname = 'public' AND tablename <> '_sqlx_migrations'",
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .collect();
    let expected: BTreeSet<String> = TABLES.iter().map(|t| (*t).to_owned()).collect();
    assert_eq!(actual, expected, "public tables differ from the v1 table set");
    for t in EXCLUDED_TABLES {
        assert!(!actual.contains(*t), "layer 5 table {t} must not exist in v1");
    }
    Ok(())
}

/// RLS exactly on tenant tables; tenant PKs lead with org_id; FKs between tenant tables carry
/// org_id on both sides.
async fn tenancy_catalog(pool: &PgPool) -> TestResult {
    let rls: BTreeSet<String> = sqlx::query_scalar(
        "SELECT c.relname::text FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
         WHERE n.nspname = 'public' AND c.relkind = 'r' AND c.relrowsecurity",
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .collect();
    let tenant: BTreeSet<String> = TABLES
        .iter()
        .filter(|t| !PLATFORM_TABLES.contains(t))
        .map(|t| (*t).to_owned())
        .collect();
    assert_eq!(rls, tenant, "row-level security must be enabled on exactly the tenant tables");

    let policies: Vec<(String, String)> = sqlx::query_as(
        "SELECT tablename::text, policyname::text FROM pg_policies WHERE schemaname = 'public'",
    )
    .fetch_all(pool)
    .await?;
    for t in &tenant {
        for suffix in ["tenant", "platform"] {
            let name = format!("{t}_{suffix}");
            assert!(
                policies.iter().any(|(table, policy)| table == t && *policy == name),
                "missing policy {name}"
            );
        }
    }

    let pk_leads: Vec<(String, String)> = sqlx::query_as(
        "SELECT c.relname::text, a.attname::text
         FROM pg_index i
         JOIN pg_class c ON c.oid = i.indrelid
         JOIN pg_namespace n ON n.oid = c.relnamespace
         JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum = i.indkey[0]
         WHERE n.nspname = 'public' AND i.indisprimary",
    )
    .fetch_all(pool)
    .await?;
    for (table, lead) in &pk_leads {
        if tenant.contains(table) {
            assert_eq!(lead, "org_id", "primary key of tenant table {table} must start with org_id");
        }
    }
    // the only tenant table without a primary key keys on a NULLS NOT DISTINCT unique constraint
    let without_pk: Vec<&String> =
        tenant.iter().filter(|t| !pk_leads.iter().any(|(table, _)| table == *t)).collect();
    assert_eq!(without_pk, vec!["ticket_links"]);

    let fks: Vec<(String, String, String, String, String)> = sqlx::query_as(
        "SELECT con.conname::text, src.relname::text, dst.relname::text, sa.attname::text, da.attname::text
         FROM pg_constraint con
         JOIN pg_class src ON src.oid = con.conrelid
         JOIN pg_class dst ON dst.oid = con.confrelid
         JOIN pg_namespace n ON n.oid = src.relnamespace
         JOIN pg_attribute sa ON sa.attrelid = con.conrelid AND sa.attnum = con.conkey[1]
         JOIN pg_attribute da ON da.attrelid = con.confrelid AND da.attnum = con.confkey[1]
         WHERE n.nspname = 'public' AND con.contype = 'f'",
    )
    .fetch_all(pool)
    .await?;
    let mut tenant_to_tenant = 0;
    for (name, src, dst, src_col, dst_col) in &fks {
        if tenant.contains(src) && tenant.contains(dst) {
            tenant_to_tenant += 1;
            assert_eq!(
                (src_col.as_str(), dst_col.as_str()),
                ("org_id", "org_id"),
                "{name}: {src} -> {dst} must lead with org_id on both sides"
            );
        }
    }
    // 41 composite FKs between tenant tables in v1; fewer means some were not seen by the query
    assert!(tenant_to_tenant >= 41, "expected the composite tenant FKs, found {tenant_to_tenant}");
    Ok(())
}

async fn new_org(pool: &PgPool, slug: &str) -> Result<Uuid, sqlx::Error> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO orgs (id, slug, name, state, retention, created_at)
         VALUES ($1, $2, $2, 'active', '{}', now())",
    )
    .bind(id)
    .bind(slug)
    .execute(pool)
    .await?;
    Ok(id)
}

/// `enqueue_in`'s INSERT (storage.md "Job queue semantics"); `None` = deduplicated.
async fn enqueue(pool: &PgPool, org: Option<Uuid>, kind: &str, dedupe: Option<&str>) -> Result<Option<Uuid>, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO jobs (id, org_id, kind, lane, payload, dedupe_key, priority, state, run_after,
                           max_attempts, created_at, updated_at)
         VALUES ($1, $2, $3, 'server', '{}', $4, 0, 'queued', now(), 10, now(), now())
         ON CONFLICT DO NOTHING
         RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(org)
    .bind(kind)
    .bind(dedupe)
    .fetch_optional(pool)
    .await
}

async fn job_dedupe(pool: &PgPool, org_a: Uuid, org_b: Uuid) -> TestResult {
    let kind = "triage.ticket";
    let first = enqueue(pool, Some(org_a), kind, Some("k")).await?;
    let first = first.ok_or("first enqueue must insert")?;
    assert_eq!(enqueue(pool, Some(org_a), kind, Some("k")).await?, None, "queued duplicate");

    // the key is scoped per (org, kind)
    assert!(enqueue(pool, Some(org_b), kind, Some("k")).await?.is_some());
    assert!(enqueue(pool, Some(org_a), "notify.ticket", Some("k")).await?.is_some());

    // running is still active
    sqlx::query(
        "UPDATE jobs SET state = 'running', attempts = 1, lease_seq = 1, leased_until = now() + interval '1 minute'
         WHERE id = $1",
    )
    .bind(first)
    .execute(pool)
    .await?;
    assert_eq!(enqueue(pool, Some(org_a), kind, Some("k")).await?, None, "running duplicate");

    // a finished job leaves the active set: the same key enqueues again
    sqlx::query(
        "UPDATE jobs SET state = 'succeeded', leased_until = NULL, finished_at = now() WHERE id = $1",
    )
    .bind(first)
    .execute(pool)
    .await?;
    assert!(enqueue(pool, Some(org_a), kind, Some("k")).await?.is_some(), "key free after completion");

    // platform jobs (org NULL) dedupe too: NULLS NOT DISTINCT
    assert!(enqueue(pool, None, "ops.purge", Some("ops.purge:2026-10-05")).await?.is_some());
    assert_eq!(enqueue(pool, None, "ops.purge", Some("ops.purge:2026-10-05")).await?, None);

    // no dedupe key: never deduplicated
    assert!(enqueue(pool, Some(org_a), kind, None).await?.is_some());
    assert!(enqueue(pool, Some(org_a), kind, None).await?.is_some());

    // lease and finish columns move together
    let err = sqlx::query("UPDATE jobs SET state = 'running' WHERE id = $1")
        .bind(first)
        .execute(pool)
        .await
        .err()
        .ok_or("running without leased_until must violate a CHECK")?;
    assert_eq!(db_code(&err).as_deref(), Some("23514"));
    Ok(())
}

async fn accept_delivery(pool: &PgPool, source: &str, delivery: &str, body: &[u8; 32]) -> Result<bool, sqlx::Error> {
    let row: Option<i32> = sqlx::query_scalar(
        "INSERT INTO webhook_inbox (source, delivery_id, org_id, event, body, body_sha256, body_bytes, received_at)
         VALUES ($1, $2, NULL, 'issues', $3, $3, 32, now())
         ON CONFLICT DO NOTHING
         RETURNING 1",
    )
    .bind(source)
    .bind(delivery)
    .bind(body.as_slice())
    .fetch_optional(pool)
    .await?;
    Ok(row.is_some())
}

async fn webhook_inbox_uniques(pool: &PgPool) -> TestResult {
    let h1 = [1u8; 32];
    let h2 = [2u8; 32];
    assert!(accept_delivery(pool, "github", "d1", &h1).await?);
    assert!(!accept_delivery(pool, "github", "d1", &h2).await?, "same delivery id is a duplicate");
    assert!(!accept_delivery(pool, "github", "d2", &h1).await?, "same body under a new delivery id is a duplicate");
    assert!(accept_delivery(pool, "linear", "d1", &h1).await?, "keys are per source");
    assert!(accept_delivery(pool, "github", "d3", &h2).await?);

    let err = sqlx::query(
        "INSERT INTO webhook_inbox (source, delivery_id, event, body_sha256, body_bytes, received_at)
         VALUES ('github', 'd4', 'push', '\\x00', 0, now())",
    )
    .execute(pool)
    .await
    .err()
    .ok_or("a 1-byte body hash must violate the 32-byte CHECK")?;
    assert_eq!(db_code(&err).as_deref(), Some("23514"));
    Ok(())
}

async fn new_repo(pool: &PgPool, org: Uuid, external_id: i64) -> Result<Uuid, sqlx::Error> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO repos (org_id, id, external_id, owner, name, default_branch, installation_id, access, created_at)
         VALUES ($1, $2, $3, 'hoophq', 'hoop', 'main', 7, 'granted', now())",
    )
    .bind(org)
    .bind(id)
    .bind(external_id)
    .execute(pool)
    .await?;
    Ok(id)
}

async fn cross_org_reference_rejected(pool: &PgPool, org_a: Uuid, org_b: Uuid) -> TestResult {
    let repo_a = new_repo(pool, org_a, 1).await?;
    new_repo(pool, org_b, 1).await?; // same external id in another org is fine

    let insert_version = |org: Uuid| {
        sqlx::query("INSERT INTO repo_versions (org_id, repo_id, sha, created_at) VALUES ($1, $2, $3, now())")
            .bind(org)
            .bind(repo_a)
            .bind(vec![0xabu8; 20])
            .execute(pool)
    };
    let err = insert_version(org_b)
        .await
        .err()
        .ok_or("org B must not reference org A's repo")?;
    assert_eq!(db_code(&err).as_deref(), Some("23503"));
    insert_version(org_a).await?;

    let err = sqlx::query("INSERT INTO repo_versions (org_id, repo_id, sha, created_at) VALUES ($1, $2, $3, now())")
        .bind(org_a)
        .bind(repo_a)
        .bind(vec![0xcdu8; 32])
        .execute(pool)
        .await
        .err()
        .ok_or("a 32-byte sha must violate the 20-byte git id CHECK")?;
    assert_eq!(db_code(&err).as_deref(), Some("23514"));
    Ok(())
}

/// The tenant group sees and writes only `current_org()` rows; unset → nothing.
async fn row_level_security(pool: &PgPool, org_a: Uuid, org_b: Uuid) -> TestResult {
    let can_assume: bool = sqlx::query_scalar("SELECT pg_has_role(current_user, 'mage_tenant', 'MEMBER')")
        .fetch_one(pool)
        .await?;
    if !can_assume {
        eprintln!("skipping RLS behavior check: current role cannot SET ROLE mage_tenant");
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL ROLE mage_tenant").execute(&mut *tx).await?;

    let unscoped: i64 = sqlx::query_scalar("SELECT count(*) FROM repos").fetch_one(&mut *tx).await?;
    assert_eq!(unscoped, 0, "no mage.org_id → no rows");

    sqlx::query("SELECT set_config('mage.org_id', $1, true)")
        .bind(org_a.to_string())
        .execute(&mut *tx)
        .await?;
    let orgs_seen: Vec<Uuid> = sqlx::query_scalar("SELECT DISTINCT org_id FROM repos")
        .fetch_all(&mut *tx)
        .await?;
    assert_eq!(orgs_seen, vec![org_a]);

    // outbox: the tenant may insert jobs and read the dedupe columns, nothing else
    let job: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO jobs (id, org_id, kind, lane, payload, priority, state, run_after, max_attempts, created_at, updated_at)
         VALUES ($1, $2, 'triage.ticket', 'server', '{}', 0, 'queued', now(), 10, now(), now())
         ON CONFLICT DO NOTHING RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(org_a)
    .fetch_optional(&mut *tx)
    .await?;
    assert!(job.is_some());

    let err = sqlx::query(
        "INSERT INTO repos (org_id, id, external_id, owner, name, default_branch, installation_id, access, created_at)
         VALUES ($1, $2, 99, 'o', 'n', 'main', 7, 'granted', now())",
    )
    .bind(org_b)
    .bind(Uuid::now_v7())
    .execute(&mut *tx)
    .await
    .err()
    .ok_or("writing another org's row must fail the policy")?;
    assert_eq!(db_code(&err).as_deref(), Some("42501"));
    tx.rollback().await?;

    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL ROLE mage_tenant").execute(&mut *tx).await?;
    let err = sqlx::query("SELECT payload FROM jobs")
        .execute(&mut *tx)
        .await
        .err()
        .ok_or("the tenant group must not read job payloads")?;
    assert_eq!(db_code(&err).as_deref(), Some("42501"));
    tx.rollback().await?;
    Ok(())
}

async fn org_feed_notifies(pool: &PgPool, org: Uuid) -> TestResult {
    let mut listener = PgListener::connect_with(pool).await?;
    listener.listen("mage_org_feed").await?;

    let mut tx = pool.begin().await?;
    sqlx::query("INSERT INTO org_feed (org_id, id, kind, subject, at) VALUES ($1, 41, 'ticket', 'tkt_x', now())")
        .bind(org)
        .execute(&mut *tx)
        .await?;
    tx.rollback().await?;
    sqlx::query("INSERT INTO org_feed (org_id, id, kind, subject, at) VALUES ($1, 42, 'ticket', 'tkt_x', now())")
        .bind(org)
        .execute(pool)
        .await?;

    // the rolled-back insert notified nobody: the first notification is the committed one
    let note = tokio::time::timeout(Duration::from_secs(5), listener.recv()).await??;
    assert_eq!(note.channel(), "mage_org_feed");
    assert_eq!(note.payload(), format!("{org}:42"));
    Ok(())
}

fn db_code(err: &sqlx::Error) -> Option<String> {
    err.as_database_error().and_then(|e| e.code()).map(|c| c.into_owned())
}
