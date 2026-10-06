//! [`SlackNotifier`]: `ChatNotifier` over the Slack Web API (intake.md "`ChatNotifier`",
//! "`mage_server::adapters::driven::slack`").

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mage_core::domain::{IntegrationKind, IntegrationStatus, OrgId, SlackThreadRef};
use mage_core::ports::Clock;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use url::Url;

use super::client::SlackApi;
use super::{SlackConfig, bot_token_secret_name, escape_text, render_link};
use crate::domain::intake::secret_names;
use crate::ports::chat::{AccessToken, ChatAuthor, ChatError, ChatGrant, ChatMessageView, ChatNotifier, ChatPost, ChatThread, PostedChat};
use crate::ports::org::{OrgStore, OrgStoreError};
use crate::ports::vault::{SecretName, SecretScope, SecretVault, VaultError, constant_time_eq};

/// `metadata.event_type` of every reply Mage posts.
pub(crate) const MAGE_EVENT_TYPE: &str = "mage_notice";

/// Prune the per-channel pacing map once it holds this many idle channels.
const PACING_PRUNE_AT: usize = 256;

/// A loaded bot token and who it acts as (`auth.test`, once per token value).
#[derive(Clone)]
struct BotSession {
    token: AccessToken,
    user_id: String,
    bot_id: Option<String>,
    loaded_at: OffsetDateTime,
}

impl BotSession {
    fn is_ours(&self, msg: &RawMessage) -> bool {
        msg.user.as_deref() == Some(self.user_id.as_str())
            || matches!((&msg.bot_id, &self.bot_id), (Some(theirs), Some(ours)) if theirs == ours)
    }
}

type PacingSlot = Arc<tokio::sync::Mutex<Option<tokio::time::Instant>>>;

pub struct SlackNotifier {
    api: SlackApi,
    config: SlackConfig,
    vault: Arc<dyn SecretVault>,
    orgs: Arc<dyn OrgStore>,
    clock: Arc<dyn Clock>,
    sessions: Mutex<HashMap<(OrgId, String), BotSession>>,
    /// `(team, channel)` → time of the last `chat.postMessage`; the async lock serializes posts
    /// per channel.
    pacing: Mutex<HashMap<(String, String), PacingSlot>>,
}

impl SlackNotifier {
    /// `Invalid` when `config.api_base` is not an http(s) base URL.
    pub fn new(config: SlackConfig, vault: Arc<dyn SecretVault>, orgs: Arc<dyn OrgStore>, clock: Arc<dyn Clock>) -> Result<SlackNotifier, ChatError> {
        let api = SlackApi::new(&config, clock.clone())?;
        Ok(SlackNotifier { api, config, vault, orgs, clock, sessions: Mutex::new(HashMap::new()), pacing: Mutex::new(HashMap::new()) })
    }

    /// `team` must be an `Active` Slack integration of `org`; anything else is `Unauthorized`.
    async fn check_tenant(&self, org: OrgId, team: &str) -> Result<(), ChatError> {
        let integrations = self.orgs.integrations(org).await.map_err(|e| match e {
            OrgStoreError::NotFound => ChatError::Unauthorized,
            other => ChatError::Unavailable(format!("org store: {other}")),
        })?;
        let bound = integrations
            .iter()
            .any(|i| i.kind == IntegrationKind::Slack && i.external_id == team && i.status == IntegrationStatus::Active);
        if bound { Ok(()) } else { Err(ChatError::Unauthorized) }
    }

    /// Tenant check, then the team's bot token (SecretVault, cached `token_cache_secs`).
    async fn session(&self, org: OrgId, team: &str) -> Result<BotSession, ChatError> {
        self.check_tenant(org, team).await?;
        let now = self.clock.now();
        let key = (org, team.to_owned());
        let cached = self.sessions.lock().get(&key).cloned();
        let ttl = Duration::from_secs(self.config.token_cache_secs);
        if let Some(session) = cached.as_ref().filter(|s| now < s.loaded_at + ttl) {
            return Ok(session.clone());
        }
        let name = bot_token_secret_name(team).map_err(vault_error)?;
        let (_, bytes) = self.vault.load(&SecretScope::Org(org), &name).await.map_err(vault_error)?;
        let token = bytes.expose_str().map(str::trim).ok_or_else(|| ChatError::Invalid("slack bot token is not UTF-8".into()))?;
        let session = match cached {
            Some(prev) if constant_time_eq(prev.token.expose().as_bytes(), token.as_bytes()) => BotSession { loaded_at: now, ..prev },
            _ => {
                let who: AuthTest = self.api.get("auth.test", token, &[]).await?;
                BotSession { token: AccessToken::new(token), user_id: who.user_id, bot_id: who.bot_id, loaded_at: now }
            }
        };
        self.sessions.lock().insert(key, session.clone());
        Ok(session)
    }

