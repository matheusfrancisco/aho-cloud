//! `ModelUpstream` adapter tests against local HTTP stubs (server.md "Testing", `ModelUpstream`
//! row): header allowlist both ways, byte-identical body passthrough (Anthropic), Vertex rewrite,
//! unbuffered streaming, non-2xx passthrough, timeout mapping, `supports`; plus the ADC source.

use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt;
use serde_json::value::RawValue;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use url::Url;
use wiremock::matchers::{body_string_contains, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::vertex::{VERTEX_ANTHROPIC_VERSION, rewrite_body};
use super::*;
use crate::ports::llm::{ModelRoute, ModelUpstream, UpstreamBody, UpstreamError, UpstreamOp, UpstreamRequest};
use crate::ports::vault::SecretBytes;

const KEY: &str = "sk-ant-api03-test-key";

fn anthropic_route() -> ModelRoute {
    ModelRoute::AnthropicApi { api_key: SecretBytes::new(KEY.as_bytes().to_vec()) }
}

fn request(op: UpstreamOp, route: ModelRoute, stream: bool, body: &'static [u8]) -> UpstreamRequest {
    UpstreamRequest { op, route, stream, anthropic_version: None, anthropic_beta: Vec::new(), body: Bytes::from_static(body) }
}

fn short_timeouts() -> UpstreamTimeouts {
    UpstreamTimeouts { connect: Duration::from_secs(2), total: Duration::from_millis(300), stream_idle: Duration::from_millis(300) }
}

fn url(s: &str) -> Url {
    Url::parse(s).unwrap()
}

async fn full_body(body: UpstreamBody) -> Vec<u8> {
    match body {
        UpstreamBody::Full(bytes) => bytes.to_vec(),
        UpstreamBody::Stream(mut s) => {
            let mut out = Vec::new();
            while let Some(chunk) = s.next().await {
                out.extend_from_slice(&chunk.unwrap());
            }
            out
        }
    }
}

/// Client body with odd spacing, key order and escapes: must reach the API byte for byte.
const CLIENT_BODY: &[u8] =
    b"{ \"max_tokens\" : 64,\"model\":\"claude-sonnet-4-5\",\n \"messages\":[{\"role\":\"user\",\"content\":\"h\\u00e9  \\\"x\\\"\"}],\"metadata\":{\"z\":1,\"a\":[1, 2]} }";

#[tokio::test]
async fn anthropic_forwards_body_byte_identical_and_only_its_own_headers() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .insert_header("request-id", "req_123")
                .insert_header("anthropic-ratelimit-tokens-remaining", "999")
                .insert_header("set-cookie", "session=leak")
                .insert_header("x-internal-route", "pod-7")
                .set_body_bytes(b"{\"ok\":true}".to_vec()),
        )
        .expect(1)
        .mount(&server)
        .await;
    let api = AnthropicApi::new(&url(&server.uri())).unwrap();
    let mut req = request(UpstreamOp::Messages, anthropic_route(), false, CLIENT_BODY);
    req.anthropic_beta = vec!["prompt-caching-2024-07-31".into(), "context-1m-2025-08-07".into()];
    let response = api.send(req).await.unwrap();

    assert_eq!(response.head.status, 200);
    let mut names: Vec<&str> = response.head.headers.iter().map(|(n, _)| n.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, ["anthropic-ratelimit-tokens-remaining", "content-type", "request-id"]);
    assert_eq!(full_body(response.body).await, b"{\"ok\":true}");

    let received = server.received_requests().await.unwrap();
    let sent = &received[0];
    assert_eq!(sent.body, CLIENT_BODY, "body forwarded unchanged");
    assert_eq!(sent.headers.get("x-api-key").unwrap(), KEY);
    assert_eq!(sent.headers.get("anthropic-version").unwrap(), "2023-06-01");
    assert_eq!(sent.headers.get("anthropic-beta").unwrap(), "prompt-caching-2024-07-31,context-1m-2025-08-07");
    assert_eq!(sent.headers.get("content-type").unwrap(), "application/json");
    for (name, _) in sent.headers.iter() {
        assert!(
            ["x-api-key", "anthropic-version", "anthropic-beta", "content-type", "content-length", "host", "accept"]
                .contains(&name.as_str()),
            "unexpected upstream request header {name}"
        );
    }
}

#[tokio::test]
async fn anthropic_count_tokens_under_a_base_path_and_client_version() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/proxy/v1/messages/count_tokens"))
        .and(header("anthropic-version", "2023-01-01"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{\"input_tokens\":12}"))
        .expect(1)
        .mount(&server)
        .await;
    let api = AnthropicApi::new(&url(&format!("{}/proxy/", server.uri()))).unwrap();
    let mut req = request(UpstreamOp::CountTokens, anthropic_route(), false, b"{\"model\":\"m\",\"messages\":[]}");
    req.anthropic_version = Some("2023-01-01".into());
    let response = api.send(req).await.unwrap();
    assert_eq!(full_body(response.body).await, b"{\"input_tokens\":12}");
}

