//! Verify use case over fakes (executor.md "Testing": Repro fresh fork per repetition and
//! observations per attempt; Fix `PatchRejected` stops early, the rebase path uploads the rebased
//! patch, Fix checks; TestBaseline; `Errored` on fork failure).

use std::sync::Arc;
use std::time::Duration;

use mage_core::domain::fix::{ApplyMode, RegressionFile};
use mage_core::domain::recipe::RecipeSpec;
use mage_core::domain::{
    ArtifactId, AttemptKind, Budget, CommitSha, FailureSignature, Matcher, OrgId, RepoId, RepoPath, RunId, RunKind, SignatureKind,
    VerifyOutcome, VerifySpec,
};
use mage_core::testing::FakeClock;
use mage_core::wire::runner::{Download, DownloadRef, LeaseTerms, RunAssignment};
use sha2::{Digest, Sha256};

use super::*;
use crate::ports::executor::{
    ApplyOutcome, Baseline, BundleSource, Command, DiffOptions, EnvPath, ExecError, Executor, Exit, ForkOptions, ForkPurpose,
    EgressProfile, NullObserver, PrepareRequest, RejectReason, RelPath,
};
use crate::testing::browser::{FakeBrowserRunner, failed, passed};
use crate::testing::executor::{FakeEnv, FakeExecutor, outcome};
use crate::testing::link::FakeServerLink;

const RECIPE: &str = r#"version = 1
ports = ["web"]

[services.web]
run = "node server.js"
ports = ["web"]
[services.web.health]
http = "http://127.0.0.1:${port.web}/"

[test]
run = "npm test"
timeout_s = 600

[fix]
lint = "npm run lint"
"#;

const SCRIPT: ArtifactId = ArtifactId::from_uuid(uuid::Uuid::from_u128(0x51));
const PATCH: ArtifactId = ArtifactId::from_uuid(uuid::Uuid::from_u128(0x52));

struct H {
    exec: Arc<FakeExecutor>,
    link: Arc<FakeServerLink>,
    browser: Arc<FakeBrowserRunner>,
    runner: VerifyRunner,
    base: BaseRef,
    outbox: tempfile::TempDir,
}

async fn harness() -> H {
    let exec = FakeExecutor::new();
    exec.add_bundle("bundle://v", vec![("src/app.ts".into(), b"a\n".to_vec()), ("package.json".into(), b"{}\n".to_vec())]);
    let req = PrepareRequest {
        org_id: OrgId::from_uuid(uuid::Uuid::from_u128(1)),
        repo_id: RepoId::from_uuid(uuid::Uuid::from_u128(2)),
        sha: CommitSha::from_bytes([7; 20]),
        recipe: Some(RecipeSpec::parse(RECIPE).unwrap()),
        source_bundle: BundleSource { url: "bundle://v".into(), sha256: [0; 32], bytes: 0 },
    };
    let base = exec.prepare(req, &NullObserver).await.unwrap();
    let link = FakeServerLink::new();
    let browser = Arc::new(FakeBrowserRunner::new());
    let config = VerifyConfig { media: MediaConfig { enabled: false, ..MediaConfig::default() }, ..VerifyConfig::default() };
    let runner = VerifyRunner::new(exec.clone(), browser.clone(), link.clone(), Arc::new(FakeClock::new()), config);
    H { exec, link, browser, runner, base, outbox: tempfile::tempdir().unwrap() }
}

fn assignment(spec: VerifySpec, downloads: Vec<Download>) -> RunAssignment {
    RunAssignment {
        run_id: RunId::from_uuid(uuid::Uuid::from_u128(0x77)),
        org_id: OrgId::from_uuid(uuid::Uuid::from_u128(1)),
        attempt: 1,
        kind: RunKind::Verify,
        lease: LeaseTerms { ttl: Duration::from_secs(60), heartbeat_every: Duration::from_secs(15) },
        repo: None,
        sha: Some(CommitSha::from_bytes([7; 20])),
        source: None,
        recipe: None,
        harness: None,
        verify: Some(spec),
        downloads,
        knowledge: None,
        gateway: None,
        budget: Budget { max_wall: Duration::from_secs(1800), max_turns: 0, max_cost_micros: 0, max_gate_attempts: 3 },
        prior_feedback: Vec::new(),
        fix: None,
        trace: None,
    }
}

