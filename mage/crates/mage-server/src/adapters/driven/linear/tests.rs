//! `LinearTracker` against a local GraphQL mock (intake.md "Testing": adapter integration).

use std::sync::Arc;

use mage_core::domain::{Integration, IntegrationKind, IntegrationStatus, OrgId};
use mage_core::ports::Clock;
use mage_core::testing::{FakeClock, MemJobQueue, SeqIdGen};
use serde_json::{json, Value};
use time::macros::datetime;
use time::{Duration, OffsetDateTime};
use url::Url;
use uuid::Uuid;
use wiremock::matchers::{body_bytes, body_partial_json, body_string_contains, header, headers, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{LinearConfig, LinearTracker};
use crate::domain::intake::{secret_names, IntegrationSettings, LinearAuthMode};
use crate::ports::audit::Actor;
use crate::ports::org::OrgStore;
use crate::ports::tracker::{Author, CommentDraft, TicketTracker, TrackerError, TrackerStateKind, TrackerUpload};
use crate::ports::vault::{Activation, SecretBytes, SecretName, SecretScope, SecretVault};
use crate::testing::vault::MemSecretVault;
use crate::testing::{MemChangeFeed, MemOrgStore};

const WS: &str = "0c2f6a8e-1111-4b2c-9d3e-5f6a7b8c9d0e";
const API_KEY: &str = "lin_api_0123456789abcdef";
const NOW: OffsetDateTime = datetime!(2026-10-05 12:00 UTC);

struct World {
    server: MockServer,
    tracker: LinearTracker,
    vault: Arc<MemSecretVault>,
    clock: Arc<FakeClock>,
    org: OrgId,
}

async fn put_secret(vault: &MemSecretVault, scope: SecretScope, name: &str, value: &str) {
    vault.put(&scope, &SecretName::new(name).unwrap(), SecretBytes::from(value.to_owned()), Activation::Now, &Actor::System).await.unwrap();
}

async fn world(mode: LinearAuthMode) -> World {
    let server = MockServer::start().await;
    let clock = Arc::new(FakeClock::at(NOW));
    let dyn_clock: Arc<dyn Clock> = clock.clone();
    let vault = Arc::new(MemSecretVault::new(dyn_clock.clone()));
    let jobs = Arc::new(MemJobQueue::new(dyn_clock.clone(), Arc::new(SeqIdGen::new())));
    let orgs = Arc::new(MemOrgStore::new(dyn_clock.clone(), jobs, Arc::new(MemChangeFeed::new(dyn_clock.clone()))));
    let org = OrgId::from_uuid(Uuid::from_u128(7));
    orgs.upsert_integration(&Integration { org_id: org, kind: IntegrationKind::Linear, external_id: WS.into(), status: IntegrationStatus::Active }, &[])
        .await
        .unwrap();
    orgs.set_integration_settings(org, IntegrationKind::Linear, WS, &IntegrationSettings { linear_auth: Some(mode), ..IntegrationSettings::default() })
        .await
        .unwrap();
    match mode {
        LinearAuthMode::ApiKey => put_secret(&vault, SecretScope::Org(org), &secret_names::linear_api_key(WS), API_KEY).await,
        LinearAuthMode::OAuth => {
            let grant = json!({ "access_token": "old-token", "refresh_token": "refresh-1", "expires_at": "2026-10-06T12:00:00Z" });
            put_secret(&vault, SecretScope::Org(org), &secret_names::linear_oauth(WS), &grant.to_string()).await;
            put_secret(&vault, SecretScope::Platform, secret_names::LINEAR_OAUTH_CLIENT_SECRET, "client-secret").await;
        }
    }
    let config = LinearConfig {
        api_url: Url::parse(&format!("{}/graphql", server.uri())).unwrap(),
        oauth_token_url: Url::parse(&format!("{}/oauth/token", server.uri())).unwrap(),
        oauth_client_id: Some("client-1".into()),
        request_timeout_ms: 2_000,
        ..LinearConfig::default()
    };
    let tracker = LinearTracker::new(&config, vault.clone(), orgs, dyn_clock).unwrap();
    World { server, tracker, vault, clock, org }
}

fn op(name: &str) -> wiremock::matchers::BodyPartialJsonMatcher {
    body_partial_json(json!({ "operationName": name }))
}

fn data(body: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({ "data": body }))
}