#[tokio::test]
async fn non_2xx_answers_are_passed_through_in_full_even_for_streams() {
    let server = MockServer::start().await;
    let error = br#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(529)
                .insert_header("retry-after", "7")
                .insert_header("content-type", "application/json")
                .set_body_bytes(error.to_vec()),
        )
        .mount(&server)
        .await;
    let api = AnthropicApi::new(&url(&server.uri())).unwrap();
    let response = api.send(request(UpstreamOp::Messages, anthropic_route(), true, b"{}")).await.unwrap();
    assert_eq!(response.head.status, 529);
    assert_eq!(response.head.header("retry-after"), Some("7"));
    assert!(matches!(response.body, UpstreamBody::Full(_)), "error answers are read in full");
    assert_eq!(full_body(response.body).await, error);
}

#[tokio::test]
async fn non_stream_total_timeout_maps_to_timeout() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(3)).set_body_string("{}"))
        .mount(&server)
        .await;
    let api = AnthropicApi::with_timeouts(&url(&server.uri()), short_timeouts()).unwrap();
    let err = api.send(request(UpstreamOp::Messages, anthropic_route(), false, b"{}")).await.unwrap_err();
    assert_eq!(err, UpstreamError::Timeout);
}

#[tokio::test]
async fn refused_connection_is_unavailable() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let api = AnthropicApi::with_timeouts(&url(&format!("http://{addr}")), short_timeouts()).unwrap();
    let err = api.send(request(UpstreamOp::Messages, anthropic_route(), false, b"{}")).await.unwrap_err();
    assert!(matches!(err, UpstreamError::Unavailable(_)), "{err:?}");
}

// --- streaming over a raw socket, so chunk timing is under the test's control ---------------

async fn read_request(sock: &mut TcpStream) {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        sock.read_exact(&mut byte).await.unwrap();
        buf.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&buf).to_ascii_lowercase();
    let len = head.lines().find_map(|l| l.strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap())).unwrap_or(0);
    let mut body = vec![0u8; len];
    sock.read_exact(&mut body).await.unwrap();
}

async fn write_chunk(sock: &mut TcpStream, data: &[u8]) {
    sock.write_all(format!("{:x}\r\n", data.len()).as_bytes()).await.unwrap();
    sock.write_all(data).await.unwrap();
    sock.write_all(b"\r\n").await.unwrap();
    sock.flush().await.unwrap();
}

const SSE_HEAD: &[u8] =
    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nrequest-id: req_s\r\nx-secret-hop: 1\r\ntransfer-encoding: chunked\r\n\r\n";

#[tokio::test]
async fn stream_chunks_arrive_before_the_upstream_sends_the_next_one() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        read_request(&mut sock).await;
        sock.write_all(SSE_HEAD).await.unwrap();
        write_chunk(&mut sock, b"event: ping\ndata: {\"type\":\"ping\"}\n\n").await;
        // Hold the second chunk until the client has seen the first one.
        released.await.unwrap();
        write_chunk(&mut sock, b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n").await;
        sock.write_all(b"0\r\n\r\n").await.unwrap();
    });
    let api = AnthropicApi::new(&url(&format!("http://{addr}"))).unwrap();
    let response = api.send(request(UpstreamOp::Messages, anthropic_route(), true, b"{\"stream\":true}")).await.unwrap();
    assert_eq!(response.head.header("request-id"), Some("req_s"));
    assert_eq!(response.head.header("x-secret-hop"), None);
    let UpstreamBody::Stream(mut chunks) = response.body else { panic!("2xx stream answer must stream") };
    let first =
        tokio::time::timeout(Duration::from_secs(5), chunks.next()).await.expect("first chunk not delivered before the second was sent");
    assert_eq!(first.unwrap().unwrap(), Bytes::from_static(b"event: ping\ndata: {\"type\":\"ping\"}\n\n"));
    release.send(()).unwrap();
    let mut rest = Vec::new();
    while let Some(chunk) = chunks.next().await {
        rest.extend_from_slice(&chunk.unwrap());
    }
    assert_eq!(rest, b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
    server.await.unwrap();
}

