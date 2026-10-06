//! `HttpServerLink` against a wiremock server: request shapes and headers per endpoint, the
//! credential lifecycle (bootstrap → assertion), token refresh (2/3 TTL and 401), retry rules,
//! error mapping, signed-URL upload and download.

use std::sync::Arc;
use std::time::Duration;

use mage_core::domain::{ArtifactKind, FailStage, RunCommand, RunId, RunStatus, RunnerId, UserId};
use mage_core::ports::{Clock, UuidV7Gen};
use mage_core::wire::runner::{
    Arch, ArtifactSpec, Capabilities, CommandBatch, CommandEnvelope, Download, DownloadRef, ExecutorKind, FinishOutcome, FinishReport,
    GateTicket, LeaseRequest, LeaseTerms, Os, OutboundEvent, OutboundEventKind, RegistrationAck, RejectionItem, RunnerPhase, SlotCounts,
    UploadGrant, WireError, WireErrorCode,
};
use mage_core::wire::token::TokenCodec;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use time::macros::datetime;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use super::*;

const NOW: OffsetDateTime = datetime!(2026-03-01 12:00 UTC);

struct FixedClock;

impl Clock for FixedClock {
    fn now(&self) -> OffsetDateTime {
        NOW
    }
}

fn capabilities() -> Capabilities {
    Capabilities {
        arch: Arch::Aarch64,
        os: Os::MacOs,
        executor: ExecutorKind::Host,
        harnesses: Vec::new(),
        browser: true,
        slots: SlotCounts { light: 2, work: 1, verify: 1 },
    }
}

fn runner_id() -> RunnerId {
    uuid::Uuid::from_u128(0x0190_0000_0000_7000_8000_0000_0000_00aa).into()
}

fn run() -> RunId {
    uuid::Uuid::from_u128(0x0190_0000_0000_7000_8000_0000_0000_0bbb).into()
}

fn registration_ack(token: &str, ttl: time::Duration) -> Value {
    serde_json::to_value(RegistrationAck {
        runner_id: runner_id(),
        runner_token: token.into(),
        expires_at: NOW + ttl,
        released: Vec::new(),
        lease: LeaseTerms { ttl: Duration::from_secs(60), heartbeat_every: Duration::from_secs(15) },
        long_poll_max: Duration::from_secs(25),
        min_version: "0.1.0".into(),
    })
    .unwrap()
}

fn wire_error(code: WireErrorCode, status: Option<RunStatus>) -> Value {
    let mut e = WireError::new(code, format!("{code:?}"));
    e.status = status;
    serde_json::to_value(e).unwrap()
}

fn fast() -> RetryPolicy {
    RetryPolicy { base: Duration::from_millis(1), cap: Duration::from_millis(5) }
}

fn link(server: &MockServer, dir: &tempfile::TempDir, bootstrap: Option<&str>) -> HttpServerLink {
    let config = LinkConfig {
        server_url: format!("{}/", server.uri()),
        name: "runner-1".into(),
        bootstrap_token: bootstrap.map(str::to_owned),
        credential_file: dir.path().join("credentials.json"),
    };
    HttpServerLink::new(config, "0.1.0".into(), Arc::new(FixedClock), Arc::new(UuidV7Gen)).unwrap().with_retry(fast())
}

async fn mount_register(server: &MockServer, token: &str, ttl: time::Duration) {
    Mock::given(method("POST"))
        .and(path(routes::REGISTER))
        .respond_with(ResponseTemplate::new(200).set_body_json(registration_ack(token, ttl)))
        .mount(server)
        .await;
}

/// A link registered with token `tok-1` (15 min TTL).
async fn registered(server: &MockServer, dir: &tempfile::TempDir) -> HttpServerLink {
    mount_register(server, "tok-1", time::Duration::minutes(15)).await;
    let link = link(server, dir, Some("mgb_bootstrap-token-0123456789"));
    link.register(&capabilities(), &[]).await.unwrap();
    link
}

async fn requests_to(server: &MockServer, route: &str) -> Vec<Request> {
    server.received_requests().await.unwrap().into_iter().filter(|r| r.url.path() == route).collect()
}

async fn first_body(server: &MockServer, route: &str) -> Value {
    requests_to(server, route).await[0].body_json().unwrap()
}

