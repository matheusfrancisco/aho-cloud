//! `PgChangeFeed` (server.md "Org feed and audit"): reads `org_feed`, wakes subscribers from one
//! dedicated `LISTEN mage_org_feed` connection per instance, and [`append_in`], the in-transaction
//! writer every pg store uses so a state change and its feed row commit together.
//!
//! Feed ids come from the per-org counter row `org_feed_heads` (storage.md): the `UPDATE … RETURNING`
//! locks the row until commit, so feed writes of one org serialize and commit order equals id
//! order. The `org_feed_notify` trigger sends `NOTIFY mage_org_feed 'org_id:id'` at commit.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use mage_core::domain::OrgId;
use mage_core::ports::clock::Clock;
use mage_infra::pg::{DbError, PgPools, TxError, begin_org, retry_tx};
use sqlx::PgConnection;
use sqlx::postgres::PgListener;
use time::OffsetDateTime;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::ports::feed::{ChangeFeed, FEED_PAGE_MAX, FeedEntry, FeedError, FeedId, FeedKind, FeedNotice, NoticeStream};

/// NOTIFY channel written by the `org_feed_notify` trigger.
pub const CHANNEL: &str = "mage_org_feed";

/// Notices buffered per subscriber before it lags.
const CAPACITY: usize = 1024;
const BACKOFF_MIN: Duration = Duration::from_millis(100);
const BACKOFF_MAX: Duration = Duration::from_secs(10);

/// `org_feed.kind` value (the serde names).
pub const fn feed_kind_str(kind: FeedKind) -> &'static str {
    match kind {
        FeedKind::Ticket => "ticket",
        FeedKind::Cause => "cause",
        FeedKind::Run => "run",
        FeedKind::Repo => "repo",
        FeedKind::Recipe => "recipe",
        FeedKind::Integration => "integration",
        FeedKind::Settings => "settings",
        FeedKind::Member => "member",
    }
}

pub fn parse_feed_kind(s: &str) -> Option<FeedKind> {
    [
        FeedKind::Ticket,
        FeedKind::Cause,
        FeedKind::Run,
        FeedKind::Repo,
        FeedKind::Recipe,
        FeedKind::Integration,
        FeedKind::Settings,
        FeedKind::Member,
    ]
    .into_iter()
    .find(|k| feed_kind_str(*k) == s)
}

/// Appends a feed entry inside the caller's transaction (tenant transactions must be scoped to
/// `org`). The per-org counter row stays locked until the caller commits.
pub async fn append_in(conn: &mut PgConnection, org: OrgId, kind: FeedKind, subject: &str, at: OffsetDateTime) -> Result<FeedId, DbError> {
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO org_feed_heads (org_id, next_id) VALUES ($1, 1) \
         ON CONFLICT (org_id) DO UPDATE SET next_id = org_feed_heads.next_id + 1 \
         RETURNING next_id",
    )
    .bind(org.as_uuid())
    .fetch_one(&mut *conn)
    .await?;
    sqlx::query("INSERT INTO org_feed (org_id, id, kind, subject, at) VALUES ($1, $2, $3, $4, $5)")
        .bind(org.as_uuid())
        .bind(id)
        .bind(feed_kind_str(kind))
        .bind(subject)
        .bind(at)
        .execute(&mut *conn)
        .await?;
    Ok(FeedId(id))
}

impl From<DbError> for FeedError {
    fn from(err: DbError) -> Self {
        match err {
            DbError::NotFound => FeedError::NotFound,
            DbError::Conflict { .. } => FeedError::Conflict,
            DbError::Invalid(msg) => FeedError::Invalid(msg),
            DbError::Unavailable(msg) | DbError::Retry(msg) => FeedError::Unavailable(msg),
            DbError::Timeout => FeedError::Unavailable("statement timeout".into()),
        }
    }
}

/// Cheap to clone; clones share the listener state and the subscriber channel.
#[derive(Clone)]
pub struct PgChangeFeed {
    inner: Arc<Inner>,
}

struct Inner {
    pools: PgPools,
    clock: Arc<dyn Clock>,
    notices: broadcast::Sender<FeedNotice>,
    listening: AtomicBool,
}

