//! `PgTokenStore` (storage.md "`TokenStore`"): sessions, one-time OAuth / install states, runner
//! bootstrap tokens, runner identities and revoked `jti`s. Every table is a platform table, so
//! every method runs on the platform pool (storage.md "Tenancy enforcement").
//!
//! `register_runner` is storage.md's four steps in one transaction: the bootstrap row is locked
//! with `SELECT … FOR UPDATE`, so concurrent registrations with one token serialize and only the
//! first consumes it; the runner is upserted on its unique `name` (insert with the token's scope,
//! or re-bootstrap: new key, generation + 1, Active). Registration writes `capabilities` but never
//! the session columns `RunStore::record_runner_session` owns (`instance_id`, `version`,
//! `last_seen_at`).

use std::sync::Arc;

use async_trait::async_trait;
use mage_core::domain::{OrgId, RunnerId, UserId};
use mage_core::ports::clock::Clock;
use mage_core::ports::ids::IdGen;
use mage_infra::pg::{DbError, PgPools, Tx, TxError, begin_platform, retry_tx};
use sqlx::types::Json;
use time::OffsetDateTime;
use uuid::Uuid;

use super::codec::{fixed, ip_text, parse_enum, parse_ip, to_unsigned};
use crate::ports::org::{integration_kind_str, parse_integration_kind};
use crate::ports::tokens::{
    BOOTSTRAP_RETRY_WINDOW, Jti, NewBootstrapToken, NewSession, OAuthPurpose, OAuthState, Revocation, Runner, RunnerRegistration,
    RunnerScope, RunnerStatus, Session, TokenError, TokenStore, bootstrap_invalid,
};

pub struct PgTokenStore {
    pools: PgPools,
    clock: Arc<dyn Clock>,
}

impl PgTokenStore {
    /// Ids are never generated here (runner ids come with the registration).
    pub fn new(pools: PgPools, clock: Arc<dyn Clock>, _ids: Arc<dyn IdGen>) -> Self {
        Self { pools, clock }
    }
}

impl From<DbError> for TokenError {
    fn from(err: DbError) -> Self {
        match err {
            DbError::NotFound => TokenError::NotFound,
            DbError::Conflict { .. } => TokenError::Conflict,
            DbError::Invalid(msg) => TokenError::Invalid(msg),
            DbError::Unavailable(msg) | DbError::Retry(msg) => TokenError::Unavailable(msg),
            DbError::Timeout => TokenError::Unavailable("statement timeout".into()),
        }
    }
}

type TxResult<T> = Result<T, TxError<TokenError>>;

#[derive(sqlx::FromRow)]
struct SessionRow {
    id_hash: Vec<u8>,
    user_id: Uuid,
    csrf_secret: Vec<u8>,
    created_at: OffsetDateTime,
    last_seen_at: OffsetDateTime,
    expires_at: OffsetDateTime,
    ip: Option<String>,
    user_agent: Option<String>,
}

impl SessionRow {
    fn into_session(self) -> Result<Session, DbError> {
        Ok(Session {
            id_hash: fixed(self.id_hash, "sessions.id_hash")?,
            user_id: UserId::from_uuid(self.user_id),
            csrf_secret: self.csrf_secret,
            created_at: self.created_at,
            last_seen_at: self.last_seen_at,
            expires_at: self.expires_at,
            ip: parse_ip(self.ip, "sessions.ip")?,
            user_agent: self.user_agent,
        })
    }
}

#[derive(sqlx::FromRow)]
struct OAuthRow {
    state_hash: Vec<u8>,
    purpose: String,
    org_id: Option<Uuid>,
    provider: Option<String>,
    user_id: Option<Uuid>,
    pkce_verifier: Option<String>,
    return_to: Option<String>,
    expires_at: OffsetDateTime,
}

impl OAuthRow {
    fn into_state(self) -> Result<OAuthState, DbError> {
        let purpose = match (self.purpose.as_str(), self.org_id, self.provider) {
            ("login", None, None) => OAuthPurpose::Login,
            ("install", Some(org), Some(provider)) => OAuthPurpose::Install {
                org: OrgId::from_uuid(org),
                provider: parse_enum(&provider, "oauth_states.provider", parse_integration_kind)?,
            },
            (purpose, _, _) => return Err(DbError::Invalid(format!("oauth_states: purpose {purpose:?} does not match org_id / provider"))),
        };
        Ok(OAuthState {
            state_hash: fixed(self.state_hash, "oauth_states.state_hash")?,
            purpose,
            user: self.user_id.map(UserId::from_uuid),
            pkce_verifier: self.pkce_verifier,
            return_to: self.return_to,
            expires_at: self.expires_at,
        })
    }
}