fn epoch_ms(at: OffsetDateTime) -> String {
    (at.unix_timestamp_nanos() / 1_000_000).to_string()
}

fn recorded_issue() -> Value {
    json!({
        "issue": {
            "id": "9b1e0f4c-aaaa-4bbb-8ccc-000000000001",
            "identifier": "SUP-42",
            "url": "https://linear.app/hoop/issue/SUP-42/login-fails",
            "title": "Login fails with 500",
            "description": "Customer sees `TypeError: x is undefined`",
            "createdAt": "2026-10-01T09:00:00.000Z",
            "updatedAt": "2026-10-04T17:30:12.345Z",
            "archivedAt": null,
            "team": { "key": "SUP" },
            "project": { "name": "Auth" },
            "state": { "name": "Triage", "type": "triage" },
            "labels": { "nodes": [{ "name": "bug" }, { "name": "customer:acme" }] },
            "attachments": { "nodes": [
                { "url": "https://hoop.slack.com/archives/C123/p1759312680000100", "title": "Slack thread", "sourceType": "slack",
                  "metadata": { "channelId": "C123", "messageCount": 4, "nothing": null } },
                { "url": "not a url", "title": "broken", "sourceType": null, "metadata": null }
            ] },
            "comments": { "nodes": [
                { "id": "c-2", "body": "Mage triage ready", "createdAt": "2026-10-02T10:00:00.000Z", "user": { "id": "u-mage" }, "botActor": { "id": "app-1" } },
                { "id": "c-1", "body": "Still broken", "createdAt": "2026-10-01T10:00:00.000Z", "user": { "id": "u-ana" }, "botActor": null }
            ] },
            "needs": { "nodes": [{ "customer": { "id": "cust-1" } }, { "customer": null }, { "customer": { "id": "cust-1" } }] }
        }
    })
}

#[tokio::test]
async fn get_issue_maps_the_recorded_shape_with_a_bare_api_key_header() {
    let w = world(LinearAuthMode::ApiKey).await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(header("authorization", API_KEY))
        .and(op("MageIssue"))
        .and(body_partial_json(json!({ "variables": { "id": "SUP-42" } })))
        .respond_with(data(recorded_issue()))
        .expect(1)
        .mount(&w.server)
        .await;

    let issue = w.tracker.get_issue(w.org, "SUP-42").await.unwrap();
    assert_eq!(issue.issue_id, "9b1e0f4c-aaaa-4bbb-8ccc-000000000001");
    assert_eq!(issue.identifier, "SUP-42");
    assert_eq!(issue.workspace_id, WS);
    assert_eq!((issue.team_key.as_str(), issue.project.as_deref()), ("SUP", Some("Auth")));
    assert_eq!((issue.state_name.as_str(), issue.state_kind), ("Triage", TrackerStateKind::Triage));
    assert_eq!(issue.labels, ["bug", "customer:acme"]);
    assert_eq!(issue.updated_at, datetime!(2026-10-04 17:30:12.345 UTC));
    assert!(!issue.archived);
    let ids: Vec<&str> = issue.comments.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, ["c-1", "c-2"], "oldest first");
    assert_eq!(issue.comments[0].author, Author::Human { id: "u-ana".into() });
    assert_eq!(issue.comments[1].author, Author::Bot { id: "app-1".into() });
    assert_eq!(issue.attachments.len(), 1, "an attachment with an unparsable url is dropped");
    let slack = &issue.attachments[0];
    assert_eq!(slack.source_kind.as_deref(), Some("slack"));
    assert_eq!(slack.metadata.get("channelId").map(String::as_str), Some("C123"));
    assert_eq!(slack.metadata.get("messageCount").map(String::as_str), Some("4"));
    assert!(!slack.metadata.contains_key("nothing"));
    assert_eq!(issue.customer_ids, ["cust-1"]);
}

