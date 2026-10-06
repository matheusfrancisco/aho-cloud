//! The composed server end to end against real Postgres (opt-in: `DATABASE_URL` pointing at a
//! server whose user may create databases): `mage migrate`, `mage admin secret put`, then the
//! full `[server]` composition from TOML (local blob, local vault key, webhook intake, GitHub /
//! Linear / Slack / indexer endpoints pointed at a closed port) bound to port 0; probes, the key
//! set, an unauthenticated API call, and graceful shutdown.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write;
use std::panic::AssertUnwindSafe;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::str::FromStr;
use std::time::{Duration, Instant};

use futures::FutureExt;
use mage_core::wire::token::JwkSet;
use mage_server::compose::{self, Assets, Server};
use mage_server::config::Config;
use rand::RngCore;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tokio_util::sync::CancellationToken;
use url::Url;

/// A database created for this test and dropped afterwards (also after a panic).
struct TestDatabase {
    admin: sqlx::PgPool,
    name: String,
    url: String,
}

impl TestDatabase {
    async fn create(base_url: &str) -> TestDatabase {
        let options = PgConnectOptions::from_str(base_url).expect("DATABASE_URL");
        let admin = PgPoolOptions::new().max_connections(1).connect_with(options).await.expect("connect DATABASE_URL");
        let name = format!("mage_compose_{}", uuid::Uuid::new_v4().simple());
        sqlx::raw_sql(&format!("CREATE DATABASE \"{name}\"")).execute(&admin).await.expect("create database");
        let mut url = Url::parse(base_url).expect("DATABASE_URL is a URL");
        url.set_path(&format!("/{name}"));
        TestDatabase { admin, name, url: url.to_string() }
    }

    async fn drop(self) {
        if let Err(e) = sqlx::raw_sql(&format!("DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)", self.name)).execute(&self.admin).await {
            eprintln!("could not drop {}: {e}", self.name);
        }
        self.admin.close().await;
    }
}

fn write_private(path: &Path, bytes: &[u8]) {
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path).unwrap();
    file.write_all(bytes).unwrap();
}

fn config(dir: &Path) -> Config {
    let github_key = concat!(env!("CARGO_MANIFEST_DIR"), "/../mage-infra/tests/fixtures/github_app_key.pem");
    let dir = dir.display();
    // Nothing listens on port 9 (discard): provider calls fail fast instead of leaving the host.
    let closed = "http://127.0.0.1:9";
    let text = format!(
        r#"
        environment = "development"
        public_url = "http://localhost:8080"
        bind = "127.0.0.1:0"
        [database]
        url_source = "file:{dir}/database-url"
        pool_max = 4
        [blob]
        backend = "local"
        root = "{dir}/blob"
        [vault]
        backend = "local-key-file"
        local_key_file = "{dir}/dev.key"
        [github]
        client_id = "Iv1.compose"
        app_private_key = "file:{github_key}"
        [github.api]
        api_base = "{closed}/"
        web_base = "{closed}/"
        [login.github]
        client_id = "Iv1.compose"
        api_base = "{closed}"
        [linear]
        api_url = "{closed}/graphql"
        oauth_token_url = "{closed}/oauth/token"
        [slack]
        api_base = "{closed}/api/"
        [intake]
        mode = "webhook"
        [runs]
        credential_mode = "local_login"
        [gateway]
        mode = "harness-login"
        anthropic_base_url = "{closed}"
        [knowledge]
        indexer_url = "{closed}"
        [jobs]
        slots = 2
        "#
    );
    let file = mage_infra::config::ConfigFile::parse(&text, "compose test").unwrap();
    file.check_no_inline_secrets().unwrap();
    toml::from_str(&text).unwrap()
}

async fn get(client: &reqwest::Client, addr: std::net::SocketAddr, path: &str) -> reqwest::Response {
    client.get(format!("http://{addr}{path}")).send().await.unwrap_or_else(|e| panic!("GET {path}: {e}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn composed_server_serves_probes_keys_and_api_then_shuts_down() {
    let Ok(base_url) = std::env::var("DATABASE_URL") else {
        eprintln!("skipping composed server test: DATABASE_URL is not set");
        return;
    };
    let db = TestDatabase::create(&base_url).await;
    let dir = tempfile::tempdir().unwrap();
    write_private(&dir.path().join("database-url"), db.url.as_bytes());
    let mut key = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut key);
    write_private(&dir.path().join("dev.key"), &key);
    let config = config(dir.path());

    let body = tokio::time::timeout(Duration::from_secs(120), async {
        compose::migrate(&config).await.unwrap();
        for name in ["github/oauth_client_secret", "github/webhook_secret", "linear/webhook_secret", "slack/signing_secret"] {
            let meta = compose::admin_secret_put(&config, "platform", name, format!("{name}-value").into_bytes(), "compose-test").await.unwrap();
            assert_eq!(meta.name.as_str(), name);
        }

        let shutdown = CancellationToken::new();
        let assets = Assets::Embedded(vec![("index.html".to_owned(), b"<!doctype html><title>mage</title>".as_slice())]);
        let server = Server::build(config.clone(), assets, shutdown.clone()).await.unwrap();
        let addr = server.local_addr().unwrap();
        let serving = tokio::spawn(server.serve());
        let client = reqwest::Client::builder().timeout(Duration::from_secs(10)).build().unwrap();

        assert_eq!(get(&client, addr, "/health").await.status(), 200);

        // Ready once the org-feed listener is connected (it starts with `serve`).
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let status = get(&client, addr, "/ready").await.status();
            if status == 200 {
                break;
            }
            assert!(Instant::now() < deadline, "/ready stayed {status}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        let keys = get(&client, addr, "/.well-known/mage/keys.json").await;
        assert_eq!(keys.status(), 200);
        let keys: JwkSet = keys.json().await.unwrap();
        assert!(!keys.keys.is_empty(), "the signing key created at start is published");

        assert_eq!(get(&client, addr, "/api/v1/session").await.status(), 401);

        shutdown.cancel();
        let stopped = tokio::time::timeout(Duration::from_secs(20), serving).await.expect("server drains within 20 s");
        stopped.expect("serve task").expect("graceful shutdown");
        assert!(client.get(format!("http://{addr}/health")).send().await.is_err(), "listener closed after shutdown");
    });
    let outcome = AssertUnwindSafe(body).catch_unwind().await;
    db.drop().await;
    match outcome {
        Ok(finished) => finished.expect("test finished within 120 s"),
        Err(panic) => std::panic::resume_unwind(panic),
    }
}
