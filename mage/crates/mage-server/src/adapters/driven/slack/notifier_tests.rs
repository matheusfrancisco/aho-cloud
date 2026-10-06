#![allow(clippy::unwrap_used)]

//! `SlackNotifier` against a local Slack Web API mock (intake.md "Testing": adapter integration).

use std::sync::Arc;
use std::time::Duration;

use mage_core::domain::{Integration, IntegrationKind, IntegrationStatus, OrgId, SlackThreadRef};
use mage_core::ports::Clock;
use mage_core::testing::{FakeClock, MemJobQueue, SeqIdGen};
use serde_json::{Value, json};
use time::OffsetDateTime;
use url::Url;
use uuid::Uuid;
use wiremock::matchers::{body_json, body_string_contains, header, method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{SlackConfig, SlackNotifier};
use crate::domain::intake::secret_names;
use crate::ports::audit::Actor;
use crate::ports::chat::{ChatAuthor, ChatError, ChatNotifier, ChatPost};
use crate::ports::org::OrgStore;
use crate::ports::vault::{Activation, SecretBytes, SecretName, SecretScope, SecretVault};
use crate::testing::vault::MemSecretVault;
use crate::testing::{MemChangeFeed, MemOrgStore};

const TEAM: &str = "T024BE7LD";
const TOKEN: &str = "xoxb-test-token";
const BOT_USER: &str = "UBOT";
const BOT_ID: &str = "BBOT";

struct World {
    server: MockServer,
    notifier: SlackNotifier,
    vault: Arc<MemSecretVault>,
    orgs: Arc<MemOrgStore>,
    clock: Arc<FakeClock>,
    org: OrgId,
}

fn thread() -> SlackThreadRef {
    SlackThreadRef { team: TEAM.into(), channel: "C123".into(), ts: "1759312680.000100".into() }
}

fn ok(body: Value) -> ResponseTemplate {
    let mut body = body;
    body["ok"] = json!(true);
    ResponseTemplate::new(200).set_body_json(body)
}

fn slack_error(code: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({ "ok": false, "error": code }))
}

async fn world_with(tweak: impl FnOnce(&mut SlackConfig)) -> World {
    let server = MockServer::start().await;
    let clock = Arc::new(FakeClock::new());
    let dyn_clock: Arc<dyn Clock> = clock.clone();
    let vault = Arc::new(MemSecretVault::new(dyn_clock.clone()));
    let jobs = Arc::new(MemJobQueue::new(dyn_clock.clone(), Arc::new(SeqIdGen::new())));
    let orgs = Arc::new(MemOrgStore::new(dyn_clock.clone(), jobs, Arc::new(MemChangeFeed::new(dyn_clock))));
    let org = OrgId::from_uuid(Uuid::from_u128(7));
    orgs.upsert_integration(&Integration { org_id: org, kind: IntegrationKind::Slack, external_id: TEAM.into(), status: IntegrationStatus::Active }, &[])
        .await
        .unwrap();
    put_secret(&vault, SecretScope::Org(org), &secret_names::slack_bot_token(TEAM), TOKEN).await;
    let mut config = SlackConfig {
        api_base: Url::parse(&format!("{}/api/", server.uri())).unwrap(),
        request_timeout_ms: 2_000,
        post_interval_ms: 0,
        ..SlackConfig::default()
    };
    tweak(&mut config);
    let notifier = SlackNotifier::new(config, vault.clone(), orgs.clone(), clock.clone()).unwrap();
    World { server, notifier, vault, orgs, clock, org }
}

async fn world() -> World {
    world_with(|_| {}).await
}

async fn put_secret(vault: &MemSecretVault, scope: SecretScope, name: &str, value: &str) {
    vault.put(&scope, &SecretName::new(name).unwrap(), SecretBytes::from(value.to_owned()), Activation::Now, &Actor::System).await.unwrap();
}

