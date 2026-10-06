//! `AuthorizeKnowledge`: a `knowledge_token` grants its scope only while its run is live at the
//! token's attempt; other kinds, revoked `jti`s and terminal runs are refused.

use std::sync::Arc;
use std::time::Duration;

use mage_core::domain::{Budget, Layer, Run, RunId, RunKind, RunStatus, RunSubject, TicketId};
use mage_core::ports::clock::Clock;
use mage_core::testing::{FakeClock, SeqIdGen};
use mage_core::wire::token::{MemoryCutoff, TokenScope};
use uuid::Uuid;

use super::Fx;
use crate::app::auth::tokens::TokenService;
use crate::app::knowledge_query::AuthorizeKnowledge;
use crate::error::AppError;
use crate::ports::runs::{NewRun, RunStore, RunWrite};
use crate::testing::runs::{sample_run, sample_spec};
use crate::testing::{FakeTokenSigner, MemTokenStore};

struct Auth {
    tokens: Arc<TokenService>,
    auth: AuthorizeKnowledge,
}

fn auth(fx: &Fx) -> Auth {
    let clock: Arc<FakeClock> = fx.clock.clone();
    let store = Arc::new(MemTokenStore::new(clock.clone()));
    let tokens = Arc::new(TokenService::new(Arc::new(FakeTokenSigner::new("https://mage.test")), store, clock.clone(), Arc::new(SeqIdGen::new())));
    let auth = AuthorizeKnowledge::new(tokens.clone(), fx.runs.clone(), clock);
    Auth { tokens, auth }
}

async fn insert_run(fx: &Fx, n: u128, status: RunStatus) -> RunId {
    let id = RunId::from_uuid(Uuid::from_u128(0x0c10 + n));
    let budget = Budget { max_wall: Duration::from_secs(600), max_turns: 10, max_cost_micros: 1, max_gate_attempts: 1 };
    let subject = RunSubject::Ticket(TicketId::from_uuid(Uuid::from_u128(0x7100 + n)));
    let mut data = sample_run(fx.org, id, RunKind::Triage, subject, budget, fx.clock.now()).into_data();
    data.status = status;
    let insert = NewRun { run: Run::rehydrate(data), spec: sample_spec(RunKind::Triage), create_key: None, source_ready: true, affinity: None };
    fx.runs.commit(fx.org, RunWrite { inserts: vec![insert], ..Default::default() }).await.unwrap_or_else(|e| panic!("{e}"));
    id
}

fn knowledge(fx: &Fx, run: RunId, attempt: u16) -> TokenScope {
    TokenScope::Knowledge {
        org: fx.org,
        run,
        attempt,
        repo: fx.repo,
        sha: fx.sha,
        memory: MemoryCutoff { exclude_tickets: vec![TicketId::from_uuid(Uuid::from_u128(9))], as_of: None },
        disabled_layers: vec![Layer::Semantic],
    }
}

#[tokio::test]
async fn a_live_run_token_yields_its_scope() {
    let fx = Fx::new().await;
    let a = auth(&fx);
    let run = insert_run(&fx, 1, RunStatus::Running).await;
    let (token, _) = a.tokens.mint(knowledge(&fx, run, 1), time::Duration::hours(1)).unwrap_or_else(|e| panic!("{e}"));
    let scope = a.auth.authorize(token.as_str()).await.unwrap_or_else(|e| panic!("{e}"));
    assert_eq!((scope.org, scope.run, scope.repo, scope.sha), (fx.org, run, fx.repo, fx.sha));
    assert_eq!(scope.memory.exclude_tickets.len(), 1);
    assert_eq!(scope.disabled_layers, vec![Layer::Semantic]);
}

#[tokio::test]
async fn stale_attempts_terminal_runs_other_kinds_and_revoked_tokens_are_refused() {
    let fx = Fx::new().await;
    let a = auth(&fx);
    let live = insert_run(&fx, 1, RunStatus::Running).await;
    let (old_attempt, _) = a.tokens.mint(knowledge(&fx, live, 2), time::Duration::hours(1)).unwrap_or_else(|e| panic!("{e}"));
    assert!(matches!(a.auth.authorize(old_attempt.as_str()).await, Err(AppError::Unauthorized)), "attempt changed");

    let done = insert_run(&fx, 2, RunStatus::Cancelled).await;
    let (finished, _) = a.tokens.mint(knowledge(&fx, done, 1), time::Duration::hours(1)).unwrap_or_else(|e| panic!("{e}"));
    assert!(matches!(a.auth.authorize(finished.as_str()).await, Err(AppError::Unauthorized)), "run terminal");

    let missing = RunId::from_uuid(Uuid::from_u128(0x0cff));
    let (unknown, _) = a.tokens.mint(knowledge(&fx, missing, 1), time::Duration::hours(1)).unwrap_or_else(|e| panic!("{e}"));
    assert!(matches!(a.auth.authorize(unknown.as_str()).await, Err(AppError::Unauthorized)), "unknown run");

    let gateway = TokenScope::Gateway { org: fx.org, run: live, attempt: 1, models: vec![] };
    let (wrong_kind, _) = a.tokens.mint(gateway, time::Duration::hours(1)).unwrap_or_else(|e| panic!("{e}"));
    assert!(matches!(a.auth.authorize(wrong_kind.as_str()).await, Err(AppError::Unauthorized)), "gateway token");
    assert!(matches!(a.auth.authorize("mgk_not.a.token").await, Err(AppError::Unauthorized)));

    let (token, claims) = a.tokens.mint(knowledge(&fx, live, 1), time::Duration::hours(1)).unwrap_or_else(|e| panic!("{e}"));
    assert!(a.auth.authorize(token.as_str()).await.is_ok());
    a.tokens.revoke(&claims, Some(fx.org), "test").await.unwrap_or_else(|e| panic!("{e}"));
    assert!(matches!(a.auth.authorize(token.as_str()).await, Err(AppError::Unauthorized)), "revoked jti");
}

#[tokio::test]
async fn expired_tokens_are_refused() {
    let fx = Fx::new().await;
    let a = auth(&fx);
    let run = insert_run(&fx, 1, RunStatus::Running).await;
    let (token, _) = a.tokens.mint(knowledge(&fx, run, 1), time::Duration::minutes(5)).unwrap_or_else(|e| panic!("{e}"));
    fx.clock.advance(Duration::from_secs(3600));
    assert!(matches!(a.auth.authorize(token.as_str()).await, Err(AppError::Unauthorized)));
}
