//! `MemTokenStore`: in-memory `TokenStore` with the same expiry and one-time rules as the pg
//! adapter (time from the `Clock`).

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use mage_core::domain::{RunnerId, UserId};
use mage_core::ports::clock::Clock;
use parking_lot::Mutex;
use time::OffsetDateTime;

use crate::ports::tokens::{
    BOOTSTRAP_RETRY_WINDOW, Jti, NewBootstrapToken, NewSession, OAuthState, Revocation, Runner, RunnerRegistration, RunnerStatus, Session,
    TokenError, TokenStore, bootstrap_invalid,
};

pub struct MemTokenStore {
    clock: Arc<dyn Clock>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    sessions: BTreeMap<[u8; 32], Session>,
    oauth: BTreeMap<[u8; 32], OAuthState>,
    bootstrap: BTreeMap<[u8; 32], BootstrapRow>,
    runners: BTreeMap<RunnerId, Runner>,
    revocations: BTreeMap<Jti, Revocation>,
}

struct BootstrapRow {
    token: NewBootstrapToken,
    used_at: Option<OffsetDateTime>,
    runner: Option<RunnerId>,
}

impl MemTokenStore {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self { clock, state: Mutex::new(State::default()) }
    }
}

/// Removes up to `limit` entries matching `expired`.
fn purge<K: Ord + Clone, V>(map: &mut BTreeMap<K, V>, limit: usize, expired: impl Fn(&V) -> bool) -> u64 {
    let keys: Vec<K> = map.iter().filter(|(_, v)| expired(v)).map(|(k, _)| k.clone()).take(limit).collect();
    for key in &keys {
        map.remove(key);
    }
    keys.len() as u64
}

#[async_trait]
impl TokenStore for MemTokenStore {
    async fn create_session(&self, s: &NewSession) -> Result<(), TokenError> {
        let now = self.clock.now();
        let mut state = self.state.lock();
        if state.sessions.contains_key(&s.id_hash) {
            return Err(TokenError::Conflict);
        }
        state.sessions.insert(s.id_hash, Session {
            id_hash: s.id_hash,
            user_id: s.user_id,
            csrf_secret: s.csrf_secret.clone(),
            created_at: now,
            last_seen_at: now,
            expires_at: s.expires_at,
            ip: s.ip,
            user_agent: s.user_agent.clone(),
        });
        Ok(())
    }

    async fn session(&self, id_hash: &[u8; 32]) -> Result<Option<Session>, TokenError> {
        let now = self.clock.now();
        Ok(self.state.lock().sessions.get(id_hash).filter(|s| s.expires_at > now).cloned())
    }

    async fn touch_session(&self, id_hash: &[u8; 32], expires_at: OffsetDateTime) -> Result<(), TokenError> {
        let now = self.clock.now();
        let mut state = self.state.lock();
        let session = state.sessions.get_mut(id_hash).filter(|s| s.expires_at > now).ok_or(TokenError::NotFound)?;
        session.last_seen_at = now;
        session.expires_at = expires_at;
        Ok(())
    }

    async fn delete_session(&self, id_hash: &[u8; 32]) -> Result<(), TokenError> {
        self.state.lock().sessions.remove(id_hash);
        Ok(())
    }

    async fn delete_user_sessions(&self, user: UserId) -> Result<u64, TokenError> {
        let mut state = self.state.lock();
        let before = state.sessions.len();
        state.sessions.retain(|_, s| s.user_id != user);
        Ok((before - state.sessions.len()) as u64)
    }

    async fn put_oauth_state(&self, s: &OAuthState) -> Result<(), TokenError> {
        let mut state = self.state.lock();
        if state.oauth.contains_key(&s.state_hash) {
            return Err(TokenError::Conflict);
        }
        state.oauth.insert(s.state_hash, s.clone());
        Ok(())
    }