/// The locked bootstrap row `register_runner` decides on.
#[derive(sqlx::FromRow)]
struct BootstrapRow {
    name: String,
    scope: Json<RunnerScope>,
    expires_at: OffsetDateTime,
    used_at: Option<OffsetDateTime>,
    runner_id: Option<Uuid>,
}

/// `runners` identity columns, in `RunnerRow` order.
macro_rules! runner_columns {
    () => {
        "id, name, scope, status, public_key, generation, registered_at, last_seen_at"
    };
}

const SELECT_RUNNER: &str = concat!("SELECT ", runner_columns!(), " FROM runners WHERE id = $1");

/// Step 3: a new name inserts with the token's scope; an existing name is re-bootstrapped (scope,
/// id and `registered_at` stay). A taken id under a new name is a unique violation (`Conflict`).
const UPSERT_RUNNER: &str = concat!(
    "INSERT INTO runners (id, name, scope, status, public_key, generation, registered_at, capabilities) \
     VALUES ($1, $2, $3, 'active', $4, 1, $5, $6) \
     ON CONFLICT (name) DO UPDATE SET public_key = EXCLUDED.public_key, generation = runners.generation + 1, \
                                      status = 'active', capabilities = EXCLUDED.capabilities \
     RETURNING ",
    runner_columns!()
);

/// Entering `revoked` bumps the generation (the CASE sees the old status).
const SET_RUNNER_STATUS: &str = concat!(
    "UPDATE runners SET status = $2, \
            generation = generation + CASE WHEN $2 = 'revoked' AND status <> 'revoked' THEN 1 ELSE 0 END \
     WHERE id = $1 RETURNING ",
    runner_columns!()
);

#[derive(sqlx::FromRow)]
struct RunnerRow {
    id: Uuid,
    name: String,
    scope: Json<RunnerScope>,
    status: String,
    public_key: Vec<u8>,
    generation: i32,
    registered_at: OffsetDateTime,
    last_seen_at: Option<OffsetDateTime>,
}

impl RunnerRow {
    fn into_runner(self) -> Result<Runner, DbError> {
        Ok(Runner {
            id: RunnerId::from_uuid(self.id),
            name: self.name,
            scope: self.scope.0,
            status: parse_enum(&self.status, "runners.status", RunnerStatus::parse)?,
            public_key: fixed(self.public_key, "runners.public_key")?,
            generation: to_unsigned(i64::from(self.generation), "runners.generation")?,
            registered_at: self.registered_at,
            last_seen_at: self.last_seen_at,
        })
    }
}

impl PgTokenStore {
    /// storage.md `register_runner` steps 1–4 inside `tx` (committed by the caller).
    async fn register_in(&self, tx: &mut Tx, bootstrap_hash: &[u8; 32], reg: &RunnerRegistration, now: OffsetDateTime) -> TxResult<Runner> {
        // 1. Lock the token; missing, expired or another name → invalid.
        let token: Option<BootstrapRow> = sqlx::query_as(
            "SELECT name, scope, expires_at, used_at, runner_id FROM runner_bootstrap_tokens WHERE token_hash = $1 FOR UPDATE",
        )
        .bind(bootstrap_hash.as_slice())
        .fetch_optional(&mut **tx)
        .await?;
        let token = token.filter(|t| t.expires_at > now && t.name == reg.name).ok_or_else(|| TxError::Port(bootstrap_invalid()))?;

        // 2. Used: only a same-key retry within the window gets the runner back.
        if let Some(used_at) = token.used_at {
            if now - used_at > BOOTSTRAP_RETRY_WINDOW {
                return Err(TxError::Port(bootstrap_invalid()));
            }
            let Some(runner_id) = token.runner_id else {
                return Err(TxError::Port(bootstrap_invalid()));
            };
            let row: Option<RunnerRow> = sqlx::query_as(SELECT_RUNNER).bind(runner_id).fetch_optional(&mut **tx).await?;
            let runner = row.map(RunnerRow::into_runner).transpose()?;
            return runner.filter(|r| r.public_key == reg.public_key).ok_or_else(|| TxError::Port(bootstrap_invalid()));
        }

        // 3. New name: insert with the token's scope. Existing name: re-bootstrap.
        let row: RunnerRow = sqlx::query_as(UPSERT_RUNNER)
            .bind(reg.id.as_uuid())
            .bind(&reg.name)
            .bind(&token.scope)
            .bind(reg.public_key.as_slice())
            .bind(now)
            .bind(Json(&reg.capabilities))
            .fetch_one(&mut **tx)
            .await?;

        // 4. Consume the token.
        sqlx::query("UPDATE runner_bootstrap_tokens SET used_at = $2, runner_id = $3 WHERE token_hash = $1")
            .bind(bootstrap_hash.as_slice())
            .bind(now)
            .bind(row.id)
            .execute(&mut **tx)
            .await?;
        Ok(row.into_runner()?)
    }
}

