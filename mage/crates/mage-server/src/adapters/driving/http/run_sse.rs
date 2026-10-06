//! `GET /events/orgs/{org}/runs/{run}` (server.md "Live updates", runs.md "StreamEvents"):
//! `retry: 2000` first, then `event: run` / `id: seq` / `data: RunEvent`, `: ping` keep-alives,
//! `event: end` when the run is done, `event: reconnect` (with `retry: 1000`) after `sse_max` or
//! on shutdown. The cursor is `Last-Event-ID`, else `?after=`, else 0.

use std::collections::HashMap;
use std::convert::Infallible;
use std::time::Duration;

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::sse::{Event, Sse};
use axum::routing::get;
use futures::{Stream, StreamExt};

use mage_core::domain::RunId;
use mage_core::wire::api::{routes, sse};

use crate::adapters::driving::http::auth::OrgCtx;
use crate::app::runs::{RunManager, StreamItem};
use crate::error::AppError;

#[derive(Clone)]
pub struct RunSseState {
    runs: RunManager,
}

impl RunSseState {
    pub fn new(runs: RunManager) -> RunSseState {
        RunSseState { runs }
    }
}

pub fn router(state: RunSseState) -> Router {
    Router::new().route(routes::EVENTS_RUN, get(stream)).with_state(state)
}

/// `Last-Event-ID` wins over `?after=`.
pub fn cursor(headers: &HeaderMap, query: &HashMap<String, String>) -> u64 {
    headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse().ok())
        .or_else(|| query.get("after").and_then(|v| v.parse().ok()))
        .unwrap_or(0)
}

fn to_event(item: Result<StreamItem, AppError>) -> Event {
    match item {
        Ok(StreamItem::Event(e)) => match serde_json::to_string(&e) {
            Ok(json) => Event::default().event(sse::RUN).id(e.seq.to_string()).data(json),
            Err(err) => Event::default().comment(format!("encode error: {err}")),
        },
        Ok(StreamItem::Ping) => Event::default().comment("ping"),
        Ok(StreamItem::End) => Event::default().event(sse::END).data(""),
        Ok(StreamItem::Reconnect) => Event::default().event(sse::RECONNECT).retry(Duration::from_millis(1000)).data(""),
        Err(err) => {
            tracing::warn!(error = %err, "run stream failed");
            Event::default().event(sse::RECONNECT).retry(Duration::from_millis(2000)).data("")
        }
    }
}

async fn stream(
    State(s): State<RunSseState>,
    ctx: OrgCtx,
    Path(p): Path<HashMap<String, String>>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, AppError> {
    let run: RunId = p.get("run").and_then(|v| v.parse().ok()).ok_or_else(|| AppError::NotFound("run".into()))?;
    // 404 before the stream starts for another org's or an unknown run.
    s.runs.store().get(ctx.org_id, run).await?;
    let after = cursor(&headers, &q);
    let start = futures::stream::once(async { Event::default().retry(Duration::from_millis(2000)).comment("stream") });
    let body = s.runs.event_stream(ctx.org_id, run, after).map(to_event);
    Ok(Sse::new(start.chain(body).map(Ok)))
}