fn header<'a>(r: &'a Request, name: &str) -> Option<&'a str> {
    r.headers.get(name).and_then(|v| v.to_str().ok())
}

#[tokio::test]
async fn first_register_uses_the_bootstrap_grant_and_records_the_runner_id() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    mount_register(&server, "tok-1", time::Duration::minutes(15)).await;
    let link = link(&server, &dir, Some("mgb_bootstrap-token-0123456789"));
    let held = [mage_core::wire::runner::HeldRun { run_id: run(), attempt: 2 }];
    let ack = link.register(&capabilities(), &held).await.unwrap();
    assert_eq!(ack.runner_id, runner_id());

    let reqs = requests_to(&server, routes::REGISTER).await;
    assert_eq!(reqs.len(), 1);
    let r = &reqs[0];
    assert_eq!(header(r, "mage-protocol"), Some("1"));
    assert_eq!(header(r, "authorization"), None, "register carries no runner token");
    let body: Value = r.body_json().unwrap();
    assert_eq!(body["grant"]["type"], "bootstrap");
    assert_eq!(body["grant"]["bootstrap_token"], "mgb_bootstrap-token-0123456789");
    assert_eq!(body["grant"]["public_key"], hex::encode(link.public_key()));
    assert_eq!(body["held"][0]["attempt"], 2);
    assert_eq!(body["instance_id"], link.instance_id().to_string());
    // The body has the shape of the wire DTO.
    let typed: mage_core::wire::runner::Registration = r.body_json().unwrap();
    assert_eq!(typed.capabilities, capabilities());

    let stored = Credentials::load_or_create(&dir.path().join("credentials.json")).unwrap();
    assert_eq!(stored.runner_id(), Some(runner_id()));
}

#[tokio::test]
async fn restart_registers_with_a_verifiable_assertion_and_a_new_instance() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let first = registered(&server, &dir).await;
    let second = link(&server, &dir, None);
    second.register(&capabilities(), &[]).await.unwrap();

    let reqs = requests_to(&server, routes::REGISTER).await;
    let body: Value = reqs[1].body_json().unwrap();
    assert_eq!(body["grant"]["type"], "assertion");
    assert_eq!(body["grant"]["runner_id"], runner_id().to_string());
    assert_ne!(body["instance_id"], first.instance_id().to_string());
    let codec = TokenCodec::new(server.uri());
    let assertion = codec.verify_assertion(body["grant"]["assertion"].as_str().unwrap(), &second.public_key(), NOW).unwrap();
    assert_eq!(assertion.runner, runner_id());
    assert_eq!(second.public_key(), first.public_key(), "the key pair survives restarts");
}

