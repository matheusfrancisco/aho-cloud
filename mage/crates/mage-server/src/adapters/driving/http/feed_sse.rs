//! Org feed SSE, `GET /events/orgs/{org}/feed` (server.md "Live updates (SSE)").
//!
//! 1. Session + `OrgCtx` (Member) come from the shell's middleware.
//! 2. Resume point: `Last-Event-ID` (EventSource reconnect), else `?after=`, else the head.
//! 3. Subscribe first (`ChangeFeed::subscribe`), then replay from the store in pages of 500.
//! 4. Each notice fetches entries `> last_sent` (ids are monotonic, so nothing repeats);
//!    `Lagged` or a lost listener resyncs from the store; a 5 s poll covers lost notices.
//! 5. `retry: 2000` first, `: ping` every 15 s, session re-checked every 60 s (gone → close).
//! 6. `reconnect` event after 10 min and immediately on shutdown.
//!
//! Event `feed`, `id` = feed id, data = `FeedEvent {kind, subject}` (ids only; the UI refetches
//! through `/api/v1`, so RBAC applies to all data).

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures::{Stream, StreamExt};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use mage_core::domain::OrgId;
use mage_core::wire::api::{FeedEvent, routes, sse};

use crate::adapters::driving::http::auth::{Authed, OrgCtx};
use crate::adapters::driving::http::error::QueryParams;
use crate::app::auth::{SessionId, Sessions};
use crate::error::AppError;
use crate::ports::feed::{ChangeFeed, FeedEntry, FeedId, FeedNotice};

/// Timings of one stream (server.md defaults).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SseTimings {
    /// Close with `reconnect` after this long (well below Cloud Run's request timeout).
    pub max_age: Duration,
    pub ping: Duration,
    pub session_recheck: Duration,
    /// Fallback poll for lost notifications.
    pub poll: Duration,
    pub page: u32,
    /// `retry:` hint sent first.
    pub retry: Duration,
}

impl Default for SseTimings {
    fn default() -> Self {
        SseTimings {
            max_age: Duration::from_secs(600),
            ping: Duration::from_secs(15),
            session_recheck: Duration::from_secs(60),
            poll: Duration::from_secs(5),
            page: 500,
            retry: Duration::from_millis(2000),
        }
    }
}

#[derive(Clone)]
pub struct FeedState {
    pub feed: Arc<dyn ChangeFeed>,
    pub sessions: Arc<Sessions>,
    /// Cancelled on SIGTERM: every stream sends `reconnect` and ends.
    pub shutdown: CancellationToken,
    pub timings: SseTimings,
}

pub fn router(state: FeedState) -> Router {
    Router::new().route(routes::EVENTS_FEED, get(feed)).with_state(state)
}

#[derive(Debug, Deserialize)]
struct AfterQuery {
    after: Option<i64>,
}

async fn feed(State(state): State<FeedState>, ctx: OrgCtx, authed: Authed, headers: HeaderMap, QueryParams(q): QueryParams<AfterQuery>) -> Response {
    let last_event_id = headers.get("last-event-id").and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<i64>().ok());
    let start = match last_event_id.or(q.after) {
        Some(id) if id >= 0 => FeedId(id),
        Some(_) => return AppError::Invalid("feed ids are non-negative".into()).into_response(),
        None => match state.feed.head(ctx.org_id).await {
            Ok(head) => head,
            Err(e) => return AppError::from(e).into_response(),
        },
    };
    Sse::new(stream(state, ctx.org_id, authed.session, start)).into_response()
}

fn feed_event(entry: &FeedEntry) -> Event {
    let data = FeedEvent { kind: entry.kind, subject: entry.subject.clone() };
    let event = Event::default().event(sse::FEED).id(entry.id.0.to_string());
    match event.json_data(&data) {
        Ok(event) => event,
        // `FeedEvent` always serializes; keep the stream alive with an id-only event otherwise.
        Err(_) => Event::default().event(sse::FEED).id(entry.id.0.to_string()),
    }
}

/// Every entry after `last`, in pages.
async fn entries_after(feed: &dyn ChangeFeed, org: OrgId, last: FeedId, page: u32) -> Result<Vec<FeedEntry>, AppError> {
    let mut out = Vec::new();
    let mut cursor = last;
    loop {
        let batch = feed.after(org, cursor, page).await?;
        let full = u32::try_from(batch.len()).unwrap_or(u32::MAX) >= page;
        if let Some(e) = batch.last() {
            cursor = e.id;
        }
        out.extend(batch);
        if !full {
            return Ok(out);
        }
    }
}

