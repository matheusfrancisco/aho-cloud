//! `TokenStore` contract (storage.md "Testing" → `TokenStore`, and the `register_runner` steps of
//! storage.md "`TokenStore`"): sessions expire by the clock, OAuth states and bootstrap tokens are
//! one-time, a lost registration response may be retried with the same key for 10 minutes,
//! re-bootstrap keeps the runner id and bumps its generation, revocation is idempotent, purges are
//! capped per kind.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;

use mage_core::domain::{IntegrationKind, OrgId, RunnerId, UserId};
use mage_core::ports::clock::Clock;
use mage_core::testing::{FakeClock, join_all};
use mage_core::wire::runner::{Arch, Capabilities, ExecutorKind, Os, SlotCounts};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::ports::audit::Actor;
use crate::ports::tokens::{
    BOOTSTRAP_RETRY_WINDOW, Jti, NewBootstrapToken, NewSession, OAuthPurpose, OAuthState, Revocation, Runner, RunnerRegistration,
    RunnerScope, RunnerStatus, Session, TokenError, TokenStore, bootstrap_invalid,
};
use crate::testing::seed::Seed;

pub struct TokenFixture {
    pub tokens: Arc<dyn TokenStore>,
    pub seed: Arc<dyn Seed>,
}

/// Runs every `TokenStore` scenario, each against a fresh fixture from `make`.
pub async fn contract_token_store<F, Fut>(make: F)
where
    F: Fn(Arc<FakeClock>) -> Fut,
    Fut: Future<Output = TokenFixture>,
{
    macro_rules! scenario {
        ($name:ident) => {{
            let clock = Arc::new(FakeClock::new());
            let fixture = make(clock.clone()).await;
            $name(&clock, &*fixture.tokens, &*fixture.seed).await;
        }};
    }
    scenario!(sessions_round_trip_touch_and_expire);
    scenario!(delete_user_sessions_removes_only_that_users);
    scenario!(oauth_states_are_one_time_and_ignore_expired);
    scenario!(bootstrap_tokens_are_one_time);
    scenario!(lost_response_retry_within_the_window_returns_the_same_runner);
    scenario!(bootstrap_checks_name_and_expiry);
    scenario!(re_bootstrap_bumps_generation_and_keeps_the_id);
    scenario!(new_runner_id_collision_is_a_conflict_and_consumes_nothing);
    scenario!(runner_status_changes_and_revocation_bumps_generation);
    scenario!(revocations_are_idempotent);
    scenario!(purge_expired_caps_each_kind);
    scenario!(one_bootstrap_token_registers_one_runner_under_concurrency);
}

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

fn org(n: u128) -> OrgId {
    OrgId::from_uuid(Uuid::from_u128(0x0c00 + n))
}

fn user(n: u128) -> UserId {
    UserId::from_uuid(Uuid::from_u128(0x0d00 + n))
}

fn runner_id(n: u128) -> RunnerId {
    RunnerId::from_uuid(Uuid::from_u128(0x0e00 + n))
}

/// A 32-byte hash or public key filled with `n`.
fn h(n: u8) -> [u8; 32] {
    [n; 32]
}

fn session(id: u8, user_id: UserId, expires_at: OffsetDateTime) -> NewSession {
    NewSession { id_hash: h(id), user_id, csrf_secret: vec![id; 32], expires_at, ip: None, user_agent: None }
}

fn login_state(id: u8, expires_at: OffsetDateTime) -> OAuthState {
    OAuthState { state_hash: h(id), purpose: OAuthPurpose::Login, user: None, pkce_verifier: None, return_to: None, expires_at }
}

fn bootstrap(id: u8, name: &str, scope: RunnerScope, expires_at: OffsetDateTime) -> NewBootstrapToken {
    NewBootstrapToken { token_hash: h(id), name: name.into(), scope, expires_at, created_by: Actor::Operator { os_user: "ops".into() } }
}

fn capabilities() -> Capabilities {
    Capabilities {
        arch: Arch::Aarch64,
        os: Os::MacOs,
        executor: ExecutorKind::Host,
        harnesses: Vec::new(),
        browser: false,
        slots: SlotCounts { light: 2, work: 1, verify: 1 },
    }
}