fn sig(kind: SignatureKind, value: &str) -> FailureSignature {
    FailureSignature { kind, value: value.into(), matcher: Matcher::Exact }
}

fn is_script(spec: &ExecSpec) -> bool {
    matches!(&spec.command, Command::Argv(argv) if argv.first().map(String::as_str) == Some("bash"))
}

impl H {
    async fn run(&self, a: &RunAssignment) -> VerifyResult {
        self.runner.run(a, &self.base, self.outbox.path()).await
    }

    fn artifact_names(&self) -> Vec<String> {
        self.link.artifacts().into_iter().map(|a| a.name).collect()
    }

    /// The patch an agent would submit: `src/app.ts` a → b, diffed on a scratch fork.
    async fn patch(&self) -> Vec<u8> {
        let opts = ForkOptions { run_id: RunId::from_uuid(uuid::Uuid::from_u128(0x99)), purpose: ForkPurpose::Session, egress: EgressProfile::Registries };
        let env = self.exec.fork(&self.base, opts).await.unwrap();
        let fake = self.exec.envs().pop().unwrap();
        fake.set_file(&EnvPath::ws("src/app.ts").unwrap(), b"b\n");
        let out = env.diff_against_base(&DiffOptions { against: Baseline::Prepared, exclude: Vec::new() }).await.unwrap();
        env.destroy().await.unwrap();
        out.patch
    }
}

#[tokio::test]
async fn repro_uses_a_fresh_fork_per_repetition_and_observes_each() {
    let h = harness().await;
    let dl = h.link.add_blob(DownloadRef::Artifact(SCRIPT), Some("repro.sh"), b"curl -s localhost\n".to_vec());
    h.exec.on_env(|env| {
        env.on_exec(|spec| is_script(spec).then(|| Ok(outcome(Exit::Code(1), b"calling\nMAGE_HTTP_STATUS=500\n", b"boom\n"))));
    });
    let spec = VerifySpec::Repro { script: SCRIPT, signature: sig(SignatureKind::HttpStatus, "500"), repetitions: 3 };
    let result = h.run(&assignment(spec, vec![dl])).await;

    assert_eq!(result.outcome, VerifyOutcome::Completed);
    assert_eq!(result.attempts.len(), 3);
    for (i, attempt) in result.attempts.iter().enumerate() {
        assert_eq!(attempt.index as usize, i + 1);
        assert_eq!(attempt.kind, AttemptKind::Script);
        assert_eq!(attempt.script_exit, Some(1));
        assert!(!attempt.script_passed);
        assert_eq!(attempt.observed, vec![sig(SignatureKind::HttpStatus, "500")]);
    }
    assert_eq!(result.observed, Some(sig(SignatureKind::HttpStatus, "500")));
    let forks = h.exec.forks();
    assert_eq!(forks.len(), 3, "one fresh fork per repetition");
    assert!(forks.iter().all(|f| f.purpose == ForkPurpose::Verify));
    let envs = h.exec.envs();
    assert!(envs.iter().all(|e| e.destroyed()), "every fork destroyed");
    for env in &envs {
        assert_eq!(env.file(&EnvPath::scratch("repro/repro.sh").unwrap()).as_deref(), Some(&b"curl -s localhost\n"[..]), "pinned bytes in scratch");
        let script_run = env.execs().into_iter().find(is_script).unwrap();
        assert_eq!(script_run.command, Command::Argv(vec!["bash".into(), "/fake/scratch/repro/repro.sh".into()]));
        assert_eq!(script_run.timeout, Duration::from_secs(300));
        assert_eq!(script_run.role, ProcessRole::Test);
        assert!(env.file(&EnvPath::ws("repro.sh").unwrap()).is_none(), "workspace stays pristine");
    }
    assert!(h.artifact_names().contains(&"env-logs-r3.tar.zst".to_owned()));
}