#[tokio::test]
async fn register_without_runner_id_or_bootstrap_token_is_invalid() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let err = link(&server, &dir, None).register(&capabilities(), &[]).await.unwrap_err();
    assert!(matches!(err, LinkError::Invalid(_)), "{err:?}");
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn per_run_endpoints_send_the_documented_requests() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let link = registered(&server, &dir).await;
    let r = run();
    let run_path = |route: &str| route.replace("{run}", &r.to_string());
    let artifact: ArtifactId = uuid::Uuid::from_u128(0x0190_0000_0000_7000_8000_0000_0000_0ccc).into();

    Mock::given(method("POST"))
        .and(path(run_path(routes::HEARTBEAT)))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "running", "lease_ttl_ms": 60000, "heartbeat_every_ms": 15000, "commands_pending": true})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(run_path(routes::EVENTS)))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"acked_local_seq": 2, "last_seq": 7, "status": "running"})))
        .mount(&server)
        .await;
    let command = CommandEnvelope { seq: 3, attempt: 1, issued_at: NOW, command: RunCommand::Steer { text: "look at x".into(), by: UserId::from(uuid::Uuid::now_v7()) } };
    Mock::given(method("GET"))
        .and(path(run_path(routes::COMMANDS)))
        .and(query_param("attempt", "1"))
        .and(query_param("after", "2"))
        .and(query_param("wait_ms", "25000"))
        .respond_with(ResponseTemplate::new(200).set_body_json(CommandBatch { commands: vec![command.clone()], status: RunStatus::Running }))
        .mount(&server)
        .await;
    let grant = UploadGrant {
        artifact_id: artifact,
        url: format!("{}/blob/up", server.uri()),
        method: HttpMethod::Put,
        headers: vec![("content-type".into(), "text/plain".into())],
        expires_at: NOW,
        already_complete: false,
    };
    Mock::given(method("POST")).and(path(run_path(routes::ARTIFACTS))).respond_with(ResponseTemplate::new(200).set_body_json(&grant)).mount(&server).await;
    Mock::given(method("POST"))
        .and(path(routes::ARTIFACT_COMPLETE.replace("{run}", &r.to_string()).replace("{artifact}", &artifact.to_string())))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"artifact_id": artifact, "state": "complete"})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(run_path(routes::RESULT)))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "awaiting_gate", "gate": null, "message": "End your turn."})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(run_path(routes::FINISH)))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "succeeded"})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(routes::LEASE))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"assignment": null})))
        .mount(&server)
        .await;

    let hb = link.heartbeat(r, 1, RunnerPhase::Preparing).await.unwrap();
    assert_eq!((hb.status, hb.commands_pending), (RunStatus::Running, true));
    let events = vec![OutboundEvent { local_seq: 2, at: NOW, kind: OutboundEventKind::Notice("hi".into()) }];
    assert_eq!(link.push_events(r, 1, &events).await.unwrap().acked_local_seq, 2);
    let batch = link.poll_commands(r, 1, 2, Duration::from_secs(25)).await.unwrap();
    assert_eq!(batch.commands, vec![command]);
    let spec = ArtifactSpec { kind: ArtifactKind::Log, name: "env-logs.tar.zst".into(), bytes: 3, sha256: [7; 32], content_type: "application/zstd".into() };
    assert_eq!(link.request_upload(r, 1, &spec).await.unwrap(), grant);
    link.complete_upload(r, 1, artifact).await.unwrap();
    let result = WireRunResult::Diagnose(mage_core::domain::DiagnoseResult {
        verdict: mage_core::domain::Verdict::Inconclusive { reason: "no boot".into() },
        script: None,
        signature: None,
        summary: "s".into(),
        suggested_fix: None,
        fix_sites: Vec::new(),
    });
    let submission = uuid::Uuid::now_v7();
    assert_eq!(link.submit_result(r, 1, submission, &result).await.unwrap().status, RunStatus::AwaitingGate);
    let report = FinishReport { outcome: FinishOutcome::Failed { stage: FailStage::Harness, reason: "crash".into(), retryable: true }, message: Some("m".into()) };
    assert_eq!(link.finish(r, 1, &report).await.unwrap(), RunStatus::Succeeded);
    let lease = LeaseRequest { request_id: uuid::Uuid::now_v7(), wait: Duration::from_secs(25), free: SlotCounts { light: 1, work: 0, verify: 1 }, prepared: Vec::new() };
    assert_eq!(link.lease(&lease).await.unwrap(), None);

    for req in server.received_requests().await.unwrap().iter().filter(|r| r.url.path() != routes::REGISTER) {
        assert_eq!(header(req, "authorization"), Some("Bearer tok-1"), "{}", req.url);
        assert_eq!(header(req, "mage-protocol"), Some("1"), "{}", req.url);
    }

    assert_eq!(first_body(&server, &run_path(routes::HEARTBEAT)).await, json!({"attempt": 1, "phase": "preparing"}));
    let events_body: mage_core::wire::runner::EventsRequest = serde_json::from_value(first_body(&server, &run_path(routes::EVENTS)).await).unwrap();
    assert_eq!(events_body.events, events);
    let upload_body: mage_core::wire::runner::UploadRequest = serde_json::from_value(first_body(&server, &run_path(routes::ARTIFACTS)).await).unwrap();
    assert_eq!((upload_body.attempt, upload_body.spec), (1, spec));
    let submit_body: mage_core::wire::runner::SubmitResultRequest = serde_json::from_value(first_body(&server, &run_path(routes::RESULT)).await).unwrap();
    assert_eq!((submit_body.attempt, submit_body.submission_id, submit_body.result), (1, submission, result));
    let finish_body: mage_core::wire::runner::FinishRequest = serde_json::from_value(first_body(&server, &run_path(routes::FINISH)).await).unwrap();
    assert_eq!((finish_body.attempt, finish_body.report), (1, report));
    let lease_body: LeaseRequest = serde_json::from_value(first_body(&server, routes::LEASE).await).unwrap();
    assert_eq!(lease_body, lease);
}

