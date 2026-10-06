//! `KnowledgeScope` and the Knowledge MCP token check (knowledge.md "Scope", server.md UC9).
//!
//! A `knowledge_token` (`mgk_`) names one org, run, attempt, repo and sha plus the memory cutoff
//! and disabled layers. [`AuthorizeKnowledge`] verifies it (signature, kind, time, `jti` not
//! revoked via `TokenService`), then checks the run: not terminal and still at the token's attempt.
//! Run state is cached for 10 s per instance; a terminal run stays rejected.

use std::collections::HashMap;
use std::sync::Arc;

use mage_core::domain::{CommitSha, Layer, OrgId, RepoId, RepoVersionKey, RunId};
use mage_core::ports::clock::Clock;
use mage_core::wire::token::{MemoryCutoff, TokenClaims, TokenKind, TokenScope};
use parking_lot::Mutex;
use time::OffsetDateTime;

use crate::app::auth::tokens::TokenService;
use crate::error::AppError;
use crate::ports::runs::RunStore;

/// What one Knowledge MCP call may read. Every store call passes `org`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnowledgeScope {
    pub org: OrgId,
    pub run: RunId,
    pub repo: RepoId,
    pub sha: CommitSha,
    pub memory: MemoryCutoff,
    /// Empty in normal runs; eval ablation. Treated as not Ready everywhere.
    pub disabled_layers: Vec<Layer>,
}

impl KnowledgeScope {
    pub fn version(&self) -> RepoVersionKey {
        RepoVersionKey::new(self.repo, self.sha)
    }

    pub fn layer_disabled(&self, layer: Layer) -> bool {
        self.disabled_layers.contains(&layer)
    }

    /// The scope a `Knowledge` token grants, with the attempt it is bound to.
    pub fn from_claims(claims: &TokenClaims) -> Option<(KnowledgeScope, u16)> {
        match &claims.scope {
            TokenScope::Knowledge { org, run, attempt, repo, sha, memory, disabled_layers } => Some((
                KnowledgeScope { org: *org, run: *run, repo: *repo, sha: *sha, memory: memory.clone(), disabled_layers: disabled_layers.clone() },
                *attempt,
            )),
            _ => None,
        }
    }
}

/// How long a run's state is trusted before it is read again.
const RUN_STATE_TTL: time::Duration = time::Duration::seconds(10);
/// Cache entries kept before stale ones are pruned.
const RUN_CACHE_PRUNE_AT: usize = 4096;

#[derive(Clone, Copy)]
struct RunState {
    fetched_at: OffsetDateTime,
    terminal: bool,
    attempt: u16,
}

/// UC9 step 1-2: bearer `mgk_…` → [`KnowledgeScope`]. Every refusal is `Unauthorized` (the HTTP
/// layer answers 401 before MCP handling); store outages are `Unavailable`.
pub struct AuthorizeKnowledge {
    tokens: Arc<TokenService>,
    runs: Arc<dyn RunStore>,
    clock: Arc<dyn Clock>,
    cache: Mutex<HashMap<(OrgId, RunId), RunState>>,
}

impl AuthorizeKnowledge {
    pub fn new(tokens: Arc<TokenService>, runs: Arc<dyn RunStore>, clock: Arc<dyn Clock>) -> Self {
        Self { tokens, runs, clock, cache: Mutex::new(HashMap::new()) }
    }

    pub async fn authorize(&self, bearer: &str) -> Result<KnowledgeScope, AppError> {
        let claims = self.tokens.verify(bearer, TokenKind::Knowledge).await?;
        let (scope, attempt) = KnowledgeScope::from_claims(&claims).ok_or(AppError::Unauthorized)?;
        let run = self.run_state(scope.org, scope.run).await?;
        if run.terminal || run.attempt != attempt {
            return Err(AppError::Unauthorized);
        }
        Ok(scope)
    }

    async fn run_state(&self, org: OrgId, run: RunId) -> Result<RunState, AppError> {
        let now = self.clock.now();
        if let Some(state) = self.cache.lock().get(&(org, run)).copied() {
            if state.terminal || now - state.fetched_at < RUN_STATE_TTL {
                return Ok(state);
            }
        }
        let record = match self.runs.get(org, run).await {
            Ok(record) => record,
            Err(e) => {
                return Err(match AppError::from(e) {
                    AppError::NotFound(_) => AppError::Unauthorized,
                    other => other,
                });
            }
        };
        let state = RunState { fetched_at: now, terminal: record.run.status.is_terminal(), attempt: record.run.attempt };
        let mut cache = self.cache.lock();
        if cache.len() >= RUN_CACHE_PRUNE_AT {
            cache.retain(|_, s| now - s.fetched_at < RUN_STATE_TTL);
        }
        cache.insert((org, run), state);
        Ok(state)
    }
}