    /// A rejected token is dropped from the cache so a rotated one is picked up on the next call.
    fn forget_on_unauthorized<T>(&self, org: OrgId, team: &str, result: Result<T, ChatError>) -> Result<T, ChatError> {
        if matches!(result, Err(ChatError::Unauthorized)) {
            self.sessions.lock().remove(&(org, team.to_owned()));
        }
        result
    }

    async fn replies_page(
        &self,
        session: &BotSession,
        thread: &SlackThreadRef,
        cursor: Option<&str>,
        oldest: Option<&str>,
    ) -> Result<RepliesPage, ChatError> {
        let mut query: Vec<(&str, String)> = vec![
            ("channel", thread.channel.clone()),
            ("ts", thread.ts.clone()),
            ("limit", self.config.replies_page_limit.max(1).to_string()),
            ("include_all_metadata", "true".into()),
        ];
        if let Some(cursor) = cursor {
            query.push(("cursor", cursor.to_owned()));
        }
        if let Some(oldest) = oldest {
            query.push(("oldest", oldest.to_owned()));
            query.push(("inclusive", "true".into()));
        }
        self.api.get("conversations.replies", session.token.expose(), &query).await
    }

    fn pacing_slot(&self, team: &str, channel: &str) -> PacingSlot {
        let mut map = self.pacing.lock();
        if map.len() >= PACING_PRUNE_AT {
            let interval = Duration::from_millis(self.config.post_interval_ms);
            map.retain(|_, slot| {
                Arc::strong_count(slot) > 1 || slot.try_lock().map_or(true, |last| last.is_some_and(|at| at.elapsed() < interval))
            });
        }
        map.entry((team.to_owned(), channel.to_owned())).or_default().clone()
    }

    async fn read_thread_inner(&self, session: &BotSession, thread: &SlackThreadRef, max_messages: u16) -> Result<ChatThread, ChatError> {
        let max = usize::from(max_messages);
        let mut messages = Vec::new();
        let mut seen = HashSet::new();
        let mut cursor: Option<String> = None;
        let mut truncated = false;
        loop {
            let page = self.replies_page(session, thread, cursor.as_deref(), None).await?;
            let next = page.next_cursor();
            for raw in page.messages {
                // Slack repeats the thread root on later pages.
                if !seen.insert(raw.ts.clone()) {
                    continue;
                }
                if messages.len() == max {
                    truncated = true;
                    break;
                }
                messages.push(view(raw, session)?);
            }
            if truncated {
                break;
            }
            match next {
                Some(_) if messages.len() == max => {
                    truncated = true;
                    break;
                }
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        Ok(ChatThread { thread: thread.clone(), messages, truncated })
    }

    async fn post_inner(&self, session: &BotSession, thread: &SlackThreadRef, post: &ChatPost) -> Result<PostedChat, ChatError> {
        let mut text = escape_text(&post.text);
        if let Some((url, label)) = &post.link {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&render_link(url, label));
        }
        let body = PostMessage {
            channel: &thread.channel,
            thread_ts: &thread.ts,
            text: &text,
            unfurl_links: false,
            unfurl_media: false,
            metadata: Metadata { event_type: MAGE_EVENT_TYPE, event_payload: MetadataPayload { key: &post.key } },
        };
        let slot = self.pacing_slot(&thread.team, &thread.channel);
        let mut last = slot.lock().await;
        if let Some(at) = *last {
            tokio::time::sleep_until(at + Duration::from_millis(self.config.post_interval_ms)).await;
        }
        let result: Result<Posted, ChatError> = self.api.post_json("chat.postMessage", session.token.expose(), &body).await;
        *last = Some(tokio::time::Instant::now());
        Ok(PostedChat { ts: result?.ts })
    }

    async fn find_inner(&self, session: &BotSession, thread: &SlackThreadRef, key: &str, since: OffsetDateTime) -> Result<Option<PostedChat>, ChatError> {
        let oldest = format_ts(since);
        let mut cursor: Option<String> = None;
        loop {
            let page = self.replies_page(session, thread, cursor.as_deref(), Some(&oldest)).await?;
            let next = page.next_cursor();
            for raw in page.messages {
                if !session.is_ours(&raw) || mage_key(&raw).as_deref() != Some(key) {
                    continue;
                }
                if parse_ts(&raw.ts)? >= since {
                    return Ok(Some(PostedChat { ts: raw.ts }));
                }
            }
            match next {
                Some(next) => cursor = Some(next),
                None => return Ok(None),
            }
        }
    }
}

#[async_trait]
impl ChatNotifier for SlackNotifier {
    async fn read_thread(&self, org: OrgId, thread: &SlackThreadRef, max_messages: u16) -> Result<ChatThread, ChatError> {
        let session = self.session(org, &thread.team).await?;
        let result = self.read_thread_inner(&session, thread, max_messages).await;
        self.forget_on_unauthorized(org, &thread.team, result)
    }