#[tokio::test]
async fn transient_failures_are_retried_with_the_same_body() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let link = registered(&server, &dir).await;
    let route = routes::RESULT.replace("{run}", &run().to_string());
    Mock::given(method("POST")).and(path(&route)).respond_with(ResponseTemplate::new(503)).up_to_n_times(1).mount(&server).await;
    Mock::given(method("POST"))
        .and(path(&route))
        .respond_with(ResponseTemplate::new(429).set_body_json({
            let mut e = WireError::new(WireErrorCode::RateLimited, "slow down");
            e.retry_after_ms = Some(20);
            e
        }))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST")).and(path(&route)).respond_with(ResponseTemplate::new(500)).up_to_n_times(1).mount(&server).await;
    Mock::given(method("POST"))
        .and(path(&route))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "succeeded", "gate": null, "message": "ok"})))
        .mount(&server)
        .await;
    let result = WireRunResult::Learn(mage_core::domain::RecipeProposal { toml: "version = 1".into(), notes: String::new() });
    let ack = link.submit_result(run(), 1, uuid::Uuid::now_v7(), &result).await.unwrap();
    assert_eq!(ack.status, RunStatus::Succeeded);
    let reqs = requests_to(&server, &route).await;
    assert_eq!(reqs.len(), 4, "503, 429, 500, then 200");
    assert!(reqs.windows(2).all(|w| w[0].body == w[1].body), "every retry repeats the same submission");
}

#[tokio::test]
async fn non_transient_answers_map_to_link_errors_without_retry() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let link = registered(&server, &dir).await;
    let cases: Vec<(u16, Value, LinkError)> = vec![
        (409, wire_error(WireErrorCode::LeaseLost, None), LinkError::Conflict(LinkConflict::LeaseLost)),
        (409, wire_error(WireErrorCode::RunTerminal, Some(RunStatus::Cancelled)), LinkError::Conflict(LinkConflict::RunTerminal(RunStatus::Cancelled))),
        (409, wire_error(WireErrorCode::InvalidState, Some(RunStatus::AwaitingGate)), LinkError::Conflict(LinkConflict::InvalidState(RunStatus::AwaitingGate))),
        (409, wire_error(WireErrorCode::IdempotencyMismatch, None), LinkError::Conflict(LinkConflict::IdempotencyMismatch)),
        (404, wire_error(WireErrorCode::NotFound, None), LinkError::NotFound),
        (403, wire_error(WireErrorCode::Forbidden, None), LinkError::Forbidden),
        (413, wire_error(WireErrorCode::TooLarge, None), LinkError::TooLarge("TooLarge".into())),
        (400, wire_error(WireErrorCode::UnsupportedProtocol, None), LinkError::Invalid("unsupported_protocol: UnsupportedProtocol".into())),
    ];
    for (i, (status, body, expected)) in cases.into_iter().enumerate() {
        let r: RunId = uuid::Uuid::from_u128(0x0190_0000_0000_7000_8000_0000_0001_0000 + i as u128).into();
        let route = routes::HEARTBEAT.replace("{run}", &r.to_string());
        Mock::given(method("POST")).and(path(&route)).respond_with(ResponseTemplate::new(status).set_body_json(body)).mount(&server).await;
        let err = link.heartbeat(r, 1, RunnerPhase::Running).await.unwrap_err();
        assert_eq!(err, expected, "HTTP {status}");
        assert_eq!(requests_to(&server, &route).await.len(), 1, "HTTP {status} is not retried");
    }
}

#[tokio::test]
async fn rejected_submissions_carry_the_items_and_gate() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let link = registered(&server, &dir).await;
    let gate = GateTicket { gate_id: uuid::Uuid::now_v7().into(), kind: mage_core::domain::GateKind::Evidence, attempt: 1 };
    let rejection = Rejection { gate: Some(gate), message: "fix the citations".into(), items: vec![RejectionItem { path: "causes[0]".into(), reason: "no quote".into() }] };
    let route = routes::RESULT.replace("{run}", &run().to_string());
    Mock::given(method("POST"))
        .and(path(&route))
        .respond_with(ResponseTemplate::new(422).set_body_json(WireError::rejected(rejection.clone())))
        .mount(&server)
        .await;
    let result = WireRunResult::Learn(mage_core::domain::RecipeProposal { toml: "x".into(), notes: String::new() });
    let err = link.submit_result(run(), 1, uuid::Uuid::now_v7(), &result).await.unwrap_err();
    assert_eq!(err, LinkError::Rejected(rejection));
}