#[tokio::test]
async fn repro_keeps_r1_recordings_and_traces_that_differ() {
    let h = harness().await;
    let dl = h.link.add_blob(DownloadRef::Artifact(SCRIPT), Some("repro.spec.ts"), b"test('x', ...)".to_vec());
    h.browser.push_recorded(failed("Error: boom", &[500]));
    h.browser.push_recorded(failed("Error: boom", &[500]));
    h.browser.push_recorded(failed("Error: other", &[404]));
    let spec = VerifySpec::Repro { script: SCRIPT, signature: sig(SignatureKind::HttpStatus, "500"), repetitions: 3 };
    let result = h.run(&assignment(spec, vec![dl])).await;

    assert_eq!(result.outcome, VerifyOutcome::Completed);
    let observed: Vec<Vec<String>> = result.attempts.iter().map(|a| a.observed.iter().map(|o| o.value.clone()).collect()).collect();
    assert_eq!(observed, [vec!["500".to_owned()], vec!["500".to_owned()], vec!["404".to_owned()]], "signature mismatch on r3 is reported, not judged");
    assert_eq!(result.attempts[0].status.as_deref(), Some("failed"));
    let names = h.artifact_names();
    for want in ["verify-r1.video.webm", "verify-r1.trace.zip", "verify-r3.trace.zip", "verify-r2.network.har", "verify-r3.report.json"] {
        assert!(names.contains(&want.to_owned()), "{want} in {names:?}");
    }
    for unwanted in ["verify-r2.video.webm", "verify-r2.trace.zip", "verify-r3.video.webm"] {
        assert!(!names.contains(&unwanted.to_owned()), "{unwanted} not kept");
    }
    let call = &h.browser.calls()[0];
    assert_eq!(call.opts.label, "verify-r1");
    assert!(call.opts.video && call.opts.trace && call.opts.har);
    assert!(matches!(&call.script.source, ScriptSource::Inline { file_name, bytes } if file_name == "repro.spec.ts" && bytes == b"test('x', ...)"));
    assert_eq!(result.artifacts.len(), names.len(), "every upload is listed");
    assert!(std::fs::read_dir(h.outbox.path()).unwrap().next().is_none(), "outbox emptied");
}

#[tokio::test]
async fn repro_unhealthy_boot_and_fork_failure() {
    let h = harness().await;
    let dl = h.link.add_blob(DownloadRef::Artifact(SCRIPT), Some("repro.sh"), b"x".to_vec());
    h.exec.on_env(|env| env.fail_service("web", "connection refused"));
    let spec = VerifySpec::Repro { script: SCRIPT, signature: sig(SignatureKind::ExitCode, "1"), repetitions: 2 };
    let result = h.run(&assignment(spec.clone(), vec![dl.clone()])).await;
    assert!(matches!(&result.outcome, VerifyOutcome::BootFailed { reason } if reason.contains("web")), "{:?}", result.outcome);
    assert!(result.attempts.is_empty());
    assert_eq!(h.exec.forks().len(), 1, "stops at the first unhealthy fork");
    assert!(h.exec.envs()[0].destroyed());

    let h = harness().await;
    let dl = h.link.add_blob(DownloadRef::Artifact(SCRIPT), Some("repro.sh"), b"x".to_vec());
    h.exec.fail_next_fork(ExecError::Unavailable("max_envs reached".into()));
    let result = h.run(&assignment(spec, vec![dl])).await;
    assert!(matches!(&result.outcome, VerifyOutcome::Errored { reason } if reason.contains("fork failed")), "{:?}", result.outcome);
}