#[tokio::test]
async fn get_issue_of_a_missing_entity_is_not_found() {
    let w = world(LinearAuthMode::ApiKey).await;
    let body = json!({ "data": null, "errors": [{ "message": "Entity not found: Issue", "extensions": { "type": "invalid input", "code": "INPUT_ERROR", "userError": true } }] });
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(400).set_body_json(body)).mount(&w.server).await;
    assert_eq!(w.tracker.get_issue(w.org, "nope").await, Err(TrackerError::NotFound));
}

#[tokio::test]
async fn comment_with_a_repeated_client_id_returns_the_existing_comment() {
    let w = world(LinearAuthMode::ApiKey).await;
    let client_id = Uuid::from_u128(0xabc);
    let draft = CommentDraft { client_id, body_markdown: "**Triage ready**".into() };
    Mock::given(method("POST"))
        .and(op("MageCommentCreate"))
        .and(body_partial_json(json!({ "variables": { "input": { "id": client_id.to_string(), "issueId": "issue-1", "body": "**Triage ready**" } } })))
        .respond_with(data(json!({ "commentCreate": { "success": true, "comment": { "id": client_id.to_string(), "url": "https://linear.app/hoop/issue/SUP-42#comment-abc" } } })))
        .up_to_n_times(1)
        .expect(1)
        .mount(&w.server)
        .await;
    let first = w.tracker.comment(w.org, "issue-1", &draft).await.unwrap();
    assert!(first.created);
    assert_eq!(first.id, client_id.to_string());

    // The earlier attempt landed: Linear refuses the id; the adapter reads the existing comment.
    let duplicate = json!({ "data": null, "errors": [{ "message": "Comment with this id already exists", "extensions": { "type": "invalid input", "code": "INPUT_ERROR", "userError": true } }] });
    Mock::given(method("POST")).and(op("MageCommentCreate")).respond_with(ResponseTemplate::new(400).set_body_json(duplicate)).expect(1).mount(&w.server).await;
    Mock::given(method("POST"))
        .and(op("MageComment"))
        .and(body_partial_json(json!({ "variables": { "id": client_id.to_string() } })))
        .respond_with(data(json!({ "comment": { "id": client_id.to_string(), "url": "https://linear.app/hoop/issue/SUP-42#comment-abc" } })))
        .expect(1)
        .mount(&w.server)
        .await;
    let again = w.tracker.comment(w.org, "issue-1", &draft).await.unwrap();
    assert!(!again.created);
    assert_eq!(again.id, client_id.to_string());
    assert_eq!(again.url.as_ref().map(Url::as_str), Some("https://linear.app/hoop/issue/SUP-42#comment-abc"));
}

#[tokio::test]
async fn upload_file_puts_the_bytes_to_the_presigned_url_with_the_returned_headers() {
    let w = world(LinearAuthMode::ApiKey).await;
    let bytes = b"\x00\x00\x00\x18ftypmp42 fake video".to_vec();
    let upload_url = format!("{}/storage/upload/abc?X-Goog-Signature=sig", w.server.uri());
    let asset_url = "https://uploads.linear.app/ws/abc/repro.mp4";
    Mock::given(method("POST"))
        .and(op("MageFileUpload"))
        .and(body_partial_json(json!({ "variables": { "contentType": "video/mp4", "filename": "repro.mp4", "size": bytes.len() } })))
        .respond_with(data(json!({ "fileUpload": { "success": true, "uploadFile": {
            "uploadUrl": upload_url, "assetUrl": asset_url,
            "headers": [{ "key": "x-goog-meta-workspace", "value": WS }, { "key": "Content-Disposition", "value": "attachment; filename=\"repro.mp4\"" }]
        } } })))
        .expect(1)
        .mount(&w.server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/storage/upload/abc"))
        .and(header("x-goog-meta-workspace", WS))
        .and(header("content-disposition", "attachment; filename=\"repro.mp4\""))
        .and(header("content-type", "video/mp4"))
        // wiremock splits header values on ',' before comparing.
        .and(headers("cache-control", vec!["public", "max-age=31536000"]))
        .and(body_bytes(bytes.clone()))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&w.server)
        .await;

    let file = TrackerUpload { file_name: "repro.mp4".into(), content_type: "video/mp4".into(), bytes };
    let url = w.tracker.upload_file(w.org, &file).await.unwrap();
    assert_eq!(url.as_str(), asset_url);
    let requests = w.server.received_requests().await.unwrap();
    let put = requests.iter().find(|r| r.method.as_str() == "PUT").unwrap();
    assert!(!put.headers.contains_key("authorization"), "the Linear credential never goes to storage");
}