impl PgChangeFeed {
    /// Reads and appends work right away; subscribers get notices once [`Self::listen`] runs.
    pub fn new(pools: PgPools, clock: Arc<dyn Clock>) -> Self {
        let (notices, _) = broadcast::channel(CAPACITY);
        Self { inner: Arc::new(Inner { pools, clock, notices, listening: AtomicBool::new(false) }) }
    }

    /// Whether the LISTEN connection is up (`/ready`).
    pub fn listening(&self) -> bool {
        self.inner.listening.load(Ordering::SeqCst)
    }

    /// The LISTEN loop, until `shutdown`: one dedicated connection (not a pooled one) built from the
    /// platform pool's options. A lost connection sends `Lagged` to every subscriber and
    /// reconnects with exponential backoff (100 ms … 10 s).
    pub async fn listen(&self, shutdown: CancellationToken) {
        let mut backoff = BACKOFF_MIN;
        while !shutdown.is_cancelled() {
            match self.connect().await {
                Ok(mut listener) => {
                    backoff = BACKOFF_MIN;
                    self.inner.listening.store(true, Ordering::SeqCst);
                    let lost = self.pump(&mut listener, &shutdown).await;
                    self.inner.listening.store(false, Ordering::SeqCst);
                    if !lost {
                        return;
                    }
                    let _ = self.inner.notices.send(FeedNotice::Lagged);
                }
                Err(err) => tracing::warn!(error = %err, "org feed listener could not connect"),
            }
            tokio::select! {
                () = shutdown.cancelled() => return,
                () = tokio::time::sleep(backoff) => {}
            }
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
    }

    async fn connect(&self) -> Result<PgListener, sqlx::Error> {
        let mut listener = PgListener::connect_with(&self.inner.pools.platform).await?;
        listener.ignore_pool_close_event(true);
        // A lost connection is rebuilt by `listen` (with a `Lagged`), not silently by sqlx.
        listener.eager_reconnect(false);
        listener.listen(CHANNEL).await?;
        Ok(listener)
    }

    /// Forwards notifications until shutdown (`false`) or connection loss (`true`).
    async fn pump(&self, listener: &mut PgListener, shutdown: &CancellationToken) -> bool {
        loop {
            let next = tokio::select! {
                () = shutdown.cancelled() => return false,
                next = listener.try_recv() => next,
            };
            match next {
                Ok(Some(notification)) => match parse_payload(notification.payload()) {
                    Some((org, id)) => {
                        let _ = self.inner.notices.send(FeedNotice::Entry { org, id });
                    }
                    None => tracing::warn!(payload = notification.payload(), "malformed org feed notification"),
                },
                Ok(None) => {
                    tracing::warn!("org feed listener connection lost");
                    return true;
                }
                Err(err) => {
                    tracing::warn!(error = %err, "org feed listener failed");
                    return true;
                }
            }
        }
    }
}

/// `org_uuid:feed_id`.
fn parse_payload(payload: &str) -> Option<(OrgId, FeedId)> {
    let (org, id) = payload.split_once(':')?;
    Some((OrgId::from_uuid(Uuid::try_parse(org).ok()?), FeedId(id.parse().ok()?)))
}

#[async_trait]
impl ChangeFeed for PgChangeFeed {
    async fn subscribe(&self, org: OrgId) -> Result<NoticeStream, FeedError> {
        let stream = BroadcastStream::new(self.inner.notices.subscribe()).filter_map(move |item| async move {
            match item {
                Ok(FeedNotice::Entry { org: o, id }) if o == org => Some(FeedNotice::Entry { org: o, id }),
                Ok(FeedNotice::Entry { .. }) => None,
                Ok(FeedNotice::Lagged) | Err(BroadcastStreamRecvError::Lagged(_)) => Some(FeedNotice::Lagged),
            }
        });
        Ok(Box::pin(stream))
    }

