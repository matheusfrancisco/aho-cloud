//! `FakeRunner`: `RunnerDeps` wired over the role fakes (`FakeServerLink`, `FakeExecutor`,
//! `MemRunnerState`, scripted `FakeHarness` behind a start gate, `FakeRunMcpHost`,
//! `FakeBrowserRunner`) with the real `VerifyRunner` and `ScipStep`, plus assignment builders.
//! Used by the run task, daemon and lease loop tests.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mage_core::domain::recipe::RecipeSpec;
use mage_core::domain::{Budget, CommitSha, HarnessKind, OrgId, RepoId, RunId, RunKind, VerifySpec};
use mage_core::ports::{Clock, SystemClock, UuidV7Gen};
use mage_core::wire::runner::{
    CredentialModeKind, DownloadRef, GatewayAccess, HarnessAssignment, KnowledgeAccess, LeaseTerms, RecipeRef, RepoRef, RunAssignment,
};
use tokio::sync::Semaphore;

use crate::app::runner::scip::{ScipConfig, ScipStep};
use crate::app::runner::task::{RunnerDeps, TaskConfig};
use crate::app::runner::tools::RunToolsConfig;
use crate::app::runner::verify::{VerifyConfig, VerifyRunner};
use crate::ports::executor::Env;
use crate::ports::harness::{Harness, HarnessError, HarnessInfo, HarnessSession, HarnessSpec};
use crate::testing::browser::FakeBrowserRunner;
use crate::testing::executor::FakeExecutor;
use crate::testing::harness::FakeHarness;
use crate::testing::link::FakeServerLink;
use crate::testing::run_mcp::FakeRunMcpHost;
use crate::testing::runner_state::MemRunnerState;

/// Recipe TOML of the built assignments.
pub const RECIPE: &str = "version = 1\n";
pub const KNOWLEDGE_TOKEN: &str = "mgk_knowledge-token-0123456789";
pub const GATEWAY_TOKEN: &str = "mgg_gateway-token-0123456789";

/// `FakeHarness` whose `start` waits for a permit of `gate`, so a test can act (e.g. submit
/// through the Run MCP) before the harness process exists.
pub struct GatedHarness {
    pub inner: Arc<FakeHarness>,
    pub gate: Arc<Semaphore>,
}

#[async_trait]
impl Harness for GatedHarness {
    fn kind(&self) -> HarnessKind {
        self.inner.kind()
    }

    async fn probe(&self) -> Result<HarnessInfo, HarnessError> {
        self.inner.probe().await
    }

    async fn start(&self, env: Arc<dyn Env>, spec: HarnessSpec) -> Result<Box<dyn HarnessSession>, HarnessError> {
        let _permit = self.gate.acquire().await.map_err(|_| HarnessError::Unavailable("start gate closed".into()))?;
        self.inner.start(env, spec).await
    }
}

pub struct FakeRunner {
    pub link: Arc<FakeServerLink>,
    pub executor: Arc<FakeExecutor>,
    pub state: Arc<MemRunnerState>,
    /// The omp harness (scripts via `push_script`).
    pub harness: Arc<FakeHarness>,
    /// Permits for `Harness::start`; open unless built with [`FakeRunner::gated`].
    pub start_gate: Arc<Semaphore>,
    pub mcp: Arc<FakeRunMcpHost>,
    pub browser: Arc<FakeBrowserRunner>,
    pub deps: Arc<RunnerDeps>,
}

impl FakeRunner {
    /// Runner data dir `data_dir`; close grace and final flush 5 s.
    pub fn new(data_dir: &Path) -> FakeRunner {
        Self::build(data_dir, Semaphore::MAX_PERMITS)
    }

    /// Like [`FakeRunner::new`], but `Harness::start` blocks until `start_gate` gets a permit.
    pub fn gated(data_dir: &Path) -> FakeRunner {
        Self::build(data_dir, 0)
    }