#[async_trait]
impl TokenStore for PgTokenStore {
    async fn create_session(&self, s: &NewSession) -> Result<(), TokenError> {
        let now = self.clock.now();
        retry_tx(|| async {
            sqlx::query(
                "INSERT INTO sessions (id_hash, user_id, csrf_secret, created_at, last_seen_at, expires_at, ip, user_agent) \
                 VALUES ($1, $2, $3, $4, $4, $5, $6::text::inet, $7)",
            )
            .bind(s.id_hash.as_slice())
            .bind(s.user_id.as_uuid())
            .bind(&s.csrf_secret)
            .bind(now)
            .bind(s.expires_at)
            .bind(ip_text(s.ip))
            .bind(s.user_agent.as_deref())
            .execute(&self.pools.platform)
            .await?;
            Ok::<_, TxError<TokenError>>(())
        })
        .await
    }

    async fn session(&self, id_hash: &[u8; 32]) -> Result<Option<Session>, TokenError> {
        let now = self.clock.now();
        retry_tx(|| async {
            let row: Option<SessionRow> = sqlx::query_as(
                "SELECT id_hash, user_id, csrf_secret, created_at, last_seen_at, expires_at, host(ip) AS ip, user_agent \
                 FROM sessions WHERE id_hash = $1 AND expires_at > $2",
            )
            .bind(id_hash.as_slice())
            .bind(now)
            .fetch_optional(&self.pools.platform)
            .await?;
            Ok::<_, TxError<TokenError>>(row.map(SessionRow::into_session).transpose()?)
        })
        .await
    }

    async fn touch_session(&self, id_hash: &[u8; 32], expires_at: OffsetDateTime) -> Result<(), TokenError> {
        let now = self.clock.now();
        retry_tx(|| async {
            let updated = sqlx::query("UPDATE sessions SET last_seen_at = $2, expires_at = $3 WHERE id_hash = $1 AND expires_at > $2")
                .bind(id_hash.as_slice())
                .bind(now)
                .bind(expires_at)
                .execute(&self.pools.platform)
                .await?;
            if updated.rows_affected() == 0 {
                return Err(TxError::Port(TokenError::NotFound));
            }
            Ok(())
        })
        .await
    }

    async fn delete_session(&self, id_hash: &[u8; 32]) -> Result<(), TokenError> {
        retry_tx(|| async {
            sqlx::query("DELETE FROM sessions WHERE id_hash = $1").bind(id_hash.as_slice()).execute(&self.pools.platform).await?;
            Ok::<_, TxError<TokenError>>(())
        })
        .await
    }

    async fn delete_user_sessions(&self, user: UserId) -> Result<u64, TokenError> {
        retry_tx(|| async {
            let deleted = sqlx::query("DELETE FROM sessions WHERE user_id = $1").bind(user.as_uuid()).execute(&self.pools.platform).await?;
            Ok::<_, TxError<TokenError>>(deleted.rows_affected())
        })
        .await
    }