/// `auth.test` for `token`, expected exactly `times` times.
async fn mount_auth_test(server: &MockServer, token: &str, times: impl Into<wiremock::Times>) {
    Mock::given(method("GET"))
        .and(path("/api/auth.test"))
        .and(header("authorization", format!("Bearer {token}").as_str()))
        .respond_with(ok(json!({ "url": "https://acme.slack.com/", "team": "Acme", "team_id": TEAM, "user_id": BOT_USER, "bot_id": BOT_ID })))
        .expect(times)
        .mount(server)
        .await;
}

fn human(ts: &str, user: &str, text: &str) -> Value {
    json!({ "type": "message", "ts": ts, "user": user, "text": text, "thread_ts": "1759312680.000100" })
}

fn ours(ts: &str, key: &str) -> Value {
    json!({
        "type": "message", "ts": ts, "user": BOT_USER, "bot_id": BOT_ID, "text": "triage ready",
        "metadata": { "event_type": "mage_notice", "event_payload": { "key": key } }
    })
}

fn foreign_bot(ts: &str, key: &str) -> Value {
    json!({
        "type": "message", "subtype": "bot_message", "ts": ts, "bot_id": "BOTHER", "text": "spoof",
        "metadata": { "event_type": "mage_notice", "event_payload": { "key": key } }
    })
}

fn page(messages: Vec<Value>, next: Option<&str>) -> ResponseTemplate {
    ok(json!({
        "messages": messages,
        "has_more": next.is_some(),
        "response_metadata": { "next_cursor": next.unwrap_or("") },
    }))
}