fn registration(id: RunnerId, name: &str, key: u8) -> RunnerRegistration {
    RunnerRegistration { id, name: name.into(), public_key: h(key), capabilities: capabilities() }
}

fn revocation(n: u8, expires_at: OffsetDateTime) -> Revocation {
    Revocation { jti: Jti([n; 16]), org_id: None, reason: "logout".into(), expires_at }
}

async fn sessions_round_trip_touch_and_expire(clock: &FakeClock, tokens: &dyn TokenStore, seed: &dyn Seed) {
    seed.user(user(1)).await;
    let t0 = clock.now();
    let hour = t0 + time::Duration::hours(1);
    let v4 = NewSession {
        ip: Some(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7))),
        user_agent: Some("Mozilla/5.0 (X11)".into()),
        ..session(1, user(1), hour)
    };
    let v6 = NewSession { ip: Some("2001:db8::1".parse().unwrap()), ..session(2, user(1), t0 + time::Duration::seconds(60)) };
    tokens.create_session(&v4).await.expect("create v4 session");
    tokens.create_session(&v6).await.expect("create v6 session");
    assert_eq!(
        tokens.create_session(&NewSession { user_agent: None, ..v4.clone() }).await,
        Err(TokenError::Conflict),
        "the hash is the key"
    );

    let stored = Session {
        id_hash: h(1),
        user_id: user(1),
        csrf_secret: vec![1; 32],
        created_at: t0,
        last_seen_at: t0,
        expires_at: hour,
        ip: v4.ip,
        user_agent: v4.user_agent.clone(),
    };
    assert_eq!(tokens.session(&h(1)).await, Ok(Some(stored.clone())));
    let v6_stored = tokens.session(&h(2)).await.unwrap().expect("v6 session");
    assert_eq!((v6_stored.ip, v6_stored.user_agent), (v6.ip, None), "IPv6 and an absent user agent round-trip");
    assert_eq!(tokens.session(&h(9)).await, Ok(None));

    clock.advance(secs(30));
    let t1 = clock.now();
    let later = t0 + time::Duration::hours(2);
    tokens.touch_session(&h(1), later).await.expect("touch");
    assert_eq!(
        tokens.session(&h(1)).await,
        Ok(Some(Session { last_seen_at: t1, expires_at: later, ..stored.clone() })),
        "touch moves last_seen_at and the expiry"
    );
    assert_eq!(tokens.touch_session(&h(9), later).await, Err(TokenError::NotFound));

    clock.advance(secs(30));
    assert_eq!(tokens.session(&h(2)).await, Ok(None), "a session is dead at its expiry instant");
    assert_eq!(tokens.touch_session(&h(2), later).await, Err(TokenError::NotFound), "an expired session cannot be revived");
    assert_eq!(tokens.session(&h(2)).await, Ok(None));

    tokens.delete_session(&h(1)).await.expect("delete");
    assert_eq!(tokens.session(&h(1)).await, Ok(None));
    assert_eq!(tokens.delete_session(&h(1)).await, Ok(()), "delete is idempotent");
    assert_eq!(tokens.touch_session(&h(1), later).await, Err(TokenError::NotFound));
}

async fn delete_user_sessions_removes_only_that_users(clock: &FakeClock, tokens: &dyn TokenStore, seed: &dyn Seed) {
    seed.user(user(1)).await;
    seed.user(user(2)).await;
    let t0 = clock.now();
    tokens.create_session(&session(1, user(1), t0 + time::Duration::hours(1))).await.unwrap();
    tokens.create_session(&session(2, user(1), t0 + time::Duration::seconds(10))).await.unwrap();
    tokens.create_session(&session(3, user(2), t0 + time::Duration::hours(1))).await.unwrap();
    clock.advance(secs(10));

    assert_eq!(tokens.delete_user_sessions(user(1)).await, Ok(2), "expired sessions of the user count too");
    assert_eq!(tokens.delete_user_sessions(user(1)).await, Ok(0));
    assert_eq!(tokens.session(&h(1)).await, Ok(None));
    assert_eq!(tokens.session(&h(3)).await.unwrap().map(|s| s.user_id), Some(user(2)), "other users keep their sessions");
    assert_eq!(tokens.delete_user_sessions(user(3)).await, Ok(0));
}