    async fn put_oauth_state(&self, s: &OAuthState) -> Result<(), TokenError> {
        let now = self.clock.now();
        let (purpose, org, provider) = match &s.purpose {
            OAuthPurpose::Login => ("login", None, None),
            OAuthPurpose::Install { org, provider } => ("install", Some(org.as_uuid()), Some(integration_kind_str(*provider))),
        };
        retry_tx(|| async {
            sqlx::query(
                "INSERT INTO oauth_states (state_hash, purpose, org_id, provider, user_id, pkce_verifier, return_to, expires_at, created_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            )
            .bind(s.state_hash.as_slice())
            .bind(purpose)
            .bind(org)
            .bind(provider)
            .bind(s.user.map(|u| u.as_uuid()))
            .bind(s.pkce_verifier.as_deref())
            .bind(s.return_to.as_deref())
            .bind(s.expires_at)
            .bind(now)
            .execute(&self.pools.platform)
            .await?;
            Ok::<_, TxError<TokenError>>(())
        })
        .await
    }

    async fn take_oauth_state(&self, state_hash: &[u8; 32]) -> Result<Option<OAuthState>, TokenError> {
        let now = self.clock.now();
        retry_tx(|| async {
            let row: Option<OAuthRow> = sqlx::query_as(
                "DELETE FROM oauth_states WHERE state_hash = $1 AND expires_at > $2 \
                 RETURNING state_hash, purpose, org_id, provider, user_id, pkce_verifier, return_to, expires_at",
            )
            .bind(state_hash.as_slice())
            .bind(now)
            .fetch_optional(&self.pools.platform)
            .await?;
            Ok::<_, TxError<TokenError>>(row.map(OAuthRow::into_state).transpose()?)
        })
        .await
    }

    async fn create_bootstrap(&self, b: &NewBootstrapToken) -> Result<(), TokenError> {
        let now = self.clock.now();
        let created_by = b.created_by.encode();
        retry_tx(|| async {
            sqlx::query(
                "INSERT INTO runner_bootstrap_tokens (token_hash, name, scope, expires_at, created_by, created_at) \
                 VALUES ($1, $2, $3, $4, $5, $6)",
            )
            .bind(b.token_hash.as_slice())
            .bind(&b.name)
            .bind(Json(&b.scope))
            .bind(b.expires_at)
            .bind(&created_by)
            .bind(now)
            .execute(&self.pools.platform)
            .await?;
            Ok::<_, TxError<TokenError>>(())
        })
        .await
    }

    async fn register_runner(&self, bootstrap_hash: &[u8; 32], reg: &RunnerRegistration) -> Result<Runner, TokenError> {
        let now = self.clock.now();
        retry_tx(|| async {
            let mut tx = begin_platform(&self.pools.platform).await?;
            let runner = self.register_in(&mut tx, bootstrap_hash, reg, now).await?;
            tx.commit().await?;
            Ok::<_, TxError<TokenError>>(runner)
        })
        .await
    }

    async fn runner(&self, id: RunnerId) -> Result<Runner, TokenError> {
        retry_tx(|| async {
            let row: Option<RunnerRow> = sqlx::query_as(SELECT_RUNNER).bind(id.as_uuid()).fetch_optional(&self.pools.platform).await?;
            let row = row.ok_or(TxError::Port(TokenError::NotFound))?;
            Ok::<_, TxError<TokenError>>(row.into_runner()?)
        })
        .await
    }

    async fn set_runner_status(&self, id: RunnerId, status: RunnerStatus) -> Result<Runner, TokenError> {
        retry_tx(|| async {
            let row: Option<RunnerRow> =
                sqlx::query_as(SET_RUNNER_STATUS).bind(id.as_uuid()).bind(status.as_str()).fetch_optional(&self.pools.platform).await?;
            let row = row.ok_or(TxError::Port(TokenError::NotFound))?;
            Ok::<_, TxError<TokenError>>(row.into_runner()?)
        })
        .await
    }

    async fn revoke(&self, r: &Revocation) -> Result<(), TokenError> {
        let now = self.clock.now();
        retry_tx(|| async {
            sqlx::query(
                "INSERT INTO token_revocations (jti, org_id, reason, revoked_at, expires_at) VALUES ($1, $2, $3, $4, $5) \
                 ON CONFLICT (jti) DO NOTHING",
            )
            .bind(r.jti.0.as_slice())
            .bind(r.org_id.map(|o| o.as_uuid()))
            .bind(&r.reason)
            .bind(now)
            .bind(r.expires_at)
            .execute(&self.pools.platform)
            .await?;
            Ok::<_, TxError<TokenError>>(())
        })
        .await
    }

    async fn is_revoked(&self, jti: &Jti) -> Result<bool, TokenError> {
        retry_tx(|| async {
            let revoked: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM token_revocations WHERE jti = $1)")
                .bind(jti.0.as_slice())
                .fetch_one(&self.pools.platform)
                .await?;
            Ok::<_, TxError<TokenError>>(revoked)
        })
        .await
    }

