//! `RunPolicy` (runs.md "RunPolicy"): lease timing, attempts, waits and defaults, deserialized
//! from the server config section `[server.runs]`.

use std::time::Duration;

use serde::Deserialize;

use mage_core::domain::{Budget, RunKind, VerifySpec};
use mage_core::wire::runner::{CredentialModeKind, ExecutorKind};

use crate::ports::runs::DueThresholds;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RunPolicy {
    /// Public base URL of the server (`{public_url}/mcp/knowledge`, `{public_url}/llm/anthropic`).
    pub public_url: String,
    pub lease_ttl_secs: u64,
    pub heartbeat_every_secs: u64,
    pub max_lease_attempts: u16,
    /// Cap of `POST /lease` and `GET /runs/{id}/commands` waits (Cloud Run: 25 s).
    pub long_poll_max_secs: u64,
    /// After this, any capable runner may take a Verify run.
    pub verify_affinity_wait_secs: u64,
    /// Work-class runs per repo with a held lease (v1: 1).
    pub repo_exec_concurrency: u16,
    pub queue_max_wait_secs: u64,
    pub gate_wait_max_secs: u64,
    /// Extra Verify runs per gate attempt after infrastructure errors.
    pub verify_infra_retries: u8,
    pub repro_repetitions: u8,
    /// Verify budget wall time (Fix gate verify: `verify_fix_wall_secs`).
    pub verify_wall_secs: u64,
    pub verify_fix_wall_secs: u64,
    /// `LocalLogin` in dev, `Gateway` in production.
    pub credential_mode: CredentialModeKind,
    /// v1: `Host` (no per-repo trust tiers yet).
    pub executor: ExecutorKind,
    /// Model of Learn/Diagnose runs (and of any run created without one).
    pub default_model: String,
    /// Claude Code's small/fast model: added to gateway tokens of Claude Code runs.
    pub claude_code_small_model: Option<String>,
    /// Runners older than this are told to upgrade (`RegistrationAck.min_version`).
    pub min_runner_version: String,
    /// Runner token lifetime.
    pub runner_token_ttl_secs: u64,
    /// Signed URL lifetimes.
    pub upload_url_ttl_secs: u64,
    pub download_url_ttl_secs: u64,
    pub bundle_url_ttl_secs: u64,
    /// SSE run streams close (with `retry`) after this.
    pub sse_max_secs: u64,
    /// Reaper job period.
    pub reap_every_secs: u64,
}

impl Default for RunPolicy {
    fn default() -> Self {
        RunPolicy {
            public_url: "http://localhost:8080".into(),
            lease_ttl_secs: 60,
            heartbeat_every_secs: 15,
            max_lease_attempts: 3,
            long_poll_max_secs: 25,
            verify_affinity_wait_secs: 30,
            repo_exec_concurrency: 1,
            queue_max_wait_secs: 2 * 3600,
            gate_wait_max_secs: 30 * 60,
            verify_infra_retries: 2,
            repro_repetitions: 2,
            verify_wall_secs: 15 * 60,
            verify_fix_wall_secs: 30 * 60,
            credential_mode: CredentialModeKind::Gateway,
            executor: ExecutorKind::Host,
            default_model: "claude-sonnet-4-5".into(),
            claude_code_small_model: Some("claude-haiku-4-5".into()),
            min_runner_version: "0.1.0".into(),
            runner_token_ttl_secs: 15 * 60,
            upload_url_ttl_secs: 15 * 60,
            download_url_ttl_secs: 15 * 60,
            bundle_url_ttl_secs: 30 * 60,
            sse_max_secs: 10 * 60,
            reap_every_secs: 15,
        }
    }
}

impl RunPolicy {
    pub fn lease_ttl(&self) -> Duration {
        Duration::from_secs(self.lease_ttl_secs)
    }

    pub fn heartbeat_every(&self) -> Duration {
        Duration::from_secs(self.heartbeat_every_secs)
    }

    pub fn long_poll_max(&self) -> Duration {
        Duration::from_secs(self.long_poll_max_secs)
    }

    pub fn verify_affinity_wait(&self) -> Duration {
        Duration::from_secs(self.verify_affinity_wait_secs)
    }

    pub fn due_thresholds(&self) -> DueThresholds {
        DueThresholds {
            queue_max_wait: Duration::from_secs(self.queue_max_wait_secs),
            gate_wait_max: Duration::from_secs(self.gate_wait_max_secs),
        }
    }

    /// Verify run budget: wall only (`max_turns`/`max_cost` do not apply), no gates.
    pub fn verify_budget(&self, spec: &VerifySpec) -> Budget {
        let wall = match spec {
            VerifySpec::Fix { .. } => self.verify_fix_wall_secs,
            _ => self.verify_wall_secs,
        };
        Budget { max_wall: Duration::from_secs(wall), max_turns: u32::MAX, max_cost_micros: u64::MAX, max_gate_attempts: 0 }
    }

    /// Lifetime of run-bound tokens: `max_wall + max_gate_attempts x verify max_wall + 15 min`,
    /// capped at 24 h.
    pub fn run_token_ttl(&self, kind: RunKind, budget: &Budget) -> Duration {
        let verify_wall = if kind == RunKind::Fix { self.verify_fix_wall_secs } else { self.verify_wall_secs };
        let gates = Duration::from_secs(verify_wall).saturating_mul(u32::from(budget.max_gate_attempts));
        let ttl = budget.max_wall.saturating_add(gates).saturating_add(Duration::from_secs(15 * 60));
        ttl.min(Duration::from_secs(24 * 3600))
    }

    pub fn knowledge_url(&self) -> String {
        format!("{}/mcp/knowledge", self.public_url.trim_end_matches('/'))
    }

    pub fn gateway_url(&self) -> String {
        format!("{}/llm/anthropic", self.public_url.trim_end_matches('/'))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_token_ttl_covers_gates_and_is_capped() {
        let p = RunPolicy::default();
        let diag = Budget { max_wall: Duration::from_secs(45 * 60), max_turns: 1, max_cost_micros: 1, max_gate_attempts: 2 };
        // 45 min + 2 x 15 min + 15 min
        assert_eq!(p.run_token_ttl(RunKind::Diagnose, &diag), Duration::from_secs(90 * 60));
        // Fix gates use the 30 min Fix verify wall
        assert_eq!(p.run_token_ttl(RunKind::Fix, &Budget { max_gate_attempts: 3, ..diag }), Duration::from_secs((45 + 90 + 15) * 60));
        let huge = Budget { max_wall: Duration::from_secs(30 * 3600), ..diag };
        assert_eq!(p.run_token_ttl(RunKind::Learn, &huge), Duration::from_secs(24 * 3600));
    }

    #[test]
    fn config_overrides_parse_and_unknown_keys_fail() {
        let p: RunPolicy = toml::from_str("lease_ttl_secs = 30\ncredential_mode = \"local_login\"").unwrap();
        assert_eq!(p.lease_ttl(), Duration::from_secs(30));
        assert_eq!(p.credential_mode, CredentialModeKind::LocalLogin);
        assert_eq!(p.heartbeat_every_secs, 15);
        assert!(toml::from_str::<RunPolicy>("lease_ttl = 30").is_err());
    }
}