    fn build(data_dir: &Path, permits: usize) -> FakeRunner {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let state = MemRunnerState::new(clock.clone());
        let executor = FakeExecutor::with_state(state.clone());
        let link = FakeServerLink::new();
        // Heartbeat answers keep the fast 50 ms cadence of the built assignments.
        link.set_terms(LeaseTerms { ttl: Duration::from_secs(60), heartbeat_every: Duration::from_millis(50) });
        let harness = Arc::new(FakeHarness::new(HarnessKind::Omp).with_clock(clock.clone()));
        let start_gate = Arc::new(Semaphore::new(permits));
        let mcp = Arc::new(FakeRunMcpHost::new());
        let browser = Arc::new(FakeBrowserRunner::new());
        let verify = Arc::new(VerifyRunner::new(executor.clone(), browser.clone(), link.clone(), clock.clone(), VerifyConfig::default()));
        let scip = Arc::new(ScipStep::new(executor.clone(), link.clone(), ScipConfig::default()));
        let mut config = TaskConfig::new(data_dir.to_owned());
        config.close_grace_secs = 5;
        config.final_flush_secs = 5;
        let deps = Arc::new(RunnerDeps {
            link: link.clone(),
            executor: executor.clone(),
            state: state.clone(),
            harnesses: vec![Arc::new(GatedHarness { inner: harness.clone(), gate: start_gate.clone() })],
            mcp_host: mcp.clone(),
            browser: browser.clone(),
            verify,
            scip,
            clock,
            ids: Arc::new(UuidV7Gen),
            tools: RunToolsConfig::default(),
            config,
        });
        FakeRunner { link, executor, state, harness, start_gate, mcp, browser, deps }
    }

    /// Attempt 1 of a new run of `kind`: the source bundle is served by the fake link (none for
    /// Triage); Diagnose/Fix/Verify carry [`RECIPE`]; Verify is `VerifySpec::Boot` without
    /// harness, knowledge or gateway; harness runs use omp in Gateway mode. Heartbeats every
    /// 50 ms keep tests short; the lease TTL is 60 s.
    pub fn assignment(&self, kind: RunKind) -> RunAssignment {
        let repo_id = RepoId::from(uuid::Uuid::now_v7());
        let sha: CommitSha = "ab".repeat(20).parse().expect("sha");
        let triage = kind == RunKind::Triage;
        let verify = kind == RunKind::Verify;
        let source = (!triage).then(|| self.link.add_blob(DownloadRef::SourceBundle { repo_id, sha }, None, b"bundle".to_vec()));
        let recipe = matches!(kind, RunKind::Diagnose | RunKind::Fix | RunKind::Verify)
            .then(|| RecipeRef { hash: RecipeSpec::parse(RECIPE).expect("recipe").hash(), toml: RECIPE.into() });
        RunAssignment {
            run_id: RunId::from(uuid::Uuid::now_v7()),
            org_id: OrgId::from(uuid::Uuid::now_v7()),
            attempt: 1,
            kind,
            lease: LeaseTerms { ttl: Duration::from_secs(60), heartbeat_every: Duration::from_millis(50) },
            repo: (!triage).then(|| RepoRef { repo_id, owner: "acme".into(), name: "shop".into() }),
            sha: (!triage).then_some(sha),
            source,
            recipe,
            harness: (!verify).then(|| HarnessAssignment {
                kind: HarnessKind::Omp,
                model: "claude-sonnet-4-5".into(),
                credential_mode: CredentialModeKind::Gateway,
                prompt: "Investigate the cause.".into(),
                system_append: "Be precise.".into(),
            }),
            verify: verify.then_some(VerifySpec::Boot),
            downloads: Vec::new(),
            knowledge: (!verify)
                .then(|| KnowledgeAccess { url: "https://mage.example/mcp/knowledge".into(), knowledge_token: KNOWLEDGE_TOKEN.into() }),
            gateway: (!verify)
                .then(|| GatewayAccess { base_url: "https://mage.example/llm/anthropic".into(), gateway_token: GATEWAY_TOKEN.into() }),
            budget: Budget { max_wall: Duration::from_secs(2700), max_turns: 150, max_cost_micros: 15_000_000, max_gate_attempts: 2 },
            prior_feedback: Vec::new(),
            fix: None,
            trace: None,
        }
    }
}