    async fn purge_expired(&self, limit: u32) -> Result<u64, TokenError> {
        const PURGES: [&str; 4] = [
            "DELETE FROM sessions WHERE id_hash IN \
             (SELECT id_hash FROM sessions WHERE expires_at <= $1 ORDER BY expires_at LIMIT $2)",
            "DELETE FROM oauth_states WHERE state_hash IN \
             (SELECT state_hash FROM oauth_states WHERE expires_at <= $1 ORDER BY expires_at LIMIT $2)",
            "DELETE FROM runner_bootstrap_tokens WHERE token_hash IN \
             (SELECT token_hash FROM runner_bootstrap_tokens WHERE expires_at <= $1 LIMIT $2)",
            "DELETE FROM token_revocations WHERE jti IN \
             (SELECT jti FROM token_revocations WHERE expires_at <= $1 ORDER BY expires_at LIMIT $2)",
        ];
        let now = self.clock.now();
        let limit = i64::from(limit);
        retry_tx(|| async {
            let mut tx = begin_platform(&self.pools.platform).await?;
            let mut deleted = 0;
            for sql in PURGES {
                deleted += sqlx::query(sql).bind(now).bind(limit).execute(&mut *tx).await?.rows_affected();
            }
            tx.commit().await?;
            Ok::<_, TxError<TokenError>>(deleted)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use mage_core::testing::{FakeClock, SeqIdGen};
    use mage_core::wire::runner::{Arch, Capabilities, ExecutorKind, Os, SlotCounts};

    use super::*;
    use crate::ports::audit::Actor;
    use crate::testing::contract::{TokenFixture, contract_token_store};
    use crate::testing::pg::with_pg;

    #[tokio::test]
    async fn pg_token_store_passes_the_contract() {
        with_pg("pg_tokens_contract", |env| async move {
            contract_token_store(|clock| {
                let env = env.clone();
                async move {
                    let db = env.fresh().await;
                    let seed = db.seed(clock.clone());
                    TokenFixture { tokens: Arc::new(PgTokenStore::new(db.pools.clone(), clock, Arc::new(SeqIdGen::new()))), seed }
                }
            })
            .await;
        })
        .await;
    }

    /// The session columns belong to `RunStore::record_runner_session`; registration owns
    /// `capabilities` (lease matching reads them).
    #[tokio::test]
    async fn re_bootstrap_writes_capabilities_and_keeps_session_columns() {
        with_pg("pg_tokens_capabilities", |env| async move {
            let db = env.fresh().await;
            let clock = Arc::new(FakeClock::new());
            let store = PgTokenStore::new(db.pools.clone(), clock.clone(), Arc::new(SeqIdGen::new()));
            let caps = |work| Capabilities {
                arch: Arch::X86_64,
                os: Os::Linux,
                executor: ExecutorKind::Host,
                harnesses: Vec::new(),
                browser: true,
                slots: SlotCounts { light: 1, work, verify: 0 },
            };
            let expires_at = clock.now() + time::Duration::hours(1);
            let id = RunnerId::from_uuid(Uuid::from_u128(1));
            for (n, work) in [(1u8, 1u16), (2, 4)] {
                let token = NewBootstrapToken {
                    token_hash: [n; 32],
                    name: "runner-a".into(),
                    scope: RunnerScope::Shared,
                    expires_at,
                    created_by: Actor::Operator { os_user: "ops".into() },
                };
                store.create_bootstrap(&token).await.unwrap();
                let reg = RunnerRegistration { id, name: "runner-a".into(), public_key: [n; 32], capabilities: caps(work) };
                store.register_runner(&[n; 32], &reg).await.unwrap();
                if n == 1 {
                    sqlx::query("UPDATE runners SET instance_id = $2, version = '1.2.3', last_seen_at = $3 WHERE id = $1")
                        .bind(id.as_uuid())
                        .bind(Uuid::from_u128(7))
                        .bind(clock.now())
                        .execute(&db.admin)
                        .await
                        .unwrap();
                }
            }
            let (stored, instance, version, last_seen): (Json<Capabilities>, Option<Uuid>, Option<String>, Option<OffsetDateTime>) =
                sqlx::query_as("SELECT capabilities, instance_id, version, last_seen_at FROM runners WHERE id = $1")
                    .bind(id.as_uuid())
                    .fetch_one(&db.admin)
                    .await
                    .unwrap();
            assert_eq!(stored.0, caps(4), "the latest registration's capabilities");
            assert_eq!((instance, version.as_deref(), last_seen), (Some(Uuid::from_u128(7)), Some("1.2.3"), Some(clock.now())));
            assert_eq!(store.runner(id).await.unwrap().last_seen_at, Some(clock.now()));
        })
        .await;
    }
}