#[tokio::test]
async fn ratelimited_error_maps_to_rate_limited_at_the_reset() {
    let w = world(LinearAuthMode::ApiKey).await;
    let reset = NOW + Duration::minutes(17);
    let body = json!({ "errors": [{ "message": "Rate limit exceeded", "extensions": { "code": "RATELIMITED", "type": "ratelimited" } }] });
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_json(body)
                .insert_header("x-ratelimit-requests-limit", "2500")
                .insert_header("x-ratelimit-requests-remaining", "0")
                .insert_header("x-ratelimit-requests-reset", epoch_ms(reset).as_str()),
        )
        .expect(1)
        .mount(&w.server)
        .await;
    assert_eq!(w.tracker.get_issue(w.org, "SUP-1").await, Err(TrackerError::RateLimited { retry_at: reset }));
}

#[tokio::test]
async fn stops_before_calling_under_five_percent_until_the_reset() {
    let w = world(LinearAuthMode::ApiKey).await;
    let reset = NOW + Duration::minutes(30);
    Mock::given(method("POST"))
        .respond_with(
            data(recorded_issue())
                .insert_header("x-ratelimit-complexity-limit", "3000000")
                .insert_header("x-ratelimit-complexity-remaining", "149999")
                .insert_header("x-ratelimit-complexity-reset", epoch_ms(reset).as_str())
                .insert_header("x-ratelimit-requests-limit", "2500")
                .insert_header("x-ratelimit-requests-remaining", "2000")
                .insert_header("x-ratelimit-requests-reset", epoch_ms(reset).as_str()),
        )
        .expect(2)
        .mount(&w.server)
        .await;
    w.tracker.get_issue(w.org, "SUP-42").await.unwrap();
    assert_eq!(w.tracker.get_issue(w.org, "SUP-42").await, Err(TrackerError::RateLimited { retry_at: reset }), "no request is sent");
    w.clock.set(reset);
    w.tracker.get_issue(w.org, "SUP-42").await.expect("budget reset");
}

#[tokio::test]
async fn http_401_with_an_api_key_is_unauthorized() {
    let w = world(LinearAuthMode::ApiKey).await;
    let body = json!({ "errors": [{ "message": "Authentication required, not authenticated", "extensions": { "type": "authentication error", "code": "AUTHENTICATION_ERROR" } }] });
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(401).set_body_json(body)).expect(1).mount(&w.server).await;
    assert_eq!(w.tracker.get_issue(w.org, "SUP-1").await, Err(TrackerError::Unauthorized));
}

#[tokio::test]
async fn org_without_an_active_linear_integration_is_unauthorized() {
    let w = world(LinearAuthMode::ApiKey).await;
    let other = OrgId::from_uuid(Uuid::from_u128(8));
    assert_eq!(w.tracker.get_issue(other, "SUP-1").await, Err(TrackerError::Unauthorized));
}