    async fn post_reply(&self, org: OrgId, thread: &SlackThreadRef, post: &ChatPost) -> Result<PostedChat, ChatError> {
        let session = self.session(org, &thread.team).await?;
        let result = self.post_inner(&session, thread, post).await;
        self.forget_on_unauthorized(org, &thread.team, result)
    }

    async fn find_reply(&self, org: OrgId, thread: &SlackThreadRef, key: &str, since: OffsetDateTime) -> Result<Option<PostedChat>, ChatError> {
        let session = self.session(org, &thread.team).await?;
        let result = self.find_inner(&session, thread, key, since).await;
        self.forget_on_unauthorized(org, &thread.team, result)
    }

    async fn complete_install(&self, code: &str, redirect_uri: &Url) -> Result<ChatGrant, ChatError> {
        let client_id = self.config.oauth_client_id.as_deref().ok_or_else(|| ChatError::Invalid("slack oauth_client_id is not configured".into()))?;
        let name = SecretName::new(secret_names::SLACK_OAUTH_CLIENT_SECRET).map_err(vault_error)?;
        let (_, secret) = self.vault.load(&SecretScope::Platform, &name).await.map_err(|e| match e {
            VaultError::NotFound => ChatError::Invalid(format!("{} is not set", secret_names::SLACK_OAUTH_CLIENT_SECRET)),
            other => vault_error(other),
        })?;
        let secret = secret.expose_str().ok_or_else(|| ChatError::Invalid("slack oauth client secret is not UTF-8".into()))?;
        let form = [("client_id", client_id), ("client_secret", secret), ("code", code), ("redirect_uri", redirect_uri.as_str())];
        let grant: OAuthAccess = self.api.post_form("oauth.v2.access", &form).await?;
        if grant.token_type != "bot" {
            return Err(ChatError::Invalid(format!("slack grant has token_type {:?}, expected \"bot\"", grant.token_type)));
        }
        let team = grant.team.ok_or_else(|| ChatError::Invalid("slack org-wide (enterprise) installs are not supported".into()))?;
        let team_domain = match team.domain {
            Some(domain) => domain,
            // `oauth.v2.access` carries no domain; `auth.test` (no extra scope) returns the
            // workspace URL `https://{domain}.slack.com/`.
            None => {
                let who: AuthTest = self.api.get("auth.test", &grant.access_token, &[]).await?;
                who.url.as_deref().and_then(domain_from_workspace_url).ok_or_else(|| {
                    ChatError::Unavailable(format!("slack auth.test returned no workspace url for team {}", team.id))
                })?
            }
        };
        Ok(ChatGrant {
            team_id: team.id,
            team_domain,
            bot_user_id: grant.bot_user_id,
            bot_token: AccessToken::new(grant.access_token),
            scopes: grant.scope.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).collect(),
        })
    }
}

fn vault_error(e: VaultError) -> ChatError {
    match e {
        // No token for an active integration: the connection is broken until an admin reconnects.
        VaultError::NotFound => ChatError::Unauthorized,
        VaultError::Invalid(msg) => ChatError::Invalid(msg),
        other => ChatError::Unavailable(format!("secret vault: {other}")),
    }
}