#[tokio::test]
async fn stalled_stream_ends_with_timeout_after_the_idle_window() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (done, finished) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        read_request(&mut sock).await;
        sock.write_all(SSE_HEAD).await.unwrap();
        write_chunk(&mut sock, b"data: {\"type\":\"ping\"}\n\n").await;
        let _ = finished.await; // stall with the connection open
    });
    let api = AnthropicApi::with_timeouts(&url(&format!("http://{addr}")), short_timeouts()).unwrap();
    let response = api.send(request(UpstreamOp::Messages, anthropic_route(), true, b"{}")).await.unwrap();
    let UpstreamBody::Stream(mut chunks) = response.body else { panic!("expected a stream") };
    assert!(chunks.next().await.unwrap().is_ok());
    assert_eq!(chunks.next().await.unwrap().unwrap_err(), UpstreamError::Timeout);
    assert!(chunks.next().await.is_none(), "the stream ends after the timeout");
    let _ = done.send(());
}

// --- Vertex -----------------------------------------------------------------------------------

#[test]
fn vertex_rewrite_drops_model_adds_version_and_keeps_nested_bytes() {
    let rewritten = rewrite_body(CLIENT_BODY).unwrap();
    let top: std::collections::BTreeMap<String, Box<RawValue>> = serde_json::from_slice(&rewritten).unwrap();
    assert!(!top.contains_key("model"));
    assert_eq!(top["anthropic_version"].get(), format!("\"{VERTEX_ANTHROPIC_VERSION}\""));
    assert_eq!(top["messages"].get(), "[{\"role\":\"user\",\"content\":\"h\\u00e9  \\\"x\\\"\"}]", "nested value not re-encoded");
    assert_eq!(top["metadata"].get(), "{\"z\":1,\"a\":[1, 2]}");
    assert_eq!(top["max_tokens"].get(), "64");

    // A client-supplied anthropic_version is replaced, not duplicated.
    let replaced = rewrite_body(br#"{"anthropic_version":"bedrock-2023-05-31","model":"m"}"#).unwrap();
    assert_eq!(&replaced[..], format!("{{\"anthropic_version\":\"{VERTEX_ANTHROPIC_VERSION}\"}}").as_bytes());

    for bad in [&b"[1,2]"[..], b"\"x\"", b"{\"a\":"] {
        assert!(matches!(rewrite_body(bad), Err(UpstreamError::Invalid(_))));
    }
}

async fn metadata_server(token: &str, expires_in: u64, expected_calls: u64) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/computeMetadata/v1/instance/service-accounts/default/token"))
        .and(header("Metadata-Flavor", "Google"))
        .and(query_param("scopes", adc::CLOUD_PLATFORM_SCOPE))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": token, "expires_in": expires_in, "token_type": "Bearer"
        })))
        .expect(expected_calls)
        .mount(&server)
        .await;
    server
}

fn vertex_route(model: &str) -> ModelRoute {
    ModelRoute::Vertex { project: "mage-prod".into(), region: "us-east5".into(), model: model.into() }
}

#[tokio::test]
async fn vertex_sends_rewritten_body_with_cached_adc_token_to_the_predict_path() {
    let metadata = metadata_server("ya29.token-1", 3600, 1).await;
    let vertex_api = MockServer::start().await;
    let base = "/v1/projects/mage-prod/locations/us-east5/publishers/anthropic/models/claude-sonnet-4-5@20250929";
    Mock::given(method("POST"))
        .and(path(format!("{base}:rawPredict")))
        .and(header("authorization", "Bearer ya29.token-1"))
        .and(body_string_contains(VERTEX_ANTHROPIC_VERSION))
        .respond_with(ResponseTemplate::new(200).set_body_string("{\"usage\":{}}"))
        .expect(1)
        .mount(&vertex_api)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("{base}:streamRawPredict")))
        .respond_with(ResponseTemplate::new(200).insert_header("content-type", "text/event-stream").set_body_string("data: {}\n\n"))
        .expect(1)
        .mount(&vertex_api)
        .await;

    let tokens = AdcTokenSource::metadata_server(url(&format!("{}/", metadata.uri()))).unwrap();
    let vertex = VertexUpstream::with_endpoint(tokens, url(&vertex_api.uri()), UpstreamTimeouts::default()).unwrap();
    let routed = RoutedUpstream::new(AnthropicApi::new(&url("https://api.anthropic.com")).unwrap(), Some(vertex));

    let mut req = request(UpstreamOp::Messages, vertex_route("claude-sonnet-4-5@20250929"), false, CLIENT_BODY);
    req.anthropic_beta = vec!["context-1m-2025-08-07".into()];
    let response = routed.send(req).await.unwrap();
    assert_eq!(response.head.status, 200);
    let streamed = routed
        .send(request(UpstreamOp::Messages, vertex_route("claude-sonnet-4-5@20250929"), true, b"{\"model\":\"x\",\"stream\":true}"))
        .await
        .unwrap();
    assert_eq!(full_body(streamed.body).await, b"data: {}\n\n");

    let received = vertex_api.received_requests().await.unwrap();
    let sent = &received[0];
    assert_eq!(&sent.body[..], &rewrite_body(CLIENT_BODY).unwrap()[..]);
    assert_eq!(sent.headers.get("anthropic-beta").unwrap(), "context-1m-2025-08-07");
    assert!(sent.headers.get("anthropic-version").is_none(), "Vertex takes the version in the body");
    assert!(sent.headers.get("x-api-key").is_none());
}

