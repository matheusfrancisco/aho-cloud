//! `mage start indexer` composition smoke test: builds the role from a TOML config the way
//! `mage-app` does, against a freshly migrated Postgres database (opt-in: `DATABASE_URL`), and
//! checks that the internal API answers, the worker seeds its periodic jobs, and a cancelled
//! shutdown token stops everything cleanly.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use mage_infra::config::ConfigFile;
use sqlx::postgres::PgPoolOptions;
use tokio_util::sync::CancellationToken;
use url::Url;

const GITHUB_KEY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../mage-infra/tests/fixtures/github_app_key.pem");

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

#[tokio::test(flavor = "multi_thread")]
async fn indexer_starts_from_config_serves_health_and_stops_on_shutdown() {
    let Ok(admin_url) = std::env::var("DATABASE_URL") else {
        eprintln!("skipping indexer compose smoke test: DATABASE_URL is not set");
        return;
    };
    let db = format!("mage_idx_smoke_{}", uuid::Uuid::new_v4().simple());
    let admin = PgPoolOptions::new().max_connections(1).connect(&admin_url).await.expect("connect DATABASE_URL");
    sqlx::raw_sql(&format!("CREATE DATABASE \"{db}\"")).execute(&admin).await.unwrap();
    let mut db_url = Url::parse(&admin_url).unwrap();
    db_url.set_path(&format!("/{db}"));
    {
        let pool = PgPoolOptions::new().max_connections(1).connect(db_url.as_str()).await.unwrap();
        mage_infra::pg::schema::migrate(&pool).await.unwrap();
        pool.close().await;
    }

    let dir = tempfile::tempdir().unwrap();
    let url_file = dir.path().join("database_url");
    std::fs::write(&url_file, db_url.as_str()).unwrap();
    let port = free_port();
    let text = format!(
        r#"
        [indexer]
        data_dir = "{data}"
        internal_listen = "127.0.0.1:{port}"
        [indexer.database]
        url_source = "file:{url_file}"
        pool_max = 2
        [indexer.blob]
        backend = "local"
        root = "{blob}"
        [indexer.github]
        client_id = "Iv1.smoke"
        app_private_key = "file:{key}"
        [indexer.internal_auth]
        issuer = "http://127.0.0.1:9"
        [indexer.workers]
        poll_interval_ms = 50
        shutdown_grace_secs = 5
        "#,
        data = dir.path().join("indexer").display(),
        url_file = url_file.display(),
        blob = dir.path().join("blob").display(),
        key = GITHUB_KEY,
    );
    let file = ConfigFile::parse(&text, "smoke.toml").unwrap();
    file.check_no_inline_secrets().unwrap();
    let config: mage_indexer::Config = file.section("indexer").unwrap();

    let shutdown = CancellationToken::new();
    let role = tokio::spawn(mage_indexer::run(config, shutdown.clone()));

    let health = format!("http://127.0.0.1:{port}/internal/v1/health");
    let client = reqwest::Client::new();
    let mut healthy = false;
    for _ in 0..200 {
        assert!(!role.is_finished(), "indexer exited early: {:?}", role.await);
        if client.get(&health).send().await.is_ok_and(|r| r.status().is_success()) {
            healthy = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(healthy, "internal api never answered {health}");

    let probe = PgPoolOptions::new().max_connections(1).connect(db_url.as_str()).await.unwrap();
    let seeded = async {
        loop {
            let n: i64 = sqlx::query_scalar("SELECT count(DISTINCT kind) FROM jobs WHERE kind IN ('index.reconcile', 'index.gc')")
                .fetch_one(&probe)
                .await
                .unwrap();
            if n == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(20), seeded).await.expect("worker never seeded index.reconcile and index.gc");
    probe.close().await;

    shutdown.cancel();
    let stopped = tokio::time::timeout(Duration::from_secs(30), role).await.expect("indexer did not stop within 30 s").unwrap();
    assert!(stopped.is_ok(), "indexer stopped with {stopped:?}");
    assert!(client.get(&health).send().await.is_err(), "internal api still answers after shutdown");

    sqlx::raw_sql(&format!("DROP DATABASE IF EXISTS \"{db}\" WITH (FORCE)")).execute(&admin).await.unwrap();
}