fn view(raw: RawMessage, session: &BotSession) -> Result<ChatMessageView, ChatError> {
    let at = parse_ts(&raw.ts)?;
    let ours = session.is_ours(&raw);
    // Other apps can attach metadata too; only our bot's key counts.
    let mage_key = if ours { mage_key(&raw) } else { None };
    let author = match (&raw.bot_id, &raw.user) {
        (Some(bot_id), _) => ChatAuthor::Bot { id: bot_id.clone(), ours },
        (None, _) if ours => ChatAuthor::Bot { id: session.user_id.clone(), ours },
        (None, user) => ChatAuthor::Human { user_id: user.clone().unwrap_or_default() },
    };
    let file_names = raw.files.into_iter().filter_map(|f| f.name.or(f.title)).collect();
    Ok(ChatMessageView { ts: raw.ts, author, text: raw.text, at, file_names, mage_key })
}

/// `metadata.event_payload.key` of a `mage_notice` message; callers check the author is ours.
fn mage_key(raw: &RawMessage) -> Option<String> {
    let meta = raw.metadata.as_ref()?;
    if meta.event_type != MAGE_EVENT_TYPE {
        return None;
    }
    meta.event_payload.get("key")?.as_str().map(str::to_owned)
}

/// `"1712345678.123456"` → instant.
pub(crate) fn parse_ts(ts: &str) -> Result<OffsetDateTime, ChatError> {
    let bad = || ChatError::Unavailable(format!("slack returned malformed ts {ts:?}"));
    let (secs, frac) = ts.split_once('.').unwrap_or((ts, ""));
    if frac.len() > 9 || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    let secs: i128 = secs.parse().map_err(|_| bad())?;
    let nanos: i128 = if frac.is_empty() { 0 } else { format!("{frac:0<9}").parse().map_err(|_| bad())? };
    OffsetDateTime::from_unix_timestamp_nanos(secs * 1_000_000_000 + nanos).map_err(|_| bad())
}

/// Instant → Slack ts string (microseconds).
pub(crate) fn format_ts(at: OffsetDateTime) -> String {
    format!("{}.{:06}", at.unix_timestamp(), at.microsecond())
}

/// `https://acme.slack.com/` → `acme`.
fn domain_from_workspace_url(url: &str) -> Option<String> {
    let url = Url::parse(url).ok()?;
    let domain = url.host_str()?.strip_suffix(".slack.com")?;
    (!domain.is_empty()).then(|| domain.to_owned())
}

#[derive(Deserialize)]
struct AuthTest {
    user_id: String,
    #[serde(default)]
    bot_id: Option<String>,
    #[serde(default)]
    url: Option<String>,
}

#[derive(Deserialize)]
struct RepliesPage {
    #[serde(default)]
    messages: Vec<RawMessage>,
    #[serde(default)]
    has_more: bool,
    #[serde(default)]
    response_metadata: Option<ResponseMetadata>,
}

impl RepliesPage {
    fn next_cursor(&self) -> Option<String> {
        let cursor = self.response_metadata.as_ref()?.next_cursor.as_str();
        (self.has_more && !cursor.is_empty()).then(|| cursor.to_owned())
    }
}

#[derive(Deserialize)]
struct ResponseMetadata {
    #[serde(default)]
    next_cursor: String,
}

#[derive(Deserialize)]
struct RawMessage {
    ts: String,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    bot_id: Option<String>,
    #[serde(default)]
    text: String,
    #[serde(default)]
    files: Vec<RawFile>,
    #[serde(default)]
    metadata: Option<RawMetadata>,
}

#[derive(Deserialize)]
struct RawFile {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    title: Option<String>,
}

#[derive(Deserialize)]
struct RawMetadata {
    event_type: String,
    #[serde(default)]
    event_payload: serde_json::Value,
}

#[derive(Serialize)]
struct PostMessage<'a> {
    channel: &'a str,
    thread_ts: &'a str,
    text: &'a str,
    unfurl_links: bool,
    unfurl_media: bool,
    metadata: Metadata<'a>,
}

#[derive(Serialize)]
struct Metadata<'a> {
    event_type: &'a str,
    event_payload: MetadataPayload<'a>,
}

#[derive(Serialize)]
struct MetadataPayload<'a> {
    key: &'a str,
}

#[derive(Deserialize)]
struct Posted {
    ts: String,
}

#[derive(Deserialize)]
struct OAuthAccess {
    access_token: String,
    token_type: String,
    #[serde(default)]
    scope: String,
    bot_user_id: String,
    #[serde(default)]
    team: Option<OAuthTeam>,
}

#[derive(Deserialize)]
struct OAuthTeam {
    id: String,
    #[serde(default)]
    domain: Option<String>,
}