    async fn after(&self, org: OrgId, after: FeedId, limit: u32) -> Result<Vec<FeedEntry>, FeedError> {
        let limit = i64::from(limit.clamp(1, FEED_PAGE_MAX));
        retry_tx(|| async {
            let mut tx = begin_org(&self.inner.pools.tenant, org).await?;
            let rows: Vec<(i64, String, String, OffsetDateTime)> =
                sqlx::query_as("SELECT id, kind, subject, at FROM org_feed WHERE org_id = $1 AND id > $2 ORDER BY id LIMIT $3")
                    .bind(org.as_uuid())
                    .bind(after.0)
                    .bind(limit)
                    .fetch_all(&mut *tx)
                    .await?;
            tx.commit().await?;
            rows.into_iter()
                .map(|(id, kind, subject, at)| {
                    let kind = parse_feed_kind(&kind).ok_or_else(|| TxError::Db(DbError::Invalid(format!("org_feed.kind: unknown value {kind:?}"))))?;
                    Ok(FeedEntry { id: FeedId(id), org, kind, subject, at })
                })
                .collect::<Result<Vec<_>, TxError<FeedError>>>()
        })
        .await
    }

    async fn head(&self, org: OrgId) -> Result<FeedId, FeedError> {
        retry_tx(|| async {
            let mut tx = begin_org(&self.inner.pools.tenant, org).await?;
            let head: Option<i64> =
                sqlx::query_scalar("SELECT next_id FROM org_feed_heads WHERE org_id = $1").bind(org.as_uuid()).fetch_optional(&mut *tx).await?;
            tx.commit().await?;
            Ok::<_, TxError<FeedError>>(FeedId(head.unwrap_or(0)))
        })
        .await
    }