#[tokio::test]
async fn read_thread_pages_marks_ours_and_extracts_metadata_key() {
    let w = world().await;
    mount_auth_test(&w.server, TOKEN, 1).await;
    let root = json!({
        "type": "message", "ts": "1759312680.000100", "user": "U1", "text": "checkout broken",
        "files": [{ "id": "F1", "name": "trace.txt" }, { "id": "F2", "title": "screenshot" }]
    });
    Mock::given(method("GET"))
        .and(path("/api/conversations.replies"))
        .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
        .and(query_param("channel", "C123"))
        .and(query_param("ts", "1759312680.000100"))
        .and(query_param("limit", "200"))
        .and(query_param("include_all_metadata", "true"))
        .and(query_param_is_missing("cursor"))
        .respond_with(page(vec![root.clone(), ours("1759312690.000200", "tkt_1/slack/triage_ready")], Some("c2")))
        .expect(1)
        .mount(&w.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/conversations.replies"))
        .and(query_param("cursor", "c2"))
        .respond_with(page(vec![root, foreign_bot("1759312700.000300", "tkt_1/slack/triage_ready"), human("1759312710.000400", "U2", "+1")], None))
        .expect(1)
        .mount(&w.server)
        .await;

    let got = w.notifier.read_thread(w.org, &thread(), 10).await.unwrap();

    assert!(!got.truncated);
    assert_eq!(got.thread, thread());
    let authors: Vec<_> = got.messages.iter().map(|m| m.author.clone()).collect();
    assert_eq!(
        authors,
        vec![
            ChatAuthor::Human { user_id: "U1".into() },
            ChatAuthor::Bot { id: BOT_ID.into(), ours: true },
            ChatAuthor::Bot { id: "BOTHER".into(), ours: false },
            ChatAuthor::Human { user_id: "U2".into() },
        ]
    );
    assert_eq!(got.messages[0].file_names, vec!["trace.txt".to_owned(), "screenshot".to_owned()]);
    assert_eq!(got.messages[0].at, OffsetDateTime::from_unix_timestamp_nanos(1_759_312_680_000_100_000).unwrap());
    assert_eq!(got.messages[1].mage_key.as_deref(), Some("tkt_1/slack/triage_ready"));
    // Another app's message carrying our event type is not ours.
    assert_eq!(got.messages[2].mage_key, None);
}

#[tokio::test]
async fn read_thread_stops_at_max_messages_and_reports_truncation() {
    let w = world_with(|c| c.replies_page_limit = 15).await;
    mount_auth_test(&w.server, TOKEN, 1).await;
    Mock::given(method("GET"))
        .and(path("/api/conversations.replies"))
        .and(query_param("limit", "15"))
        .and(query_param_is_missing("cursor"))
        .respond_with(page(
            vec![human("1759312680.000100", "U1", "a"), human("1759312681.000000", "U2", "b"), human("1759312682.000000", "U3", "c")],
            Some("c2"),
        ))
        .mount(&w.server)
        .await;
    Mock::given(method("GET")).and(path("/api/conversations.replies")).and(query_param("cursor", "c2")).respond_with(page(vec![], None)).expect(0).mount(&w.server).await;

    let cut_inside_page = w.notifier.read_thread(w.org, &thread(), 2).await.unwrap();
    assert_eq!(cut_inside_page.messages.len(), 2);
    assert!(cut_inside_page.truncated);

    // Exactly a full page with more pages behind it: truncated without fetching the next page.
    let cut_at_page_end = w.notifier.read_thread(w.org, &thread(), 3).await.unwrap();
    assert_eq!(cut_at_page_end.messages.len(), 3);
    assert!(cut_at_page_end.truncated);
}

#[tokio::test]
async fn post_reply_sends_escaped_text_metadata_and_no_unfurls() {
    let w = world().await;
    mount_auth_test(&w.server, TOKEN, 1).await;
    let expected = json!({
        "channel": "C123",
        "thread_ts": "1759312680.000100",
        "text": "Verdict: a &amp; b &lt;!channel&gt; &lt;https://evil|click&gt;\n<https://mage.example/tickets/tkt_1|Open &lt;in&gt; Mage>",
        "unfurl_links": false,
        "unfurl_media": false,
        "metadata": { "event_type": "mage_notice", "event_payload": { "key": "tkt_1/slack/triage_ready" } },
    });
    Mock::given(method("POST"))
        .and(path("/api/chat.postMessage"))
        .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
        .and(body_json(expected))
        .respond_with(ok(json!({ "channel": "C123", "ts": "1759312799.000900" })))
        .expect(1)
        .mount(&w.server)
        .await;

    let post = ChatPost {
        key: "tkt_1/slack/triage_ready".into(),
        text: "Verdict: a & b <!channel> <https://evil|click>".into(),
        link: Some((Url::parse("https://mage.example/tickets/tkt_1").unwrap(), "Open |<in> Mage".into())),
    };
    let posted = w.notifier.post_reply(w.org, &thread(), &post).await.unwrap();
    assert_eq!(posted.ts, "1759312799.000900");
}

#[tokio::test]
async fn post_reply_paces_posts_per_channel() {
    let w = world_with(|c| c.post_interval_ms = 300).await;
    mount_auth_test(&w.server, TOKEN, 1).await;
    Mock::given(method("POST"))
        .and(path("/api/chat.postMessage"))
        .respond_with(ok(json!({ "ts": "1.000001" })))
        .expect(2)
        .mount(&w.server)
        .await;
    let post = ChatPost { key: "k".into(), text: "x".into(), link: None };
    let started = std::time::Instant::now();
    w.notifier.post_reply(w.org, &thread(), &post).await.unwrap();
    w.notifier.post_reply(w.org, &thread(), &post).await.unwrap();
    assert!(started.elapsed() >= Duration::from_millis(300), "second post went out after {:?}", started.elapsed());
}

#[tokio::test]
async fn find_reply_returns_our_message_with_the_key() {
    let w = world().await;
    mount_auth_test(&w.server, TOKEN, 1).await;
    let since = OffsetDateTime::from_unix_timestamp_nanos(1_759_312_690_000_000_000).unwrap();
    Mock::given(method("GET"))
        .and(path("/api/conversations.replies"))
        .and(query_param("oldest", "1759312690.000000"))
        .and(query_param("inclusive", "true"))
        .and(query_param("include_all_metadata", "true"))
        .and(query_param_is_missing("cursor"))
        .respond_with(page(
            vec![
                human("1759312680.000100", "U1", "root"),
                ours("1759312691.000000", "tkt_1/slack/other"),
                foreign_bot("1759312692.000000", "tkt_1/slack/triage_ready"),
            ],
            Some("c2"),
        ))
        .mount(&w.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/conversations.replies"))
        .and(query_param("cursor", "c2"))
        .respond_with(page(vec![ours("1759312693.000000", "tkt_1/slack/triage_ready")], None))
        .mount(&w.server)
        .await;

    let found = w.notifier.find_reply(w.org, &thread(), "tkt_1/slack/triage_ready", since).await.unwrap();
    assert_eq!(found.map(|p| p.ts).as_deref(), Some("1759312693.000000"));
    let missing = w.notifier.find_reply(w.org, &thread(), "tkt_1/slack/pr_opened", since).await.unwrap();
    assert_eq!(missing, None);
}

#[tokio::test]
async fn slack_errors_map_to_chat_errors() {
    let cases: [(&str, ChatError); 9] = [
        ("not_in_channel", ChatError::NoAccess("not_in_channel".into())),
        ("missing_scope", ChatError::NoAccess("missing_scope".into())),
        ("channel_not_found", ChatError::NotFound),
        ("thread_not_found", ChatError::NotFound),
        ("invalid_auth", ChatError::Unauthorized),
        ("token_revoked", ChatError::Unauthorized),
        ("account_inactive", ChatError::Unauthorized),
        ("msg_too_long", ChatError::Invalid("msg_too_long".into())),
        ("invalid_blocks", ChatError::Invalid("invalid_blocks".into())),
    ];
    let w = world().await;
    let post = ChatPost { key: "k".into(), text: "x".into(), link: None };
    for (code, expected) in cases {
        w.server.reset().await;
        mount_auth_test(&w.server, TOKEN, 0..=1).await;
        Mock::given(method("POST")).and(path("/api/chat.postMessage")).respond_with(slack_error(code)).mount(&w.server).await;
        assert_eq!(w.notifier.post_reply(w.org, &thread(), &post).await, Err(expected), "slack error {code}");
    }
}

#[tokio::test]
async fn http_failures_map_to_rate_limited_and_unavailable() {
    let w = world_with(|c| c.request_timeout_ms = 300).await;
    mount_auth_test(&w.server, TOKEN, 1).await;
    Mock::given(method("GET"))
        .and(path("/api/conversations.replies"))
        .and(query_param("channel", "C429"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "30").set_body_json(json!({ "ok": false, "error": "ratelimited" })))
        .mount(&w.server)
        .await;
    Mock::given(method("GET")).and(path("/api/conversations.replies")).and(query_param("channel", "C500")).respond_with(ResponseTemplate::new(503)).mount(&w.server).await;
    Mock::given(method("GET"))
        .and(path("/api/conversations.replies"))
        .and(query_param("channel", "CSLOW"))
        .respond_with(page(vec![], None).set_delay(Duration::from_secs(2)))
        .mount(&w.server)
        .await;
    let in_channel = |channel: &str| SlackThreadRef { channel: channel.into(), ..thread() };

    assert_eq!(
        w.notifier.read_thread(w.org, &in_channel("C429"), 10).await,
        Err(ChatError::RateLimited { retry_at: w.clock.now() + Duration::from_secs(30) })
    );
    assert!(matches!(w.notifier.read_thread(w.org, &in_channel("C500"), 10).await, Err(ChatError::Unavailable(_))));
    assert!(matches!(w.notifier.read_thread(w.org, &in_channel("CSLOW"), 10).await, Err(ChatError::Unavailable(_))));
}

#[tokio::test]
async fn foreign_or_revoked_team_is_unauthorized_without_calling_slack() {
    let w = world().await;
    mount_auth_test(&w.server, TOKEN, 0).await;
    let other_team = SlackThreadRef { team: "TOTHER".into(), ..thread() };
    assert_eq!(w.notifier.read_thread(w.org, &other_team, 10).await, Err(ChatError::Unauthorized));

    w.orgs.set_integration_status(w.org, IntegrationKind::Slack, TEAM, IntegrationStatus::Revoked, &[]).await.unwrap();
    assert_eq!(w.notifier.read_thread(w.org, &thread(), 10).await, Err(ChatError::Unauthorized));

    let other_org = OrgId::from_uuid(Uuid::from_u128(8));
    assert_eq!(w.notifier.read_thread(other_org, &thread(), 10).await, Err(ChatError::Unauthorized));
}

#[tokio::test]
async fn bot_token_is_cached_for_five_minutes_then_reloaded() {
    let w = world().await;
    const ROTATED: &str = "xoxb-rotated";
    mount_auth_test(&w.server, TOKEN, 1).await;
    mount_auth_test(&w.server, ROTATED, 1).await;
    Mock::given(method("GET"))
        .and(path("/api/conversations.replies"))
        .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
        .respond_with(page(vec![], None))
        .expect(2)
        .mount(&w.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/conversations.replies"))
        .and(header("authorization", format!("Bearer {ROTATED}").as_str()))
        .respond_with(page(vec![], None))
        .expect(1)
        .mount(&w.server)
        .await;

    w.notifier.read_thread(w.org, &thread(), 10).await.unwrap();
    put_secret(&w.vault, SecretScope::Org(w.org), &secret_names::slack_bot_token(TEAM), ROTATED).await;
    w.clock.advance(Duration::from_secs(4 * 60));
    w.notifier.read_thread(w.org, &thread(), 10).await.unwrap();
    w.clock.advance(Duration::from_secs(2 * 60));
    w.notifier.read_thread(w.org, &thread(), 10).await.unwrap();
}

#[tokio::test]
async fn complete_install_exchanges_the_code_for_a_grant() {
    let w = world_with(|c| c.oauth_client_id = Some("123.456".into())).await;
    put_secret(&w.vault, SecretScope::Platform, secret_names::SLACK_OAUTH_CLIENT_SECRET, "client-secret").await;
    Mock::given(method("POST"))
        .and(path("/api/oauth.v2.access"))
        .and(body_string_contains("client_id=123.456"))
        .and(body_string_contains("client_secret=client-secret"))
        .and(body_string_contains("code=the-code"))
        .and(body_string_contains("redirect_uri=https%3A%2F%2Fmage.example%2Fintegrations%2Fslack%2Fcallback"))
        .respond_with(ok(json!({
            "access_token": "xoxb-new", "token_type": "bot", "scope": "channels:history,groups:history,chat:write",
            "bot_user_id": "UNEWBOT", "app_id": "A1", "team": { "id": "TNEW", "name": "Acme" }, "enterprise": null,
            "authed_user": { "id": "U1" }
        })))
        .expect(1)
        .mount(&w.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/auth.test"))
        .and(header("authorization", "Bearer xoxb-new"))
        .respond_with(ok(json!({ "url": "https://acme-corp.slack.com/", "team_id": "TNEW", "user_id": "UNEWBOT", "bot_id": "B9" })))
        .expect(1)
        .mount(&w.server)
        .await;

    let redirect = Url::parse("https://mage.example/integrations/slack/callback").unwrap();
    let grant = w.notifier.complete_install("the-code", &redirect).await.unwrap();
    assert_eq!(grant.team_id, "TNEW");
    assert_eq!(grant.team_domain, "acme-corp");
    assert_eq!(grant.bot_user_id, "UNEWBOT");
    assert_eq!(grant.bot_token.expose(), "xoxb-new");
    assert_eq!(grant.scopes, vec!["channels:history", "groups:history", "chat:write"]);
}

#[tokio::test]
async fn complete_install_errors() {
    let unconfigured = world().await;
    let redirect = Url::parse("https://mage.example/cb").unwrap();
    assert!(matches!(unconfigured.notifier.complete_install("c", &redirect).await, Err(ChatError::Invalid(_))));

    let w = world_with(|c| c.oauth_client_id = Some("123.456".into())).await;
    put_secret(&w.vault, SecretScope::Platform, secret_names::SLACK_OAUTH_CLIENT_SECRET, "client-secret").await;
    Mock::given(method("POST")).and(path("/api/oauth.v2.access")).respond_with(slack_error("invalid_code")).mount(&w.server).await;
    assert_eq!(w.notifier.complete_install("stale", &redirect).await, Err(ChatError::Invalid("invalid_code".into())));
}