async fn oauth_states_are_one_time_and_ignore_expired(clock: &FakeClock, tokens: &dyn TokenStore, seed: &dyn Seed) {
    seed.org(org(1)).await;
    seed.user(user(1)).await;
    let t0 = clock.now();
    let ten_min = t0 + time::Duration::minutes(10);
    let login = OAuthState { pkce_verifier: Some("verifier-1".into()), return_to: Some("/orgs/acme".into()), ..login_state(1, ten_min) };
    let install = OAuthState {
        state_hash: h(2),
        purpose: OAuthPurpose::Install { org: org(1), provider: IntegrationKind::Slack },
        user: Some(user(1)),
        pkce_verifier: None,
        return_to: None,
        expires_at: ten_min,
    };
    let short = OAuthState {
        state_hash: h(3),
        purpose: OAuthPurpose::Install { org: org(1), provider: IntegrationKind::GitHubFix },
        expires_at: t0 + time::Duration::seconds(30),
        ..install.clone()
    };
    for state in [&login, &install, &short] {
        tokens.put_oauth_state(state).await.expect("put state");
    }
    assert_eq!(tokens.put_oauth_state(&login_state(1, ten_min)).await, Err(TokenError::Conflict), "the hash is the key");

    assert_eq!(tokens.take_oauth_state(&h(1)).await, Ok(Some(login.clone())));
    assert_eq!(tokens.take_oauth_state(&h(1)).await, Ok(None), "a state is taken once");
    assert_eq!(tokens.take_oauth_state(&h(9)).await, Ok(None));

    clock.advance(secs(30));
    assert_eq!(tokens.take_oauth_state(&h(3)).await, Ok(None), "an expired state is never returned");
    assert_eq!(tokens.take_oauth_state(&h(3)).await, Ok(None));
    assert_eq!(tokens.take_oauth_state(&h(2)).await, Ok(Some(install)), "install states keep org, provider and user");
    assert_eq!(tokens.take_oauth_state(&h(2)).await, Ok(None));

    tokens.put_oauth_state(&login).await.expect("a taken state's hash is free again");
}

async fn bootstrap_tokens_are_one_time(clock: &FakeClock, tokens: &dyn TokenStore, _seed: &dyn Seed) {
    let t0 = clock.now();
    let scope = RunnerScope::Orgs(vec![org(1), org(2)]);
    let token = bootstrap(1, "runner-a", scope.clone(), t0 + time::Duration::hours(1));
    tokens.create_bootstrap(&token).await.expect("create bootstrap");
    assert_eq!(tokens.create_bootstrap(&token).await, Err(TokenError::Conflict), "the hash is the key");

    let runner = tokens.register_runner(&h(1), &registration(runner_id(1), "runner-a", 1)).await.expect("register");
    let expected = Runner {
        id: runner_id(1),
        name: "runner-a".into(),
        scope,
        status: RunnerStatus::Active,
        public_key: h(1),
        generation: 1,
        registered_at: t0,
        last_seen_at: None,
    };
    assert_eq!(runner, expected, "a new name inserts the runner with the token's scope");
    assert_eq!(tokens.runner(runner_id(1)).await, Ok(expected.clone()));

    clock.advance(secs(1));
    assert_eq!(
        tokens.register_runner(&h(1), &registration(runner_id(2), "runner-a", 2)).await,
        Err(bootstrap_invalid()),
        "a used token admits no other key"
    );
    assert_eq!(tokens.runner(runner_id(2)).await, Err(TokenError::NotFound));
    assert_eq!(tokens.runner(runner_id(1)).await, Ok(expected), "the first runner is untouched");
    assert_eq!(tokens.register_runner(&h(9), &registration(runner_id(3), "runner-a", 3)).await, Err(bootstrap_invalid()), "unknown token");
}