fn stream(state: FeedState, org: OrgId, session: SessionId, start: FeedId) -> impl Stream<Item = Result<Event, Infallible>> {
    async_stream::stream! {
        let FeedState { feed, sessions, shutdown, timings } = state;
        yield Ok(Event::default().retry(timings.retry));

        // Subscribe before replaying, so nothing committed in between is missed.
        let mut notices = match feed.subscribe(org).await {
            Ok(n) => Some(n),
            Err(e) => {
                tracing::warn!(error = %e, "org feed subscribe failed; polling");
                None
            }
        };
        let mut last = start;
        let mut resync = true;

        let deadline = tokio::time::sleep(timings.max_age);
        tokio::pin!(deadline);
        let now = tokio::time::Instant::now();
        let mut ping = tokio::time::interval_at(now + timings.ping, timings.ping);
        let mut recheck = tokio::time::interval_at(now + timings.session_recheck, timings.session_recheck);
        let mut poll = tokio::time::interval_at(now + timings.poll, timings.poll);

        loop {
            if resync {
                resync = false;
                match entries_after(feed.as_ref(), org, last, timings.page).await {
                    Ok(entries) => {
                        for entry in &entries {
                            if entry.id <= last {
                                continue;
                            }
                            last = entry.id;
                            yield Ok(feed_event(entry));
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "org feed read failed; retrying on the next poll"),
                }
            }
            tokio::select! {
                biased;
                () = shutdown.cancelled() => {
                    yield Ok(Event::default().event(sse::RECONNECT).data(""));
                    break;
                }
                () = &mut deadline => {
                    yield Ok(Event::default().event(sse::RECONNECT).data(""));
                    break;
                }
                _ = recheck.tick() => {
                    match sessions.is_active(session).await {
                        Ok(true) => {}
                        Ok(false) => break,
                        Err(e) => tracing::warn!(error = %e, "session re-check failed; keeping the stream"),
                    }
                }
                _ = ping.tick() => yield Ok(Event::default().comment("ping")),
                _ = poll.tick() => resync = true,
                notice = next_notice(&mut notices) => match notice {
                    Some(FeedNotice::Entry { id, .. }) => resync = id > last,
                    Some(FeedNotice::Lagged) => resync = true,
                    None => {
                        tracing::warn!("org feed listener ended; polling");
                        notices = None;
                        resync = true;
                    }
                },
            }
        }
    }
}

/// Next notice, or pending forever when there is no subscription (poll only).
async fn next_notice(notices: &mut Option<crate::ports::feed::NoticeStream>) -> Option<FeedNotice> {
    match notices {
        Some(stream) => stream.next().await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use mage_core::domain::{MemberRole, UserId};
    use mage_core::testing::FakeClock;
    use tower::ServiceExt;

    use super::*;
    use crate::app::auth::SessionPolicy;
    use crate::ports::feed::FeedKind;
    use crate::testing::{MemChangeFeed, MemSecretVault, MemTokenStore};

    struct Fixture {
        feed: Arc<MemChangeFeed>,
        org: OrgId,
        router: Router,
        shutdown: CancellationToken,
    }

    fn fixture(timings: SseTimings) -> Fixture {
        let clock = Arc::new(FakeClock::new());
        let feed = Arc::new(MemChangeFeed::new(clock.clone()));
        let sessions = Arc::new(Sessions::new(
            Arc::new(MemTokenStore::new(clock.clone())),
            Arc::new(MemSecretVault::new(clock.clone())),
            clock,
            SessionPolicy::default(),
        ));
        let org = OrgId::from(uuid::Uuid::from_u128(1));
        let user = UserId::from(uuid::Uuid::from_u128(2));
        let shutdown = CancellationToken::new();
        let router = router(FeedState { feed: feed.clone(), sessions, shutdown: shutdown.clone(), timings })
            .layer(axum::Extension(OrgCtx { org_id: org, user_id: user, role: MemberRole::Member }))
            .layer(axum::Extension(Authed { user_id: user, session: SessionId([3; 32]) }));
        Fixture { feed, org, router, shutdown }
    }

    async fn append(f: &Fixture, n: usize) {
        for i in 0..n {
            f.feed.append(f.org, FeedKind::Ticket, &format!("tkt_{i}")).await.expect("append");
        }
    }

    /// Reads SSE frames until `until` matches one, returning (event, id) of every frame seen.
    async fn read_until(body: &mut axum::body::BodyDataStream, buf: &mut String, until: impl Fn(&(String, String)) -> bool) -> Vec<(String, String)> {
        let mut frames = Vec::new();
        loop {
            while let Some(end) = buf.find("\n\n") {
                let frame: String = buf.drain(..end + 2).collect();
                let field = |name: &str| frame.lines().find_map(|l| l.strip_prefix(name)).map(|v| v.trim().to_owned()).unwrap_or_default();
                let parsed = (field("event:"), field("id:"));
                let done = until(&parsed);
                frames.push(parsed);
                if done {
                    return frames;
                }
            }
            let chunk = tokio::time::timeout(Duration::from_secs(5), body.next()).await.expect("frame in time").expect("open").expect("chunk");
            buf.push_str(std::str::from_utf8(&chunk).expect("utf8"));
        }
    }

    fn feed_ids(frames: &[(String, String)]) -> Vec<String> {
        frames.iter().filter(|(e, _)| e == sse::FEED).map(|(_, id)| id.clone()).collect()
    }

    #[tokio::test]
    async fn resumes_after_last_event_id_without_gaps_or_duplicates() {
        let f = fixture(SseTimings { poll: Duration::from_secs(60), ..SseTimings::default() });
        append(&f, 4).await;
        let request = Request::get("/events/orgs/hoop/feed").header("last-event-id", "2").body(Body::empty()).expect("request");
        let response = f.router.clone().oneshot(request).await.expect("response");
        assert_eq!(response.headers()["content-type"], "text/event-stream");
        let mut body = response.into_body().into_data_stream();
        let mut buf = String::new();
        let frames = read_until(&mut body, &mut buf, |(_, id)| id == "4").await;
        assert_eq!(feed_ids(&frames), ["3", "4"]);

        // Live entries arrive through the subscription (poll is 60 s here).
        append(&f, 2).await;
        let frames = read_until(&mut body, &mut buf, |(_, id)| id == "6").await;
        assert_eq!(feed_ids(&frames), ["5", "6"]);
    }

    #[tokio::test]
    async fn first_connect_without_an_id_starts_at_the_head() {
        let f = fixture(SseTimings::default());
        append(&f, 3).await;
        let response = f.router.clone().oneshot(Request::get("/events/orgs/hoop/feed").body(Body::empty()).expect("request")).await.expect("response");
        let mut body = response.into_body().into_data_stream();
        let mut buf = String::new();
        append(&f, 1).await;
        let frames = read_until(&mut body, &mut buf, |(e, _)| e == sse::FEED).await;
        assert_eq!(feed_ids(&frames), ["4"]);
    }

    #[tokio::test]
    async fn streams_end_with_reconnect_at_the_deadline_and_on_shutdown() {
        let f = fixture(SseTimings { max_age: Duration::from_millis(100), ..SseTimings::default() });
        let response = f.router.clone().oneshot(Request::get("/events/orgs/hoop/feed?after=0").body(Body::empty()).expect("request")).await.expect("response");
        let mut body = response.into_body().into_data_stream();
        let mut buf = String::new();
        let frames = read_until(&mut body, &mut buf, |(e, _)| e == sse::RECONNECT).await;
        assert_eq!(frames.last().map(|(e, _)| e.as_str()), Some(sse::RECONNECT));
        assert!(tokio::time::timeout(Duration::from_secs(1), body.next()).await.expect("ends").is_none());

        let f = fixture(SseTimings::default());
        let response = f.router.clone().oneshot(Request::get("/events/orgs/hoop/feed?after=0").body(Body::empty()).expect("request")).await.expect("response");
        let mut body = response.into_body().into_data_stream();
        let mut buf = String::new();
        f.shutdown.cancel();
        let frames = read_until(&mut body, &mut buf, |(e, _)| e == sse::RECONNECT).await;
        assert_eq!(frames.last().map(|(e, _)| e.as_str()), Some(sse::RECONNECT));
    }
}