#[tokio::test]
async fn downloads_are_pinned() {
    let h = harness().await;
    let spec = VerifySpec::Repro { script: SCRIPT, signature: sig(SignatureKind::ExitCode, "1"), repetitions: 1 };
    let result = h.run(&assignment(spec.clone(), Vec::new())).await;
    assert!(matches!(&result.outcome, VerifyOutcome::Errored { reason } if reason.contains("not in the assignment")));

    let mut dl = h.link.add_blob(DownloadRef::Artifact(SCRIPT), Some("repro.sh"), b"x".to_vec());
    dl.name = None;
    let result = h.run(&assignment(spec.clone(), vec![dl])).await;
    assert!(matches!(&result.outcome, VerifyOutcome::Errored { reason } if reason.contains("no name")));

    let dl = h.link.add_blob(DownloadRef::Artifact(SCRIPT), Some("repro.txt"), b"x".to_vec());
    let result = h.run(&assignment(spec, vec![dl])).await;
    assert!(matches!(&result.outcome, VerifyOutcome::Errored { reason } if reason.contains("not a repro script")));
    assert!(h.exec.envs().iter().all(|e| e.destroyed()));
}

#[tokio::test]
async fn boot_spec_reports_health() {
    let h = harness().await;
    let result = h.run(&assignment(VerifySpec::Boot, Vec::new())).await;
    assert_eq!(result.outcome, VerifyOutcome::Completed);
    assert_eq!(result.attempts.len(), 1);
    assert_eq!(result.attempts[0].kind, AttemptKind::Boot);
    assert!(result.attempts[0].health_ok);
    assert!(h.exec.envs()[0].destroyed());
}

fn fix_spec(repetitions: u8, mode: ApplyMode, upstream_check: bool, regression: Option<RegressionFile>) -> VerifySpec {
    VerifySpec::Fix { patch: PATCH, script: SCRIPT, signature: sig(SignatureKind::HttpStatus, "500"), repetitions, regression, mode, upstream_check }
}

/// The fixed app: the script exits 0 without the status line; tests and lint pass.
fn fixed_app(env: &Arc<FakeEnv>) {
    env.on_exec(|spec| {
        if is_script(spec) {
            return Some(Ok(outcome(Exit::Code(0), b"ok\n", b"")));
        }
        match &spec.command {
            Command::Shell(cmd) if cmd == "npm test" => Some(Ok(outcome(Exit::Code(0), b"12 passing\n", b""))),
            Command::Shell(cmd) if cmd == "npm run lint" => Some(Ok(outcome(Exit::Code(3), b"", b"lint: 2 warnings\n"))),
            _ => None,
        }
    });
}

#[tokio::test]
async fn fix_checks_with_regression_file() {
    let h = harness().await;
    let patch = h.patch().await;
    let script = b"curl -s localhost\n".to_vec();
    let dl_script = h.link.add_blob(DownloadRef::Artifact(SCRIPT), Some("repro.sh"), script.clone());
    let dl_patch = h.link.add_blob(DownloadRef::Artifact(PATCH), Some("fix-1.patch"), patch.clone());
    h.exec.on_env(fixed_app);
    let regression = RegressionFile {
        path: RepoPath::new(".mage/regressions/eng-1-null-customer.sh").unwrap(),
        script: SCRIPT,
        script_sha256: Sha256::digest(&script).into(),
    };
    let result = h.run(&assignment(fix_spec(2, ApplyMode::Exact, false, Some(regression)), vec![dl_script, dl_patch])).await;

    assert_eq!(result.outcome, VerifyOutcome::Completed, "{:?}", result.outcome);
    assert_eq!(result.attempts.len(), 2);
    assert!(result.attempts.iter().all(|a| a.script_passed && a.observed.is_empty()));
    let checks = result.fix_checks.clone().unwrap();
    assert_eq!(checks.applied_patch_sha256, Some(Sha256::digest(&patch).into()));
    assert!(checks.boot);
    assert_eq!(checks.test_exit, Some(0));
    assert_eq!(checks.lint_exit, Some(3), "lint exit recorded; the gate decides by lint_mode");
    assert_eq!(checks.source_applies, Some(true));
    assert_eq!(checks.upstream_already_fixed, None);
    assert_eq!(result.rebased_patch, None);
    let envs: Vec<Arc<FakeEnv>> = h.exec.envs().into_iter().skip(1).collect();
    assert_eq!(envs.len(), 3, "two repetition forks plus the tests fork");
    for env in &envs {
        assert!(env.destroyed());
        assert_eq!(env.applied(), vec![(patch.clone(), ApplyMode::Exact)]);
        assert_eq!(env.file(&EnvPath::ws(".mage/regressions/eng-1-null-customer.sh").unwrap()), Some(script.clone()));
    }
    let names = h.artifact_names();
    assert!(names.contains(&"tests.log".to_owned()) && names.contains(&"lint.log".to_owned()), "{names:?}");
    let tests_log = h.link.artifacts().into_iter().find(|a| a.name == "tests.log").unwrap();
    assert!(String::from_utf8_lossy(&tests_log.bytes).contains("12 passing"));
}