async fn lost_response_retry_within_the_window_returns_the_same_runner(clock: &FakeClock, tokens: &dyn TokenStore, _seed: &dyn Seed) {
    let t0 = clock.now();
    tokens.create_bootstrap(&bootstrap(1, "runner-a", RunnerScope::Shared, t0 + time::Duration::hours(1))).await.unwrap();
    let first = tokens.register_runner(&h(1), &registration(runner_id(1), "runner-a", 1)).await.expect("register");

    clock.advance(secs(300));
    assert_eq!(
        tokens.register_runner(&h(1), &registration(runner_id(5), "runner-a", 1)).await,
        Ok(first.clone()),
        "a retry with the same key returns the same runner (its own id is ignored)"
    );
    clock.advance(BOOTSTRAP_RETRY_WINDOW.unsigned_abs() - secs(300));
    assert_eq!(
        tokens.register_runner(&h(1), &registration(runner_id(1), "runner-a", 1)).await,
        Ok(first.clone()),
        "the window runs 10 minutes from the first use; retries do not extend it"
    );
    clock.advance(secs(1));
    assert_eq!(
        tokens.register_runner(&h(1), &registration(runner_id(1), "runner-a", 1)).await,
        Err(bootstrap_invalid()),
        "past the window"
    );
    assert_eq!(tokens.runner(runner_id(1)).await, Ok(first), "retries never bump the generation");
    assert_eq!(tokens.runner(runner_id(5)).await, Err(TokenError::NotFound));
}

async fn bootstrap_checks_name_and_expiry(clock: &FakeClock, tokens: &dyn TokenStore, _seed: &dyn Seed) {
    let t0 = clock.now();
    let minute = t0 + time::Duration::seconds(60);
    tokens.create_bootstrap(&bootstrap(1, "runner-a", RunnerScope::Shared, minute)).await.unwrap();
    tokens.create_bootstrap(&bootstrap(2, "runner-b", RunnerScope::Shared, minute)).await.unwrap();

    assert_eq!(
        tokens.register_runner(&h(1), &registration(runner_id(1), "runner-b", 1)).await,
        Err(bootstrap_invalid()),
        "the name must equal the token's"
    );
    let a = tokens.register_runner(&h(1), &registration(runner_id(1), "runner-a", 1)).await.expect("a mismatch consumes nothing");
    assert_eq!(a.id, runner_id(1));

    clock.advance(secs(60));
    assert_eq!(
        tokens.register_runner(&h(2), &registration(runner_id(2), "runner-b", 2)).await,
        Err(bootstrap_invalid()),
        "a token is dead at its expiry instant"
    );
    assert_eq!(tokens.runner(runner_id(2)).await, Err(TokenError::NotFound));
    assert_eq!(
        tokens.register_runner(&h(1), &registration(runner_id(1), "runner-a", 1)).await,
        Err(bootstrap_invalid()),
        "an expired token is invalid even for a retry inside the window"
    );
}

async fn re_bootstrap_bumps_generation_and_keeps_the_id(clock: &FakeClock, tokens: &dyn TokenStore, seed: &dyn Seed) {
    seed.org(org(1)).await;
    let t0 = clock.now();
    let hour = t0 + time::Duration::hours(1);
    tokens.create_bootstrap(&bootstrap(1, "runner-a", RunnerScope::Shared, hour)).await.unwrap();
    let first = tokens.register_runner(&h(1), &registration(runner_id(1), "runner-a", 1)).await.unwrap();
    let revoked = tokens.set_runner_status(runner_id(1), RunnerStatus::Revoked).await.unwrap();
    assert_eq!(revoked.generation, 2);

    clock.advance(secs(60));
    tokens.create_bootstrap(&bootstrap(2, "runner-a", RunnerScope::Orgs(vec![org(1)]), hour)).await.unwrap();
    let again = tokens.register_runner(&h(2), &registration(runner_id(2), "runner-a", 2)).await.expect("re-bootstrap");
    let expected = Runner { status: RunnerStatus::Active, public_key: h(2), generation: 3, ..first.clone() };
    assert_eq!(again, expected, "same id, scope and registration time; new key, generation + 1, Active");
    assert_eq!(tokens.runner(runner_id(1)).await, Ok(expected.clone()));
    assert_eq!(tokens.runner(runner_id(2)).await, Err(TokenError::NotFound), "the registration's id is used only for new names");

    clock.advance(secs(60));
    assert_eq!(
        tokens.register_runner(&h(2), &registration(runner_id(2), "runner-a", 2)).await,
        Ok(expected),
        "a lost re-bootstrap response is retried without another bump"
    );
}