#[tokio::test]
async fn a_401_re_registers_once_with_an_assertion_then_retries() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let link = registered(&server, &dir).await;
    server.reset().await;
    mount_register(&server, "tok-2", time::Duration::minutes(15)).await;
    let route = routes::HEARTBEAT.replace("{run}", &run().to_string());
    Mock::given(method("POST"))
        .and(path(&route))
        .and(wiremock::matchers::header("authorization", "Bearer tok-1"))
        .respond_with(ResponseTemplate::new(401).set_body_json(wire_error(WireErrorCode::Unauthorized, None)))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(&route))
        .and(wiremock::matchers::header("authorization", "Bearer tok-2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "running", "lease_ttl_ms": 60000, "heartbeat_every_ms": 15000, "commands_pending": false})))
        .mount(&server)
        .await;
    assert_eq!(link.heartbeat(run(), 1, RunnerPhase::Running).await.unwrap().status, RunStatus::Running);
    let registers = requests_to(&server, routes::REGISTER).await;
    assert_eq!(registers.len(), 1);
    let body: Value = registers[0].body_json().unwrap();
    assert_eq!(body["grant"]["type"], "assertion");
    assert_eq!(body["instance_id"], link.instance_id().to_string(), "a refresh keeps the instance id");
    assert_eq!(body["held"], json!([]));
}

#[tokio::test]
async fn a_second_401_after_re_registering_is_unauthorized() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let link = registered(&server, &dir).await;
    let route = routes::HEARTBEAT.replace("{run}", &run().to_string());
    Mock::given(method("POST"))
        .and(path(&route))
        .respond_with(ResponseTemplate::new(401).set_body_json(wire_error(WireErrorCode::Unauthorized, None)))
        .mount(&server)
        .await;
    assert_eq!(link.heartbeat(run(), 1, RunnerPhase::Running).await.unwrap_err(), LinkError::Unauthorized);
    assert_eq!(requests_to(&server, &route).await.len(), 2);
    assert_eq!(requests_to(&server, routes::REGISTER).await.len(), 2, "startup + exactly one re-registration");
}

#[tokio::test]
async fn the_token_is_refreshed_at_two_thirds_of_its_ttl() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    mount_register(&server, "tok-1", time::Duration::milliseconds(300)).await;
    let link = link(&server, &dir, Some("mgb_bootstrap-token-0123456789"));
    link.register(&capabilities(), &[]).await.unwrap();
    let route = routes::HEARTBEAT.replace("{run}", &run().to_string());
    Mock::given(method("POST"))
        .and(path(&route))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "running", "lease_ttl_ms": 60000, "heartbeat_every_ms": 15000, "commands_pending": false})))
        .mount(&server)
        .await;
    link.heartbeat(run(), 1, RunnerPhase::Running).await.unwrap();
    assert_eq!(requests_to(&server, routes::REGISTER).await.len(), 1, "fresh token: no refresh");
    tokio::time::sleep(Duration::from_millis(220)).await;
    link.heartbeat(run(), 1, RunnerPhase::Running).await.unwrap();
    let registers = requests_to(&server, routes::REGISTER).await;
    assert_eq!(registers.len(), 2, "past 2/3 of the TTL the next call re-registers first");
    assert_eq!(registers[1].body_json::<Value>().unwrap()["grant"]["type"], "assertion");
}