#[tokio::test]
async fn fix_patch_rejected_stops_early() {
    let h = harness().await;
    let dl_script = h.link.add_blob(DownloadRef::Artifact(SCRIPT), Some("repro.sh"), b"x".to_vec());
    let dl_patch = h.link.add_blob(DownloadRef::Artifact(PATCH), Some("fix-1.patch"), b"not a patch".to_vec());
    h.exec.on_env(|env| {
        env.script_apply(Ok(ApplyOutcome::Rejected {
            reason: RejectReason::DoesNotApply,
            detail: "patch failed: src/app.ts:1".into(),
            paths: vec![RelPath::new("src/app.ts").unwrap()],
        }));
    });
    let result = h.run(&assignment(fix_spec(2, ApplyMode::Exact, false, None), vec![dl_script, dl_patch])).await;
    assert!(matches!(&result.outcome, VerifyOutcome::PatchRejected { reason } if reason.contains("src/app.ts")), "{:?}", result.outcome);
    assert_eq!(h.exec.forks().len(), 1, "no further forks");
    assert!(result.attempts.is_empty());
    let checks = result.fix_checks.unwrap();
    assert_eq!((checks.applied_patch_sha256, checks.boot, checks.test_exit), (None, false, None));
}

#[tokio::test]
async fn fix_rebase_uploads_the_rebased_patch_and_checks_upstream() {
    let h = harness().await;
    let patch = h.patch().await;
    let dl_script = h.link.add_blob(DownloadRef::Artifact(SCRIPT), Some("repro.sh"), b"x".to_vec());
    let dl_patch = h.link.add_blob(DownloadRef::Artifact(PATCH), Some("fix-1.patch"), patch.clone());
    h.exec.on_env(fixed_app);
    let result = h.run(&assignment(fix_spec(2, ApplyMode::AllowOffset, true, None), vec![dl_script, dl_patch])).await;

    assert_eq!(result.outcome, VerifyOutcome::Completed, "{:?}", result.outcome);
    assert_eq!(result.attempts[0].kind, AttemptKind::UpstreamScript);
    assert_eq!(result.attempts[0].index, 0);
    let checks = result.fix_checks.clone().unwrap();
    assert_eq!(checks.upstream_already_fixed, Some(true), "the unpatched script already passes here");
    let rebased = h.link.artifacts().into_iter().find(|a| a.name == "rebased.patch").expect("rebased patch uploaded");
    assert_eq!(result.rebased_patch, Some(rebased.id));
    assert!(result.artifacts.contains(&rebased.id));
    assert_eq!(checks.applied_patch_sha256, Some(Sha256::digest(&rebased.bytes).into()));
    // The upstream fork applies nothing; r1 applies with offsets; later forks apply the rebased bytes exactly.
    let applied: Vec<Vec<(Vec<u8>, ApplyMode)>> = h.exec.envs().into_iter().skip(1).map(|e| e.applied()).collect();
    assert!(applied[0].is_empty());
    assert_eq!(applied[1], vec![(patch, ApplyMode::AllowOffset)]);
    assert_eq!(applied[2], vec![(rebased.bytes.clone(), ApplyMode::Exact)]);
    assert_eq!(applied[3], vec![(rebased.bytes, ApplyMode::Exact)]);
}