async fn new_runner_id_collision_is_a_conflict_and_consumes_nothing(clock: &FakeClock, tokens: &dyn TokenStore, _seed: &dyn Seed) {
    let hour = clock.now() + time::Duration::hours(1);
    tokens.create_bootstrap(&bootstrap(1, "runner-a", RunnerScope::Shared, hour)).await.unwrap();
    let a = tokens.register_runner(&h(1), &registration(runner_id(1), "runner-a", 1)).await.unwrap();
    tokens.create_bootstrap(&bootstrap(2, "runner-b", RunnerScope::Shared, hour)).await.unwrap();

    assert_eq!(
        tokens.register_runner(&h(2), &registration(runner_id(1), "runner-b", 2)).await,
        Err(TokenError::Conflict),
        "a new name with a taken id"
    );
    assert_eq!(tokens.runner(runner_id(1)).await, Ok(a));
    let b =
        tokens.register_runner(&h(2), &registration(runner_id(2), "runner-b", 2)).await.expect("the failed attempt left the token unused");
    assert_eq!((b.id, b.name.as_str(), b.generation), (runner_id(2), "runner-b", 1));
}

async fn runner_status_changes_and_revocation_bumps_generation(clock: &FakeClock, tokens: &dyn TokenStore, _seed: &dyn Seed) {
    assert_eq!(tokens.runner(runner_id(9)).await, Err(TokenError::NotFound));
    assert_eq!(tokens.set_runner_status(runner_id(9), RunnerStatus::Draining).await, Err(TokenError::NotFound));

    tokens.create_bootstrap(&bootstrap(1, "runner-a", RunnerScope::Shared, clock.now() + time::Duration::hours(1))).await.unwrap();
    let base = tokens.register_runner(&h(1), &registration(runner_id(1), "runner-a", 1)).await.unwrap();
    let steps = [
        (RunnerStatus::Draining, 1, "draining keeps the generation"),
        (RunnerStatus::Draining, 1, "same status: no change"),
        (RunnerStatus::Revoked, 2, "entering Revoked bumps the generation"),
        (RunnerStatus::Revoked, 2, "already Revoked: no second bump"),
        (RunnerStatus::Active, 2, "reactivation keeps the generation"),
        (RunnerStatus::Revoked, 3, "each entry into Revoked bumps once"),
    ];
    for (status, generation, why) in steps {
        let expected = Runner { status, generation, ..base.clone() };
        assert_eq!(tokens.set_runner_status(runner_id(1), status).await, Ok(expected.clone()), "{why}");
        assert_eq!(tokens.runner(runner_id(1)).await, Ok(expected), "{why}");
    }
}

async fn revocations_are_idempotent(clock: &FakeClock, tokens: &dyn TokenStore, _seed: &dyn Seed) {
    let hour = clock.now() + time::Duration::hours(1);
    let first = Revocation { org_id: Some(org(1)), ..revocation(1, hour) };
    assert_eq!(tokens.is_revoked(&first.jti).await, Ok(false));
    tokens.revoke(&first).await.expect("revoke");
    assert_eq!(tokens.is_revoked(&first.jti).await, Ok(true));
    assert_eq!(tokens.revoke(&Revocation { reason: "again".into(), org_id: None, ..first.clone() }).await, Ok(()), "revoke is idempotent");
    assert_eq!(tokens.is_revoked(&first.jti).await, Ok(true));
    assert_eq!(tokens.is_revoked(&Jti([2; 16])).await, Ok(false));
}