#[tokio::test]
async fn upload_puts_the_file_to_the_signed_url_without_the_runner_token() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let link = registered(&server, &dir).await;
    let file = dir.path().join("script.spec.ts");
    std::fs::write(&file, b"test('x', () => {})").unwrap();
    Mock::given(method("PUT")).and(path("/blob/up")).respond_with(ResponseTemplate::new(503)).up_to_n_times(1).mount(&server).await;
    Mock::given(method("PUT")).and(path("/blob/up")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
    let grant = UploadGrant {
        artifact_id: uuid::Uuid::now_v7().into(),
        url: format!("{}/blob/up", server.uri()),
        method: HttpMethod::Put,
        headers: vec![("content-type".into(), "text/plain".into()), ("x-goog-meta".into(), "signed".into())],
        expires_at: NOW,
        already_complete: false,
    };
    link.upload(&grant, &file).await.unwrap();
    let puts = requests_to(&server, "/blob/up").await;
    assert_eq!(puts.len(), 2, "a 5xx from the blob host is retried");
    let put = &puts[1];
    assert_eq!(put.body, b"test('x', () => {})");
    assert_eq!(header(put, "content-type"), Some("text/plain"));
    assert_eq!(header(put, "x-goog-meta"), Some("signed"));
    assert_eq!(header(put, "content-length"), Some("19"));
    assert_eq!(header(put, "authorization"), None);
    assert_eq!(header(put, "mage-protocol"), None);

    Mock::given(method("PUT")).and(path("/blob/expired")).respond_with(ResponseTemplate::new(403)).mount(&server).await;
    let expired = UploadGrant { url: format!("{}/blob/expired", server.uri()), ..grant };
    assert_eq!(link.upload(&expired, &file).await.unwrap_err(), LinkError::Forbidden);
}

fn signed(server: &MockServer, url_path: &str, bytes: &[u8]) -> Download {
    Download {
        what: DownloadRef::SourceBundle { repo_id: uuid::Uuid::now_v7().into(), sha: "a".repeat(40).parse().unwrap() },
        url: format!("{}{url_path}", server.uri()),
        bytes: bytes.len() as u64,
        sha256: Sha256::digest(bytes).into(),
        expires_at: NOW + time::Duration::minutes(15),
        name: None,
    }
}

#[tokio::test]
async fn download_verifies_and_refreshes_an_expired_url_once() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let link = registered(&server, &dir).await;
    let bytes = b"bundle-bytes".to_vec();
    let stale = signed(&server, "/blob/stale", &bytes);
    let fresh = Download { url: format!("{}/blob/fresh", server.uri()), ..stale.clone() };
    Mock::given(method("GET")).and(path("/blob/stale")).respond_with(ResponseTemplate::new(403)).mount(&server).await;
    Mock::given(method("GET")).and(path("/blob/fresh")).respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.clone())).mount(&server).await;
    Mock::given(method("POST"))
        .and(path(routes::DOWNLOADS.replace("{run}", &run().to_string())))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "downloads": [fresh] })))
        .mount(&server)
        .await;
    let dest = dir.path().join("out").join("bundle.tar.zst");
    link.download(run(), 1, &stale, &dest).await.unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), bytes);
    assert!(!dest.with_file_name("bundle.tar.zst.partial").exists());
    let refresh = requests_to(&server, &routes::DOWNLOADS.replace("{run}", &run().to_string())).await;
    assert_eq!(refresh.len(), 1);
    assert_eq!(refresh[0].body_json::<Value>().unwrap()["refs"][0], serde_json::to_value(stale.what).unwrap());
}

#[tokio::test]
async fn download_refuses_bytes_that_do_not_match_the_pin() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let link = registered(&server, &dir).await;
    let dl = signed(&server, "/blob/tampered", b"expected");
    Mock::given(method("GET")).and(path("/blob/tampered")).respond_with(ResponseTemplate::new(200).set_body_bytes(b"tampered".to_vec())).mount(&server).await;
    let dest = dir.path().join("bundle");
    let err = link.download(run(), 1, &dl, &dest).await.unwrap_err();
    assert!(matches!(err, LinkError::Invalid(_)), "{err:?}");
    assert!(!dest.exists());
    assert_eq!(requests_to(&server, "/blob/tampered").await.len() as u32, DOWNLOAD_INTEGRITY_TRIES);
}

#[tokio::test]
async fn an_expired_pause_budget_reports_unavailable() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let link = link(&server, &dir, None).with_retry(RetryPolicy { base: Duration::from_millis(50), cap: Duration::from_millis(50) });
    let until = Some(Instant::now());
    let err = link.pause(3, None, until, "503").await.unwrap_err();
    assert_eq!(err, LinkError::Unavailable("503".into()));
}