#[tokio::test]
async fn fix_rebuild_failure_is_boot_failed() {
    let h = harness().await;
    let patch = h.patch().await;
    let dl_script = h.link.add_blob(DownloadRef::Artifact(SCRIPT), Some("repro.sh"), b"x".to_vec());
    let dl_patch = h.link.add_blob(DownloadRef::Artifact(PATCH), Some("fix-1.patch"), patch);
    h.exec.on_env(|env| env.fail_service("web", "exited 1"));
    let result = h.run(&assignment(fix_spec(2, ApplyMode::Exact, false, None), vec![dl_script, dl_patch])).await;
    assert!(matches!(&result.outcome, VerifyOutcome::BootFailed { .. }), "{:?}", result.outcome);
    assert!(!result.fix_checks.unwrap().boot);
}

#[tokio::test]
async fn test_baseline_reports_the_test_exit() {
    let h = harness().await;
    h.exec.on_env(|env| {
        env.on_exec(|spec| matches!(&spec.command, Command::Shell(c) if c == "npm test").then(|| Ok(outcome(Exit::Code(1), b"1 failing\n", b""))));
    });
    let result = h.run(&assignment(VerifySpec::TestBaseline, Vec::new())).await;
    assert_eq!(result.outcome, VerifyOutcome::Completed);
    let checks = result.fix_checks.unwrap();
    assert_eq!((checks.test_exit, checks.boot, checks.lint_exit), (Some(1), true, None));
    let spec = h.exec.envs()[0].execs().into_iter().find(|s| s.role == ProcessRole::Test).unwrap();
    assert_eq!(spec.timeout, Duration::from_secs(600), "recipe test.timeout_s");
    assert!(h.artifact_names().contains(&"tests.log".to_owned()));
}

#[test]
fn prepare_failures_split_recipe_from_infra() {
    let build = ExecError::PrepareFailed { step: "deps".into(), exit: Some(Exit::Code(1)), log_tail: "npm ERR! 404".into() };
    assert!(matches!(prepare_failure_outcome(&build), VerifyOutcome::BootFailed { reason } if reason.contains("deps") && reason.contains("npm ERR! 404")));
    let health = ExecError::ServiceUnhealthy { service: "api".into(), reason: "no healthy probe".into(), log_tail: "EADDRINUSE".into() };
    assert!(matches!(prepare_failure_outcome(&health), VerifyOutcome::BootFailed { reason } if reason.contains("api")));
    for infra in [ExecError::Unavailable("disk".into()), ExecError::BundleRejected("sha".into()), ExecError::SourceUrlExpired("x".into())] {
        assert!(matches!(prepare_failure_outcome(&infra), VerifyOutcome::Errored { .. }), "{infra:?}");
    }
    let empty = failed_result(VerifyOutcome::Errored { reason: "x".into() });
    assert!(empty.attempts.is_empty() && empty.fix_checks.is_none());
}

#[test]
fn observation_helpers_map_outcomes() {
    let browser = failed("Error: \u{1b}[31mexpected 41\u{1b}[39m", &[502]);
    let obs = observe_browser(&sig(SignatureKind::AssertionText, "Error: expected 41"), &browser, &[]);
    assert_eq!(obs[0].value, "Error: expected 41");
    let timed_out = BrowserOutcome { status: crate::ports::browser::BrowserStatus::TimedOut, ..failed("x", &[500]) };
    assert!(observe_browser(&sig(SignatureKind::HttpStatus, "500"), &timed_out, &[]).is_empty());
    assert!(observe_browser(&sig(SignatureKind::HttpStatus, "500"), &passed(), &[]).is_empty());
    let exec = outcome(Exit::Signal(9), b"", b"");
    assert_eq!(observe_exec(&sig(SignatureKind::ExitCode, "signal:9"), &exec, &[])[0].value, "signal:9");
}