async fn purge_expired_caps_each_kind(clock: &FakeClock, tokens: &dyn TokenStore, seed: &dyn Seed) {
    seed.user(user(1)).await;
    let t0 = clock.now();
    let soon = t0 + time::Duration::seconds(10);
    let hour = t0 + time::Duration::hours(1);
    for (id, expires_at) in [(1, soon), (2, soon), (3, hour)] {
        tokens.create_session(&session(id, user(1), expires_at)).await.unwrap();
        tokens.put_oauth_state(&login_state(id, expires_at)).await.unwrap();
        tokens.create_bootstrap(&bootstrap(id, &format!("runner-{id}"), RunnerScope::Shared, expires_at)).await.unwrap();
        tokens.revoke(&revocation(id, expires_at)).await.unwrap();
    }
    let used = tokens.register_runner(&h(1), &registration(runner_id(1), "runner-1", 1)).await.expect("register before expiry");

    assert_eq!(tokens.purge_expired(10).await, Ok(0), "nothing has expired yet");
    clock.advance(secs(10));
    assert_eq!(tokens.purge_expired(1).await, Ok(4), "at most `limit` rows of each kind");
    assert_eq!(tokens.purge_expired(10).await, Ok(4));
    assert_eq!(tokens.purge_expired(10).await, Ok(0));

    assert_eq!(tokens.is_revoked(&Jti([1; 16])).await, Ok(false), "expired revocations are gone");
    assert_eq!(tokens.is_revoked(&Jti([3; 16])).await, Ok(true));
    assert_eq!(tokens.session(&h(3)).await.unwrap().map(|s| s.id_hash), Some(h(3)));
    assert_eq!(tokens.take_oauth_state(&h(3)).await.unwrap().map(|s| s.state_hash), Some(h(3)));
    let live = tokens.register_runner(&h(3), &registration(runner_id(3), "runner-3", 3)).await.expect("the live token still works");
    assert_eq!(live.id, runner_id(3));
    assert_eq!(tokens.runner(runner_id(1)).await, Ok(used), "purging a used token keeps its runner");

    for id in [1, 2] {
        tokens.create_session(&session(id, user(1), hour)).await.expect("a purged session's hash is free");
        tokens.put_oauth_state(&login_state(id, hour)).await.expect("a purged state's hash is free");
        tokens.create_bootstrap(&bootstrap(id, "runner-x", RunnerScope::Shared, hour)).await.expect("a purged token's hash is free");
    }
}

async fn one_bootstrap_token_registers_one_runner_under_concurrency(clock: &FakeClock, tokens: &dyn TokenStore, _seed: &dyn Seed) {
    tokens.create_bootstrap(&bootstrap(1, "runner-a", RunnerScope::Shared, clock.now() + time::Duration::hours(1))).await.unwrap();
    let regs: Vec<RunnerRegistration> = (1..=8u8).map(|n| registration(runner_id(u128::from(n)), "runner-a", n)).collect();
    let hash = h(1);
    let results = join_all(regs.iter().map(|reg| tokens.register_runner(&hash, reg))).await;

    let winners: Vec<&Runner> = results.iter().filter_map(|r| r.as_ref().ok()).collect();
    assert_eq!(winners.len(), 1, "exactly one concurrent registration wins: {results:?}");
    assert!(results.iter().filter(|r| r.is_err()).all(|r| *r == Err(bootstrap_invalid())), "the others see a used token: {results:?}");
    let winner = winners[0].clone();
    let reg = regs.iter().find(|r| r.id == winner.id).expect("the winner is one of the registrations");
    assert_eq!((winner.public_key, winner.generation), (reg.public_key, 1));
    assert_eq!(tokens.runner(winner.id).await, Ok(winner.clone()));
    for loser in regs.iter().filter(|r| r.id != winner.id) {
        assert_eq!(tokens.runner(loser.id).await, Err(TokenError::NotFound));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{MemTokenStore, NoSeed};

    #[tokio::test]
    async fn mem_token_store_passes_the_contract() {
        contract_token_store(|clock| async move { TokenFixture { tokens: Arc::new(MemTokenStore::new(clock)), seed: Arc::new(NoSeed) } })
            .await;
    }
}