    async fn take_oauth_state(&self, state_hash: &[u8; 32]) -> Result<Option<OAuthState>, TokenError> {
        let now = self.clock.now();
        let mut state = self.state.lock();
        match state.oauth.get(state_hash) {
            Some(s) if s.expires_at > now => Ok(state.oauth.remove(state_hash)),
            _ => Ok(None),
        }
    }

    async fn create_bootstrap(&self, b: &NewBootstrapToken) -> Result<(), TokenError> {
        let mut state = self.state.lock();
        if state.bootstrap.contains_key(&b.token_hash) {
            return Err(TokenError::Conflict);
        }
        state.bootstrap.insert(b.token_hash, BootstrapRow { token: b.clone(), used_at: None, runner: None });
        Ok(())
    }

    async fn register_runner(&self, bootstrap_hash: &[u8; 32], reg: &RunnerRegistration) -> Result<Runner, TokenError> {
        let now = self.clock.now();
        let mut state = self.state.lock();
        let row = state.bootstrap.get(bootstrap_hash).ok_or_else(bootstrap_invalid)?;
        if row.token.expires_at <= now || row.token.name != reg.name {
            return Err(bootstrap_invalid());
        }
        if let Some(used_at) = row.used_at {
            let retry = row.runner.and_then(|id| state.runners.get(&id)).filter(|r| r.public_key == reg.public_key && now - used_at <= BOOTSTRAP_RETRY_WINDOW);
            return retry.cloned().ok_or_else(bootstrap_invalid);
        }
        let scope = row.token.scope.clone();
        let existing = state.runners.values().find(|r| r.name == reg.name).map(|r| r.id);
        let runner = match existing {
            Some(id) => {
                let runner = state.runners.get_mut(&id).ok_or(TokenError::NotFound)?;
                runner.public_key = reg.public_key;
                runner.generation += 1;
                runner.status = RunnerStatus::Active;
                runner.clone()
            }
            None => {
                if state.runners.contains_key(&reg.id) {
                    return Err(TokenError::Conflict);
                }
                let runner = Runner {
                    id: reg.id,
                    name: reg.name.clone(),
                    scope,
                    status: RunnerStatus::Active,
                    public_key: reg.public_key,
                    generation: 1,
                    registered_at: now,
                    last_seen_at: None,
                };
                state.runners.insert(reg.id, runner.clone());
                runner
            }
        };
        let row = state.bootstrap.get_mut(bootstrap_hash).ok_or_else(bootstrap_invalid)?;
        row.used_at = Some(now);
        row.runner = Some(runner.id);
        Ok(runner)
    }

    async fn runner(&self, id: RunnerId) -> Result<Runner, TokenError> {
        self.state.lock().runners.get(&id).cloned().ok_or(TokenError::NotFound)
    }

    async fn set_runner_status(&self, id: RunnerId, status: RunnerStatus) -> Result<Runner, TokenError> {
        let mut state = self.state.lock();
        let runner = state.runners.get_mut(&id).ok_or(TokenError::NotFound)?;
        if status == RunnerStatus::Revoked && runner.status != RunnerStatus::Revoked {
            runner.generation += 1;
        }
        runner.status = status;
        Ok(runner.clone())
    }

    async fn revoke(&self, r: &Revocation) -> Result<(), TokenError> {
        self.state.lock().revocations.entry(r.jti).or_insert_with(|| r.clone());
        Ok(())
    }

    async fn is_revoked(&self, jti: &Jti) -> Result<bool, TokenError> {
        Ok(self.state.lock().revocations.contains_key(jti))
    }

    async fn purge_expired(&self, limit: u32) -> Result<u64, TokenError> {
        let now = self.clock.now();
        let limit = limit as usize;
        let mut state = self.state.lock();
        let mut n = purge(&mut state.sessions, limit, |s| s.expires_at <= now);
        n += purge(&mut state.oauth, limit, |s| s.expires_at <= now);
        n += purge(&mut state.bootstrap, limit, |b| b.token.expires_at <= now);
        n += purge(&mut state.revocations, limit, |r| r.expires_at <= now);
        Ok(n)
    }
}