    async fn append(&self, org: OrgId, kind: FeedKind, subject: &str) -> Result<FeedId, FeedError> {
        if subject.is_empty() {
            return Err(FeedError::Invalid("feed subject is empty".into()));
        }
        let at = self.inner.clock.now();
        retry_tx(|| async {
            let mut tx = begin_org(&self.inner.pools.tenant, org).await?;
            let id = append_in(&mut tx, org, kind, subject, at).await?;
            tx.commit().await?;
            Ok::<_, TxError<FeedError>>(id)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::time::Duration;

    use super::*;
    use crate::testing::contract::{FeedFixture, contract_change_feed};
    use crate::testing::Seed;
    use crate::testing::pg::with_pg;

    /// Polls `ready` every 20 ms for up to 10 s.
    async fn wait_until<F, Fut>(what: &str, mut ready: F)
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = bool>,
    {
        for _ in 0..500 {
            if ready().await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("timed out waiting until {what}");
    }

    /// Backend pids of this database's `LISTEN` connections.
    async fn listener_pids(admin: &sqlx::PgPool) -> Vec<i32> {
        sqlx::query_scalar(
            "SELECT pid FROM pg_stat_activity WHERE datname = current_database() AND query ILIKE 'LISTEN%' AND pid <> pg_backend_pid()",
        )
        .fetch_all(admin)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn pg_change_feed_passes_the_contract() {
        with_pg("pg_change_feed_passes_the_contract", |env| async move {
            contract_change_feed(|clock| {
                let env = env.clone();
                async move {
                    let db = env.fresh().await;
                    let feed = PgChangeFeed::new(db.pools.clone(), clock.clone());
                    let shutdown = CancellationToken::new();
                    tokio::spawn({
                        let feed = feed.clone();
                        let shutdown = shutdown.clone();
                        async move { feed.listen(shutdown).await }
                    });
                    wait_until("the listener is up", || {
                        let feed = feed.clone();
                        async move { feed.listening() }
                    })
                    .await;
                    let admin = db.admin.clone();
                    let watched = feed.clone();
                    FeedFixture {
                        feed: Arc::new(feed),
                        seed: db.seed(clock),
                        drop_listener: Arc::new(move || {
                            let admin = admin.clone();
                            let watched = watched.clone();
                            Box::pin(async move {
                                let old = listener_pids(&admin).await;
                                assert_eq!(old.len(), 1, "one LISTEN connection per feed");
                                sqlx::query("SELECT pg_terminate_backend($1)").bind(old[0]).execute(&admin).await.unwrap();
                                let old_pid = old[0];
                                wait_until("the listener reconnected", || {
                                    let admin = admin.clone();
                                    let watched = watched.clone();
                                    async move {
                                        let pids = listener_pids(&admin).await;
                                        watched.listening() && !pids.is_empty() && !pids.contains(&old_pid)
                                    }
                                })
                                .await;
                            })
                        }),
                        keep_alive: Some(Box::new(shutdown.drop_guard())),
                    }
                }
            })
            .await;
        })
        .await;
    }

    #[test]
    fn notification_payloads_parse() {
        let org = Uuid::from_u128(5);
        assert_eq!(parse_payload(&format!("{org}:42")), Some((OrgId::from_uuid(org), FeedId(42))));
        assert_eq!(parse_payload("nope"), None);
        assert_eq!(parse_payload(&format!("{org}:x")), None);
        for kind in [FeedKind::Ticket, FeedKind::Cause, FeedKind::Run, FeedKind::Repo, FeedKind::Recipe, FeedKind::Integration, FeedKind::Settings, FeedKind::Member] {
            assert_eq!(parse_feed_kind(feed_kind_str(kind)), Some(kind));
            assert_eq!(serde_json::to_value(kind).unwrap(), serde_json::Value::String(feed_kind_str(kind).into()), "column value = wire name");
        }
    }

    /// storage.md "Tenancy tests": a tenant-pool connection without `mage.org_id` sees zero rows,
    /// and a transaction scoped to org A can neither read nor write org B's rows.
    #[tokio::test]
    async fn pg_tenant_pool_is_fenced_by_row_level_security() {
        with_pg("pg_tenant_pool_is_fenced_by_row_level_security", |env| async move {
            let db = env.fresh().await;
            let restricted: bool = sqlx::query_scalar("SELECT NOT rolsuper AND NOT rolbypassrls FROM pg_roles WHERE rolname = current_user")
                .fetch_one(&db.pools.tenant)
                .await
                .unwrap();
            if !restricted {
                eprintln!("skipping: tenant pool is not a restricted role");
                return;
            }
            let clock: Arc<dyn Clock> = Arc::new(mage_core::testing::FakeClock::new());
            let seed = db.seed(clock.clone());
            let (a, b) = (OrgId::from_uuid(Uuid::from_u128(0xa1)), OrgId::from_uuid(Uuid::from_u128(0xb2)));
            seed.org(a).await;
            seed.org(b).await;
            let feed = PgChangeFeed::new(db.pools.clone(), clock);
            feed.append(a, FeedKind::Ticket, "tkt_a").await.unwrap();
            feed.append(b, FeedKind::Ticket, "tkt_b").await.unwrap();

            let unscoped: i64 = sqlx::query_scalar("SELECT count(*) FROM org_feed").fetch_one(&db.pools.tenant).await.unwrap();
            assert_eq!(unscoped, 0, "no mage.org_id → no rows");

            let mut tx = begin_org(&db.pools.tenant, a).await.unwrap();
            let visible: Vec<uuid::Uuid> = sqlx::query_scalar("SELECT org_id FROM org_feed").fetch_all(&mut *tx).await.unwrap();
            assert_eq!(visible, vec![a.as_uuid()], "only org A's rows, even without a WHERE");
            let foreign = sqlx::query("INSERT INTO org_feed (org_id, id, kind, subject, at) VALUES ($1, 99, 'ticket', 'x', now())")
                .bind(b.as_uuid())
                .execute(&mut *tx)
                .await
                .expect_err("writing org B's row from org A's transaction");
            assert_eq!(foreign.as_database_error().and_then(|e| e.code()).as_deref(), Some("42501"), "{foreign}");
            drop(tx);

            let mut tx = begin_org(&db.pools.tenant, a).await.unwrap();
            let updated = sqlx::query("UPDATE org_feed SET subject = 'hijacked' WHERE org_id = $1").bind(b.as_uuid()).execute(&mut *tx).await.unwrap();
            assert_eq!(updated.rows_affected(), 0, "org B's rows are invisible to updates");
            tx.commit().await.unwrap();
            assert_eq!(feed.after(b, FeedId(0), 10).await.unwrap()[0].subject, "tkt_b");
        })
        .await;
    }
}