#[tokio::test]
async fn vertex_rejects_path_unsafe_model_ids_before_sending() {
    let metadata = metadata_server("t", 3600, 0).await;
    let tokens = AdcTokenSource::metadata_server(url(&format!("{}/", metadata.uri()))).unwrap();
    let vertex = VertexUpstream::with_endpoint(tokens, url("http://127.0.0.1:9"), UpstreamTimeouts::default()).unwrap();
    for model in ["../../evil", "a/b", "m?x=1", ""] {
        let err = vertex.send(request(UpstreamOp::Messages, vertex_route(model), false, b"{}")).await.unwrap_err();
        assert!(matches!(err, UpstreamError::Invalid(_)), "{model}: {err:?}");
    }
}

#[test]
fn supports_follows_the_route_and_configuration() {
    let anthropic = || AnthropicApi::new(&url("https://api.anthropic.com")).unwrap();
    let without_vertex = RoutedUpstream::new(anthropic(), None);
    assert!(without_vertex.supports(&anthropic_route(), UpstreamOp::Messages));
    assert!(without_vertex.supports(&anthropic_route(), UpstreamOp::CountTokens));
    assert!(!without_vertex.supports(&vertex_route("m"), UpstreamOp::Messages));

    let tokens = AdcTokenSource::metadata_server(url("http://127.0.0.1:9/")).unwrap();
    let vertex = VertexUpstream::with_endpoint(tokens, url("http://127.0.0.1:9"), UpstreamTimeouts::default()).unwrap();
    let with_vertex = RoutedUpstream::new(anthropic(), Some(vertex));
    assert!(with_vertex.supports(&vertex_route("m"), UpstreamOp::Messages));
    assert!(!with_vertex.supports(&vertex_route("m"), UpstreamOp::CountTokens));
}

#[tokio::test]
async fn vertex_route_without_vertex_adapter_is_unavailable() {
    let routed = RoutedUpstream::new(AnthropicApi::new(&url("https://api.anthropic.com")).unwrap(), None);
    let err = routed.send(request(UpstreamOp::Messages, vertex_route("m"), false, b"{}")).await.unwrap_err();
    assert!(matches!(err, UpstreamError::Unavailable(_)));
}

// --- ADC ----------------------------------------------------------------------------------------

#[tokio::test]
async fn adc_refetches_tokens_that_expire_within_the_early_refresh_window() {
    // 100 s lifetime < 5 min early refresh: every call fetches.
    let metadata = metadata_server("short", 100, 2).await;
    let tokens = AdcTokenSource::metadata_server(url(&format!("{}/", metadata.uri()))).unwrap();
    assert_eq!(tokens.access_token().await.unwrap(), "short");
    assert_eq!(tokens.access_token().await.unwrap(), "short");
}

#[tokio::test]
async fn adc_authorized_user_uses_the_refresh_token_grant() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .and(body_string_contains("refresh_token=1%2F%2Frt"))
        .and(body_string_contains("client_id=cid"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"access_token": "ya29.user", "expires_in": 3599})))
        .expect(1)
        .mount(&server)
        .await;
    let json = serde_json::json!({
        "type": "authorized_user", "client_id": "cid", "client_secret": "cs", "refresh_token": "1//rt",
        "token_uri": format!("{}/token", server.uri()),
    });
    let tokens = AdcTokenSource::from_credentials_json(json.to_string().as_bytes()).unwrap();
    assert_eq!(tokens.access_token().await.unwrap(), "ya29.user");
    assert_eq!(tokens.access_token().await.unwrap(), "ya29.user", "cached");
}

#[tokio::test]
async fn adc_refuses_key_files_and_reports_endpoint_failures() {
    let key_file = serde_json::json!({"type": "service_account", "client_email": "a@b", "private_key": "-----BEGIN"});
    assert!(matches!(AdcTokenSource::from_credentials_json(key_file.to_string().as_bytes()), Err(AdcError::Config(_))));
    assert!(matches!(AdcTokenSource::from_credentials_json(br#"{"type":"authorized_user"}"#), Err(AdcError::Config(_))));

    let server = MockServer::start().await;
    Mock::given(method("GET")).respond_with(ResponseTemplate::new(403).set_body_string("denied")).mount(&server).await;
    let tokens = AdcTokenSource::metadata_server(url(&format!("{}/", server.uri()))).unwrap();
    assert!(matches!(tokens.access_token().await, Err(AdcError::Unavailable(_))));
}