#[tokio::test]
async fn oauth_sends_bearer_and_refreshes_once_after_a_401() {
    let w = world(LinearAuthMode::OAuth).await;
    let unauthenticated = json!({ "errors": [{ "message": "Authentication required", "extensions": { "code": "AUTHENTICATION_ERROR" } }] });
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(header("authorization", "Bearer old-token"))
        .respond_with(ResponseTemplate::new(401).set_body_json(unauthenticated))
        .expect(1)
        .mount(&w.server)
        .await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .and(body_string_contains("refresh_token=refresh-1"))
        .and(body_string_contains("client_id=client-1"))
        .and(body_string_contains("client_secret=client-secret"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "access_token": "new-token", "token_type": "Bearer", "expires_in": 86399, "scope": "read,write" })))
        .expect(1)
        .mount(&w.server)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(header("authorization", "Bearer new-token"))
        .and(op("MageViewerApp"))
        .and(body_partial_json(json!({ "variables": { "clientId": "client-1" } })))
        .respond_with(data(json!({ "viewer": { "id": "app-user-1" }, "applicationInfo": { "id": "app-1" } })))
        .expect(1)
        .mount(&w.server)
        .await;

    let identity = w.tracker.identity(w.org).await.unwrap();
    assert_eq!((identity.user_id.as_str(), identity.app_id.as_deref()), ("app-user-1", Some("app-1")));

    let (_, stored) = w.vault.load(&SecretScope::Org(w.org), &SecretName::new(secret_names::linear_oauth(WS)).unwrap()).await.unwrap();
    let stored: Value = serde_json::from_slice(stored.expose()).unwrap();
    assert_eq!(stored["access_token"], "new-token");
    assert_eq!(stored["refresh_token"], "refresh-1", "a refresh without a new refresh token keeps the old one");
    assert_eq!(stored["expires_at"], "2026-10-06T11:59:59Z");

    // Cached for an hour: no further request.
    w.tracker.identity(w.org).await.unwrap();
}

#[tokio::test]
async fn complete_install_exchanges_the_code_and_reads_the_workspace() {
    let w = world(LinearAuthMode::OAuth).await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_string_contains("grant_type=authorization_code"))
        .and(body_string_contains("code=the-code"))
        .and(body_string_contains("code_verifier=verifier-1"))
        .and(body_string_contains("redirect_uri=https%3A%2F%2Fmage.example%2Fintegrations%2Flinear%2Fcallback"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "fresh", "refresh_token": "r-2", "token_type": "Bearer", "expires_in": 3600, "scope": "read write comments:create"
        })))
        .expect(1)
        .mount(&w.server)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(header("authorization", "Bearer fresh"))
        .and(op("MageViewerOrganization"))
        .respond_with(data(json!({ "viewer": { "organization": { "id": WS, "name": "Hoop" } } })))
        .expect(1)
        .mount(&w.server)
        .await;
    let redirect = Url::parse("https://mage.example/integrations/linear/callback").unwrap();
    let grant = w.tracker.complete_install("the-code", &redirect, "verifier-1").await.unwrap();
    assert_eq!((grant.workspace_id.as_str(), grant.workspace_name.as_str()), (WS, "Hoop"));
    assert_eq!(grant.access_token.expose(), "fresh");
    assert_eq!(grant.refresh_token.as_ref().map(|t| t.expose()), Some("r-2"));
    assert_eq!(grant.expires_at, Some(NOW + Duration::hours(1)));
    assert_eq!(grant.scopes, ["read", "write", "comments:create"]);
}

#[tokio::test]
async fn set_state_looks_up_the_team_state_and_skips_when_already_there() {
    let w = world(LinearAuthMode::ApiKey).await;
    Mock::given(method("POST"))
        .and(op("MageIssueState"))
        .respond_with(data(json!({ "issue": { "id": "issue-1", "state": { "id": "s-todo", "name": "Todo" }, "team": { "id": "team-1" } } })))
        .expect(3)
        .mount(&w.server)
        .await;
    Mock::given(method("POST"))
        .and(op("MageTeamStates"))
        .respond_with(data(json!({ "team": { "states": { "nodes": [{ "id": "s-todo", "name": "Todo" }, { "id": "s-review", "name": "In Review" }] } } })))
        // Once for "in review"; "Todo" needs no lookup; an unknown name refetches the cached list.
        .expect(2)
        .mount(&w.server)
        .await;
    Mock::given(method("POST"))
        .and(op("MageIssueUpdate"))
        .and(body_partial_json(json!({ "variables": { "id": "issue-1", "input": { "stateId": "s-review" } } })))
        .respond_with(data(json!({ "issueUpdate": { "success": true } })))
        .expect(1)
        .mount(&w.server)
        .await;
    w.tracker.set_state(w.org, "issue-1", "in review").await.unwrap();
    w.tracker.set_state(w.org, "issue-1", "Todo").await.unwrap();
    assert!(matches!(w.tracker.set_state(w.org, "issue-1", "Shipped").await, Err(TrackerError::Invalid(_))));
}
