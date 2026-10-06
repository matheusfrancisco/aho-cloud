//! `mage start runner` composition smoke test: builds the role from a `[runner]` TOML section
//! (dev profile, no harness binaries) against a wiremock server, checks the bootstrap → assertion
//! credential lifecycle across two starts, and that a cancelled shutdown token stops the lease
//! loop cleanly.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::time::Duration;

use mage_core::wire::runner::{LeaseResponse, LeaseTerms, RegistrationAck, routes};
use serde_json::Value;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const BOOTSTRAP: &str = "mgb_smoke-bootstrap-token-0123456789";

fn config(dir: &Path, server: &MockServer) -> mage_runner::Config {
    let text = format!(
        r#"
        [runner]
        environment = "dev"
        isolation = "dev"
        data_dir = "{data}"
        server_url = "{url}"
        [runner.link]
        name = "smoke-runner"
        bootstrap_token = "file:{bootstrap}"
        [runner.harness.omp]
        binary = "{data}/no-such-omp"
        [runner.harness.claude]
        binary = "{data}/no-such-claude"
        [runner.daemon]
        drain_grace_secs = 5
        "#,
        data = dir.join("runner").display(),
        url = server.uri(),
        bootstrap = dir.join("bootstrap").display(),
    );
    let mut file: toml::Table = text.parse().unwrap();
    file.remove("runner").unwrap().try_into().unwrap()
}

async fn mount(server: &MockServer) {
    let ack = RegistrationAck {
        runner_id: uuid::Uuid::now_v7().into(),
        runner_token: "mgr_smoke-runner-token".into(),
        expires_at: OffsetDateTime::now_utc() + time::Duration::minutes(15),
        released: Vec::new(),
        lease: LeaseTerms { ttl: Duration::from_secs(60), heartbeat_every: Duration::from_secs(15) },
        long_poll_max: Duration::from_secs(25),
        min_version: "0.1.0".into(),
    };
    Mock::given(method("POST")).and(path(routes::REGISTER)).respond_with(ResponseTemplate::new(200).set_body_json(&ack)).mount(server).await;
    Mock::given(method("POST"))
        .and(path(routes::LEASE))
        .respond_with(ResponseTemplate::new(200).set_body_json(LeaseResponse { assignment: None }).set_delay(Duration::from_millis(100)))
        .mount(server)
        .await;
}

/// Starts the runner, waits for a register and two lease polls, shuts it down, and returns the
/// grant of the register request.
async fn start_and_stop(dir: &Path, server: &MockServer) -> Value {
    server.reset().await;
    mount(server).await;
    let shutdown = CancellationToken::new();
    let role = tokio::spawn(mage_runner::run(config(dir, server), shutdown.clone()));
    let polled = async {
        loop {
            let requests = server.received_requests().await.unwrap_or_default();
            if requests.iter().filter(|r| r.url.path() == routes::LEASE).count() >= 2 {
                return requests;
            }
            assert!(!role.is_finished(), "runner exited early");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    let requests = tokio::time::timeout(Duration::from_secs(30), polled).await.expect("runner never registered and leased");
    shutdown.cancel();
    let stopped = tokio::time::timeout(Duration::from_secs(30), role).await.expect("runner did not stop within 30 s").unwrap();
    assert!(stopped.is_ok(), "runner stopped with {stopped:?}");
    let register = requests.iter().find(|r| r.url.path() == routes::REGISTER).expect("no register request");
    let body: Value = serde_json::from_slice(&register.body).unwrap();
    body["grant"].clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn runner_registers_with_the_bootstrap_token_then_the_assertion_and_stops_on_shutdown() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("bootstrap"), format!("{BOOTSTRAP}\n")).unwrap();

    let first = start_and_stop(dir.path(), &server).await;
    assert_eq!(first["type"], "bootstrap");
    assert_eq!(first["bootstrap_token"], BOOTSTRAP);
    assert_eq!(first["name"], "smoke-runner");
    assert!(dir.path().join("runner/state.db").is_file());
    assert!(dir.path().join("runner/credentials.json").is_file());

    // Registered: the second start signs an assertion and deletes the spent bootstrap file.
    let second = start_and_stop(dir.path(), &server).await;
    assert_eq!(second["type"], "assertion");
    assert!(!dir.path().join("bootstrap").exists(), "spent bootstrap token file kept");
}
