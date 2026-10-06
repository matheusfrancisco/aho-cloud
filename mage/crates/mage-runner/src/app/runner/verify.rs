//! Verify runs (executor.md "Verify runs", fix.md "Fix gate"): Boot, Repro, Fix, TestBaseline.
//! No harness, no Run MCP. Every repetition gets a fresh fork; the runner reports facts
//! (`VerifyResult`) and `gates::{boot, repro, fix}` on the server decide.
//!
//! The session task (RunnerLoop) prepares the base, calls [`VerifyRunner::run`], posts the result
//! and finishes the run; prepare failures become [`prepare_failure_outcome`].
//!
//! Also home of the script-observation helpers shared with `submit_diagnosis`'s precheck.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mage_core::domain::fix::{ApplyMode, RegressionFile};
use mage_core::domain::{
    ArtifactId, ArtifactKind, AttemptKind, FailureSignature, FixChecks, ScriptKind, VerifyAttempt, VerifyOutcome, VerifyResult, VerifySpec,
};
use mage_core::ports::Clock;
use mage_core::wire::runner::{Download, DownloadRef, RunAssignment};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::app::runner::artifacts::{tar_zst, upload_file};
use crate::app::runner::media::{MediaConfig, MediaEncoder};
use crate::app::runner::tools::shape::tail_bytes;
use crate::domain::observe::{PlaywrightRun, ScriptRun, ShellExit, ShellRun, observe};
use crate::ports::browser::{BrowserError, BrowserOutcome, BrowserRunner, BrowserScript, BrowserStatus, LocalArtifact, RecordOpts, ScreenshotMode, ScriptSource};
use crate::ports::executor::{
    ApplyOutcome, Baseline, BaseRef, CaptureLimits, DiffOptions, EgressProfile, Env, EnvPath, ExecError, ExecOutcome, ExecSpec, Executor, Exit,
    ForkOptions, ForkPurpose, LogCursor, LogQuery, LogSource, NullObserver, PathGlob, ProcessRole, RebuildScope, RelPath, ServiceSelector,
    WriteMode,
};
use crate::ports::link::ServerLink;

/// Log tails carried into `BootFailed` / `Errored` reasons.
const REASON_TAIL_BYTES: usize = 8 * 1024;
/// Service log bytes kept per fork (`env-logs-<x>.tar.zst`).
const ENV_LOGS_CAP: usize = 20 * 1024 * 1024;
/// Lines read per service log for observations and the per-fork log bundle.
const SERVICE_LOG_LINES: u32 = 2000;
/// Test / lint output kept for `tests.log` / `lint.log`.
const TEST_CAPTURE: CaptureLimits = CaptureLimits { head: 256 * 1024, tail: 4 * 1024 * 1024 };

/// `[runner.verify]`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VerifyConfig {
    /// Shell scripts (`bash <scratch>/repro/<file>`).
    pub script_timeout_s: u64,
    /// Playwright scripts: whole invocation (`globalTimeout`) and per test.
    pub playwright_timeout_s: u64,
    pub playwright_test_timeout_s: u64,
    /// mp4 + GIF derived from the repetition-1 recording.
    pub media: MediaConfig,
}

impl Default for VerifyConfig {
    fn default() -> Self {
        Self { script_timeout_s: 300, playwright_timeout_s: 180, playwright_test_timeout_s: 120, media: MediaConfig::default() }
    }
}

/// Outcome of a prepare failure: recipe-caused (a build step, migrate, seed, or health) →
/// `BootFailed` (a gate failure); infrastructure → `Errored` (no gate attempt).
pub fn prepare_failure_outcome(err: &ExecError) -> VerifyOutcome {
    match err {
        ExecError::PrepareFailed { step, exit, log_tail } => {
            let exit = exit.map_or_else(String::new, |e| format!(" ({e})"));
            VerifyOutcome::BootFailed { reason: format!("prepare step {step} failed{exit}\n{}", tail(log_tail)) }
        }
        ExecError::ServiceUnhealthy { service, reason, log_tail } => {
            VerifyOutcome::BootFailed { reason: format!("service {service} unhealthy: {reason}\n{}", tail(log_tail)) }
        }
        other => VerifyOutcome::Errored { reason: other.to_string() },
    }
}

/// A result with no attempts (prepare failed, or the assignment was unusable).
pub fn failed_result(outcome: VerifyOutcome) -> VerifyResult {
    VerifyResult { outcome, observed: None, attempts: Vec::new(), artifacts: Vec::new(), fix_checks: None, rebased_patch: None }
}

fn tail(text: &str) -> String {
    tail_bytes(text, REASON_TAIL_BYTES).to_owned()
}

fn ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

// ---------------------------------------------------------------------------------------------
// Observation helpers (shared with submit_diagnosis's precheck)
// ---------------------------------------------------------------------------------------------

/// Current end cursor of every recipe service's log (before running a script).
pub async fn service_cursors(env: &dyn Env) -> Vec<(String, LogCursor)> {
    let Some(recipe) = env.recipe() else { return Vec::new() };
    let mut out = Vec::with_capacity(recipe.services.len());
    for name in recipe.services.keys() {
        let q = LogQuery { source: LogSource::Service(name.clone()), tail: 0, grep: None, since: None };
        // A service without a log yet has nothing to compare against.
        if let Ok(slice) = env.logs(q).await {
            out.push((name.clone(), slice.cursor));
        }
    }
    out
}

/// Service log lines appended after `cursors`.
pub async fn service_lines_since(env: &dyn Env, cursors: &[(String, LogCursor)]) -> Vec<String> {
    let mut lines = Vec::new();
    for (name, cursor) in cursors {
        let q = LogQuery { source: LogSource::Service(name.clone()), tail: SERVICE_LOG_LINES, grep: None, since: Some(*cursor) };
        if let Ok(slice) = env.logs(q).await {
            lines.extend(slice.lines);
        }
    }
    lines
}

/// Observations of a Playwright run.
pub fn observe_browser(expected: &FailureSignature, outcome: &BrowserOutcome, service_lines: &[String]) -> Vec<FailureSignature> {
    let messages: Vec<&str> = outcome.failures.iter().map(|f| f.message.as_str()).collect();
    let run = ScriptRun::Playwright(PlaywrightRun {
        timed_out: outcome.status == BrowserStatus::TimedOut,
        failure_messages: &messages,
        http_statuses: &outcome.http_statuses,
        output: &outcome.stdout_tail,
    });
    observe(expected, &run, service_lines)
}

/// Observations of a shell script run.
pub fn observe_exec(expected: &FailureSignature, outcome: &ExecOutcome, service_lines: &[String]) -> Vec<FailureSignature> {
    let stdout = captured_text(&outcome.stdout.head, &outcome.stdout.tail);
    let stderr = captured_text(&outcome.stderr.head, &outcome.stderr.tail);
    let run = ScriptRun::Shell(ShellRun { exit: shell_exit(outcome.exit), stdout: &stdout, stderr: &stderr });
    observe(expected, &run, service_lines)
}

fn shell_exit(exit: Exit) -> ShellExit {
    match exit {
        Exit::Code(c) => ShellExit::Code(c),
        Exit::Signal(s) => ShellExit::Signal(s),
        Exit::TimedOut => ShellExit::TimedOut,
        Exit::Killed(_) => ShellExit::Killed,
    }
}

/// Head and tail of a stream; a dropped middle becomes a line break so lines stay separate.
fn captured_text(head: &[u8], tail: &[u8]) -> String {
    let mut text = String::from_utf8_lossy(head).into_owned();
    if !tail.is_empty() {
        text.push('\n');
        text.push_str(&String::from_utf8_lossy(tail));
    }
    text
}

// ---------------------------------------------------------------------------------------------
// Verify runner
// ---------------------------------------------------------------------------------------------

/// A downloaded, pinned artifact.
#[derive(Debug, Clone)]
struct Pinned {
    name: String,
    bytes: Vec<u8>,
}

/// Infrastructure failure: the run ends `Errored` (no gate attempt).
#[derive(Debug)]
struct Infra(String);

impl From<ExecError> for Infra {
    fn from(e: ExecError) -> Self {
        Infra(e.to_string())
    }
}

/// How one fork ended before the script ran.
enum ForkStop {
    /// Recipe-caused (rebuild or service health) → `BootFailed`.
    Boot(String),
    /// `apply_patch` rejected → `PatchRejected`.
    Patch(String),
    Infra(Infra),
}

impl From<Infra> for ForkStop {
    fn from(e: Infra) -> Self {
        ForkStop::Infra(e)
    }
}

/// Outcome of one script run.
struct ScriptResult {
    attempt: VerifyAttempt,
    browser: Option<BrowserOutcome>,
}

pub struct VerifyRunner {
    executor: Arc<dyn Executor>,
    browser: Arc<dyn BrowserRunner>,
    link: Arc<dyn ServerLink>,
    clock: Arc<dyn Clock>,
    config: VerifyConfig,
    media: MediaEncoder,
}

/// Per-run context.
struct Ctx<'a> {
    a: &'a RunAssignment,
    base: &'a BaseRef,
    outbox: &'a Path,
    artifacts: Vec<ArtifactId>,
}

impl VerifyRunner {
    pub fn new(
        executor: Arc<dyn Executor>,
        browser: Arc<dyn BrowserRunner>,
        link: Arc<dyn ServerLink>,
        clock: Arc<dyn Clock>,
        config: VerifyConfig,
    ) -> Self {
        let media = MediaEncoder::new(config.media.clone());
        Self { executor, browser, link, clock, config, media }
    }

    /// Runs the assignment's `VerifySpec` on `base`. Never fails: infrastructure problems become
    /// `VerifyOutcome::Errored`.
    pub async fn run(&self, a: &RunAssignment, base: &BaseRef, outbox: &Path) -> VerifyResult {
        let Some(spec) = &a.verify else { return failed_result(VerifyOutcome::Errored { reason: "assignment has no verify spec".into() }) };
        if let Err(e) = tokio::fs::create_dir_all(outbox).await {
            return failed_result(VerifyOutcome::Errored { reason: format!("outbox: {e}") });
        }
        tracing::info!(run = %a.run_id, at = %self.clock.now(), "verify started");
        let mut ctx = Ctx { a, base, outbox, artifacts: Vec::new() };
        let mut result = match spec {
            VerifySpec::Boot => self.boot(&mut ctx).await,
            VerifySpec::Repro { script, signature, repetitions } => self.repro(&mut ctx, *script, signature, *repetitions).await,
            VerifySpec::Fix { patch, script, signature, repetitions, regression, mode, upstream_check } => {
                self.fix(&mut ctx, *patch, *script, signature, *repetitions, regression.as_ref(), *mode, *upstream_check).await
            }
            VerifySpec::TestBaseline => self.test_baseline(&mut ctx).await,
        };
        result.artifacts = ctx.artifacts;
        result.observed = result.attempts.last().and_then(|at| at.observed.first().cloned());
        result
    }

    async fn fork(&self, ctx: &Ctx<'_>) -> Result<Arc<dyn Env>, Infra> {
        let opts = ForkOptions { run_id: ctx.a.run_id, purpose: ForkPurpose::Verify, egress: EgressProfile::Registries };
        self.executor.fork(ctx.base, opts).await.map_err(|e| Infra(format!("fork failed: {e}")))
    }

    async fn destroy(&self, env: &dyn Env) {
        if let Err(e) = env.destroy().await {
            tracing::warn!(env = %env.id(), error = %e, "verify fork destroy failed; the reaper retries");
        }
    }

    /// `start_services(All)`: a service that does not come up is a recipe failure.
    async fn boot_services(&self, env: &dyn Env) -> Result<(), ForkStop> {
        match env.start_services(ServiceSelector::All).await {
            Ok(report) if report.all_healthy => Ok(()),
            Ok(report) => {
                let bad: Vec<String> = report
                    .services
                    .iter()
                    .filter(|s| !matches!(s.state, crate::ports::executor::ServiceState::Healthy))
                    .map(|s| format!("{} ({:?})", s.name, s.state))
                    .collect();
                Err(ForkStop::Boot(format!("services not healthy: {}", bad.join(", "))))
            }
            Err(e @ (ExecError::ServiceUnhealthy { .. } | ExecError::PrepareFailed { .. })) => match prepare_failure_outcome(&e) {
                VerifyOutcome::BootFailed { reason } => Err(ForkStop::Boot(reason)),
                _ => Err(ForkStop::Infra(Infra(e.to_string()))),
            },
            Err(e) => Err(ForkStop::Infra(e.into())),
        }
    }

    async fn download(&self, ctx: &Ctx<'_>, artifact: ArtifactId) -> Result<Pinned, Infra> {
        let dl: &Download = ctx
            .a
            .downloads
            .iter()
            .find(|d| d.what == DownloadRef::Artifact(artifact))
            .ok_or_else(|| Infra(format!("artifact {artifact} is not in the assignment's downloads")))?;
        let name = dl.name.clone().ok_or_else(|| Infra(format!("download of {artifact} carries no name")))?;
        let dest = ctx.outbox.join(format!("download-{artifact}"));
        self.link
            .download(ctx.a.run_id, ctx.a.attempt, dl, &dest)
            .await
            .map_err(|e| Infra(format!("download of {name} failed: {e}")))?;
        let bytes = tokio::fs::read(&dest).await.map_err(|e| Infra(format!("read {name}: {e}")))?;
        let _ = tokio::fs::remove_file(&dest).await;
        if <[u8; 32]>::from(Sha256::digest(&bytes)) != dl.sha256 {
            return Err(Infra(format!("{name}: sha256 does not match its pin")));
        }
        Ok(Pinned { name, bytes })
    }

    /// Uploads a local file; failures are logged, never fatal (artifacts are evidence, not facts).
    async fn upload(&self, ctx: &mut Ctx<'_>, local: &LocalArtifact) {
        match upload_file(self.link.as_ref(), ctx.a.run_id, ctx.a.attempt, local.kind, &local.name, &local.content_type, &local.path).await {
            Ok(id) => {
                ctx.artifacts.push(id);
                let _ = tokio::fs::remove_file(&local.path).await;
            }
            Err(e) => tracing::warn!(run = %ctx.a.run_id, artifact = %local.name, error = %e, "verify artifact upload failed"),
        }
    }

    async fn upload_bytes(&self, ctx: &mut Ctx<'_>, kind: ArtifactKind, name: &str, content_type: &str, bytes: &[u8]) -> Result<ArtifactId, Infra> {
        let path = ctx.outbox.join(name);
        tokio::fs::write(&path, bytes).await.map_err(|e| Infra(format!("outbox: {e}")))?;
        let result = upload_file(self.link.as_ref(), ctx.a.run_id, ctx.a.attempt, kind, name, content_type, &path).await;
        let _ = tokio::fs::remove_file(&path).await;
        let id = result.map_err(|e| Infra(format!("upload of {name} failed: {e}")))?;
        ctx.artifacts.push(id);
        Ok(id)
    }

    /// Service logs of a fork as `env-logs-<tag>.tar.zst` (Log).
    async fn upload_env_logs(&self, ctx: &mut Ctx<'_>, env: &dyn Env, tag: &str) {
        let Some(recipe) = env.recipe() else { return };
        let mut entries = Vec::new();
        for name in recipe.services.keys() {
            let q = LogQuery { source: LogSource::Service(name.clone()), tail: SERVICE_LOG_LINES, grep: None, since: None };
            if let Ok(slice) = env.logs(q).await {
                let mut text = slice.lines.join("\n");
                text.push('\n');
                entries.push((format!("svc/{name}.log"), text.into_bytes()));
            }
        }
        if entries.is_empty() {
            return;
        }
        let name = format!("env-logs-{tag}.tar.zst");
        let path: PathBuf = ctx.outbox.join(&name);
        if let Err(e) = tar_zst(entries, ENV_LOGS_CAP, path.clone()).await {
            tracing::warn!(error = %e, "packing verify service logs failed");
            return;
        }
        let bytes = tokio::fs::metadata(&path).await.map(|m| m.len()).unwrap_or(0);
        let local = LocalArtifact { kind: ArtifactKind::Log, name, path, bytes, sha256: [0; 32], content_type: "application/zstd".into() };
        self.upload(ctx, &local).await;
    }

    /// Runs the pinned script in `env` (written to `<scratch>/repro/<file>`), observes, and
    /// uploads the recordings this repetition keeps.
    async fn run_script(
        &self,
        ctx: &mut Ctx<'_>,
        env: &dyn Env,
        script: &Pinned,
        signature: &FailureSignature,
        index: u8,
        kind: AttemptKind,
        label: &str,
    ) -> Result<ScriptResult, Infra> {
        let script_kind = ScriptKind::from_file_name(&script.name).ok_or_else(|| Infra(format!("{} is not a repro script name", script.name)))?;
        let repro = format!("repro/{}", script.name);
        env.write(&EnvPath::scratch(&repro)?, &script.bytes, WriteMode::Overwrite).await?;
        let cursors = service_cursors(env).await;
        let started = Instant::now();
        let (mut attempt, browser) = match script_kind {
            ScriptKind::Playwright => {
                let opts = RecordOpts {
                    video: true,
                    trace: true,
                    screenshots: ScreenshotMode::OnFailure,
                    har: true,
                    timeout: Duration::from_secs(self.config.playwright_timeout_s),
                    test_timeout: Duration::from_secs(self.config.playwright_test_timeout_s),
                    outbox: ctx.outbox.to_path_buf(),
                    label: label.to_owned(),
                };
                let run = BrowserScript { source: ScriptSource::Inline { file_name: script.name.clone(), bytes: script.bytes.clone() }, grep: None };
                let outcome = self.browser.run(env, &run, &opts).await.map_err(|e| match e {
                    BrowserError::Env(e) => Infra::from(e),
                    other => Infra(format!("playwright: {other}")),
                })?;
                let lines = service_lines_since(env, &cursors).await;
                let observed = observe_browser(signature, &outcome, &lines);
                let attempt = VerifyAttempt {
                    index,
                    kind,
                    script_exit: None,
                    script_passed: outcome.passed,
                    status: Some(outcome.status.as_str().to_owned()),
                    observed,
                    health_ok: true,
                    duration_ms: 0,
                };
                (attempt, Some(outcome))
            }
            ScriptKind::Shell => {
                let path = format!("{}/{repro}", env.scratch_root().trim_end_matches('/'));
                let mut spec = ExecSpec::argv(vec!["bash".to_owned(), path], ProcessRole::Test);
                spec.cwd = EnvPath::ws(".")?;
                spec.timeout = Duration::from_secs(self.config.script_timeout_s);
                let outcome = env.exec(spec, None).await?;
                if let Exit::Killed(reason) = outcome.exit {
                    return Err(Infra(format!("script killed ({reason:?})")));
                }
                let lines = service_lines_since(env, &cursors).await;
                let observed = observe_exec(signature, &outcome, &lines);
                let attempt = VerifyAttempt {
                    index,
                    kind,
                    script_exit: match outcome.exit {
                        Exit::Code(c) => Some(c),
                        _ => None,
                    },
                    script_passed: outcome.exit.success(),
                    status: None,
                    observed,
                    health_ok: true,
                    duration_ms: 0,
                };
                (attempt, None)
            }
        };
        attempt.duration_ms = ms(started.elapsed());
        Ok(ScriptResult { attempt, browser })
    }

    /// Keeps: video + trace (+ mp4 + GIF) of repetition 1; the trace of a repetition whose
    /// observations differ from repetition 1; HAR and report of every repetition.
    async fn keep_recordings(&self, ctx: &mut Ctx<'_>, outcome: &BrowserOutcome, first: bool, differs: bool) {
        if first && let Some(video) = &outcome.video {
            if self.media.enabled() {
                let label = video.name.trim_end_matches(".video.webm").to_owned();
                match self.media.derive(&video.path, ctx.outbox, &label).await {
                    Ok(media) => {
                        for local in media.mp4.iter().chain(media.gif.iter()) {
                            self.upload(ctx, local).await;
                        }
                    }
                    Err(e) => tracing::warn!(run = %ctx.a.run_id, error = %e, "deriving mp4/gif failed; webm only"),
                }
            }
            self.upload(ctx, video).await;
        }
        if let Some(trace) = &outcome.trace
            && (first || differs)
        {
            self.upload(ctx, trace).await;
        }
        for local in outcome.har.iter().chain(outcome.report.iter()) {
            self.upload(ctx, local).await;
        }
        // Whatever was not kept leaves the outbox now.
        for local in outcome.artifacts() {
            let _ = tokio::fs::remove_file(&local.path).await;
        }
    }

    // ----- Boot ---------------------------------------------------------------------------------

    async fn boot(&self, ctx: &mut Ctx<'_>) -> VerifyResult {
        let env = match self.fork(ctx).await {
            Ok(env) => env,
            Err(Infra(reason)) => return failed_result(VerifyOutcome::Errored { reason }),
        };
        let started = Instant::now();
        let outcome = match self.boot_services(env.as_ref()).await {
            Ok(()) => match env.health().await {
                Ok(health) => Ok(health.all_ok),
                Err(e) => Err(ForkStop::Infra(e.into())),
            },
            Err(stop) => Err(stop),
        };
        self.upload_env_logs(ctx, env.as_ref(), "boot").await;
        self.destroy(env.as_ref()).await;
        match outcome {
            Ok(health_ok) => {
                let attempt = VerifyAttempt {
                    index: 1,
                    kind: AttemptKind::Boot,
                    script_exit: None,
                    script_passed: false,
                    status: None,
                    observed: Vec::new(),
                    health_ok,
                    duration_ms: ms(started.elapsed()),
                };
                VerifyResult { attempts: vec![attempt], ..failed_result(VerifyOutcome::Completed) }
            }
            Err(stop) => failed_result(stop_outcome(stop)),
        }
    }

    // ----- Repro --------------------------------------------------------------------------------

    async fn repro(&self, ctx: &mut Ctx<'_>, script: ArtifactId, signature: &FailureSignature, repetitions: u8) -> VerifyResult {
        let script = match self.download(ctx, script).await {
            Ok(s) => s,
            Err(Infra(reason)) => return failed_result(VerifyOutcome::Errored { reason }),
        };
        let mut attempts: Vec<VerifyAttempt> = Vec::new();
        for r in 1..=repetitions.max(1) {
            let env = match self.fork(ctx).await {
                Ok(env) => env,
                Err(Infra(reason)) => return VerifyResult { attempts, ..failed_result(VerifyOutcome::Errored { reason }) },
            };
            let step = async {
                self.boot_services(env.as_ref()).await?;
                Ok::<_, ForkStop>(
                    self.run_script(ctx, env.as_ref(), &script, signature, r, AttemptKind::Script, &format!("verify-r{r}")).await?,
                )
            }
            .await;
            self.upload_env_logs(ctx, env.as_ref(), &format!("r{r}")).await;
            self.destroy(env.as_ref()).await;
            match step {
                Ok(run) => {
                    let differs = attempts.first().is_some_and(|first| first.observed != run.attempt.observed);
                    if let Some(outcome) = &run.browser {
                        self.keep_recordings(ctx, outcome, r == 1, differs).await;
                    }
                    attempts.push(run.attempt);
                }
                Err(stop) => return VerifyResult { attempts, ..failed_result(stop_outcome(stop)) },
            }
        }
        VerifyResult { attempts, ..failed_result(VerifyOutcome::Completed) }
    }

    // ----- Fix ----------------------------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    async fn fix(
        &self,
        ctx: &mut Ctx<'_>,
        patch: ArtifactId,
        script_id: ArtifactId,
        signature: &FailureSignature,
        repetitions: u8,
        regression: Option<&RegressionFile>,
        mode: ApplyMode,
        upstream_check: bool,
    ) -> VerifyResult {
        let pins = async {
            let patch = self.download(ctx, patch).await?;
            let script = self.download(ctx, script_id).await?;
            let regression = match regression {
                Some(reg) => {
                    let bytes = if reg.script == script_id { script.bytes.clone() } else { self.download(ctx, reg.script).await?.bytes };
                    if <[u8; 32]>::from(Sha256::digest(&bytes)) != reg.script_sha256 {
                        return Err(Infra("regression script does not match its pin".into()));
                    }
                    Some((RelPath::new(reg.path.as_str())?, bytes))
                }
                None => None,
            };
            Ok::<_, Infra>((patch, script, regression))
        }
        .await;
        let (patch, script, regression) = match pins {
            Ok(p) => p,
            Err(Infra(reason)) => return failed_result(VerifyOutcome::Errored { reason }),
        };
        let mut checks =
            FixChecks { applied_patch_sha256: None, boot: false, test_exit: None, lint_exit: None, source_applies: None, upstream_already_fixed: None };
        let mut attempts = Vec::new();
        let mut patch_bytes = patch.bytes;
        let mut mode = mode;
        let mut rebased_patch = None;
        let done = |attempts, checks, rebased_patch, outcome| VerifyResult {
            outcome,
            observed: None,
            attempts,
            artifacts: Vec::new(),
            fix_checks: Some(checks),
            rebased_patch,
        };

        if upstream_check {
            let env = match self.fork(ctx).await {
                Ok(env) => env,
                Err(Infra(reason)) => return done(attempts, checks, None, VerifyOutcome::Errored { reason }),
            };
            let step = async {
                self.boot_services(env.as_ref()).await?;
                Ok::<_, ForkStop>(self.run_script(ctx, env.as_ref(), &script, signature, 0, AttemptKind::UpstreamScript, "verify-upstream").await?)
            }
            .await;
            self.upload_env_logs(ctx, env.as_ref(), "upstream").await;
            self.destroy(env.as_ref()).await;
            match step {
                Ok(run) => {
                    if let Some(outcome) = &run.browser {
                        self.keep_recordings(ctx, outcome, false, false).await;
                    }
                    checks.upstream_already_fixed = Some(run.attempt.script_passed && !signature.matches_any(&run.attempt.observed));
                    attempts.push(run.attempt);
                }
                Err(stop) => return done(attempts, checks, None, stop_outcome(stop)),
            }
        }

        for r in 1..=repetitions.max(1) {
            let env = match self.fork(ctx).await {
                Ok(env) => env,
                Err(Infra(reason)) => return done(attempts, checks, rebased_patch, VerifyOutcome::Errored { reason }),
            };
            let step = async {
                let applied = self.apply(env.as_ref(), &patch_bytes, mode, regression.as_ref()).await?;
                Ok::<_, ForkStop>(applied)
            }
            .await;
            let step = match step {
                Ok(()) if mode == ApplyMode::AllowOffset => {
                    // Rebase: re-diff the applied tree; later checks use these bytes, Exact.
                    match self.rebase(ctx, env.as_ref(), regression.as_ref()).await {
                        Ok((bytes, id)) => {
                            patch_bytes = bytes;
                            mode = ApplyMode::Exact;
                            rebased_patch = Some(id);
                            Ok(())
                        }
                        Err(e) => Err(ForkStop::Infra(e)),
                    }
                }
                other => other,
            };
            let step = match step {
                Ok(()) => {
                    checks.applied_patch_sha256 = Some(Sha256::digest(&patch_bytes).into());
                    let booted = self.boot_services(env.as_ref()).await;
                    match booted {
                        Ok(()) => {
                            checks.boot = true;
                            self.run_script(ctx, env.as_ref(), &script, signature, r, AttemptKind::Script, &format!("verify-r{r}"))
                                .await
                                .map_err(ForkStop::Infra)
                        }
                        Err(stop) => Err(stop),
                    }
                }
                Err(stop) => Err(stop),
            };
            self.upload_env_logs(ctx, env.as_ref(), &format!("r{r}")).await;
            self.destroy(env.as_ref()).await;
            match step {
                Ok(run) => {
                    let differs = attempts.iter().find(|a: &&VerifyAttempt| a.kind == AttemptKind::Script).is_some_and(|f| f.observed != run.attempt.observed);
                    if let Some(outcome) = &run.browser {
                        self.keep_recordings(ctx, outcome, r == 1, differs).await;
                    }
                    attempts.push(run.attempt);
                }
                Err(ForkStop::Boot(reason)) => {
                    checks.boot = false;
                    return done(attempts, checks, rebased_patch, VerifyOutcome::BootFailed { reason });
                }
                Err(stop) => return done(attempts, checks, rebased_patch, stop_outcome(stop)),
            }
        }

        // Tests fork: same apply + rebuild + services, then the test command and the lint.
        let env = match self.fork(ctx).await {
            Ok(env) => env,
            Err(Infra(reason)) => return done(attempts, checks, rebased_patch, VerifyOutcome::Errored { reason }),
        };
        let tests = async {
            self.apply(env.as_ref(), &patch_bytes, mode, regression.as_ref()).await?;
            self.boot_services(env.as_ref()).await?;
            let (test_exit, lint_exit) = self.tests_and_lint(ctx, env.as_ref(), true).await.map_err(ForkStop::Infra)?;
            let source = env.check_patch(&patch_bytes, Baseline::Source).await.map_err(|e| ForkStop::Infra(e.into()))?;
            Ok::<_, ForkStop>((test_exit, lint_exit, matches!(source, ApplyOutcome::Applied { .. })))
        }
        .await;
        self.upload_env_logs(ctx, env.as_ref(), "tests").await;
        self.destroy(env.as_ref()).await;
        match tests {
            Ok((test_exit, lint_exit, source_applies)) => {
                checks.test_exit = test_exit;
                checks.lint_exit = lint_exit;
                checks.source_applies = Some(source_applies);
                done(attempts, checks, rebased_patch, VerifyOutcome::Completed)
            }
            Err(ForkStop::Boot(reason)) => {
                checks.boot = false;
                done(attempts, checks, rebased_patch, VerifyOutcome::BootFailed { reason })
            }
            Err(stop) => done(attempts, checks, rebased_patch, stop_outcome(stop)),
        }
    }

    /// `apply_patch`, the regression file, then `rebuild(Changed(patch ∪ regression))`.
    async fn apply(&self, env: &dyn Env, patch: &[u8], mode: ApplyMode, regression: Option<&(RelPath, Vec<u8>)>) -> Result<(), ForkStop> {
        let files = match env.apply_patch(patch, mode).await.map_err(|e| ForkStop::Infra(e.into()))? {
            ApplyOutcome::Applied { files, .. } => files,
            ApplyOutcome::Rejected { reason, detail, paths } => {
                let paths: Vec<&str> = paths.iter().map(RelPath::as_str).collect();
                return Err(ForkStop::Patch(format!("{reason:?}: {detail} [{}]", paths.join(", "))));
            }
        };
        let mut changed = files;
        if let Some((path, bytes)) = regression {
            env.write(&EnvPath::new(crate::ports::executor::Area::Workspace, path.clone()), bytes, WriteMode::Overwrite)
                .await
                .map_err(|e| ForkStop::Infra(e.into()))?;
            if !changed.contains(path) {
                changed.push(path.clone());
            }
        }
        match env.rebuild(RebuildScope::Changed(changed), &NullObserver).await {
            Ok(_) => Ok(()),
            Err(e @ (ExecError::PrepareFailed { .. } | ExecError::ServiceUnhealthy { .. })) => match prepare_failure_outcome(&e) {
                VerifyOutcome::BootFailed { reason } => Err(ForkStop::Boot(reason)),
                _ => Err(ForkStop::Infra(e.into())),
            },
            Err(e) => Err(ForkStop::Infra(e.into())),
        }
    }

    /// The patch re-diffed against the prepared baseline of this (new) base, uploaded as `Patch`.
    async fn rebase(&self, ctx: &mut Ctx<'_>, env: &dyn Env, regression: Option<&(RelPath, Vec<u8>)>) -> Result<(Vec<u8>, ArtifactId), Infra> {
        let mut exclude = match &ctx.a.fix {
            Some(fix) => fix.policy.exclude_globs().into_iter().map(PathGlob::new).collect::<Result<Vec<_>, _>>()?,
            None => Vec::new(),
        };
        // The regression file is part of the change set, not of the patch.
        if let Some((path, _)) = regression {
            exclude.push(PathGlob::new(path.as_str())?);
        }
        let out = env.diff_against_base(&DiffOptions { against: Baseline::Prepared, exclude }).await?;
        if out.patch.is_empty() {
            return Err(Infra("the rebased patch is empty".into()));
        }
        let id = self.upload_bytes(ctx, ArtifactKind::Patch, "rebased.patch", "text/x-diff", &out.patch).await?;
        Ok((out.patch, id))
    }

    /// Recipe test command (and `[fix].lint` when `lint`), logs uploaded as `tests.log` /
    /// `lint.log`.
    async fn tests_and_lint(&self, ctx: &mut Ctx<'_>, env: &dyn Env, lint: bool) -> Result<(Option<i32>, Option<i32>), Infra> {
        let recipe = env.recipe().ok_or_else(|| Infra("the base has no recipe".into()))?;
        let test = recipe.test.as_ref().ok_or_else(|| Infra("the recipe has no [test] command".into()))?;
        let mut vars = Vec::with_capacity(test.env.len());
        for (name, value) in &test.env {
            vars.push((name.clone(), env.render(value)?));
        }
        let test_exit = self.command(ctx, env, &env.render(&test.run)?, &test.cwd, vars, test.timeout_s, "tests.log").await?;
        let lint_exit = match recipe.fix.as_ref().and_then(|f| f.lint.as_ref()) {
            Some(cmd) if lint => Some(self.command(ctx, env, &env.render(cmd)?, ".", Vec::new(), test.timeout_s, "lint.log").await?),
            _ => None,
        };
        Ok((Some(test_exit), lint_exit))
    }

    /// Runs a recipe command as `ProcessRole::Test`; exit as a number (signal → 128+n,
    /// timeout → 124), output uploaded as `log_name`.
    #[allow(clippy::too_many_arguments)]
    async fn command(
        &self,
        ctx: &mut Ctx<'_>,
        env: &dyn Env,
        cmd: &str,
        cwd: &str,
        vars: Vec<(String, String)>,
        timeout_s: u32,
        log_name: &str,
    ) -> Result<i32, Infra> {
        let mut spec = ExecSpec::shell(cmd, ProcessRole::Test);
        spec.cwd = EnvPath::ws(cwd)?;
        spec.env = vars;
        spec.timeout = Duration::from_secs(u64::from(timeout_s));
        spec.capture = TEST_CAPTURE;
        let out = env.exec(spec, None).await?;
        let mut log = Vec::with_capacity(out.stdout.head.len() + out.stdout.tail.len() + out.stderr.head.len() + out.stderr.tail.len() + 128);
        log.extend_from_slice(format!("$ {cmd}\n# {}\n# stdout\n", out.exit).as_bytes());
        log.extend_from_slice(&out.stdout.head);
        if out.stdout.omitted() > 0 {
            log.extend_from_slice(format!("\n…[{} bytes omitted]…\n", out.stdout.omitted()).as_bytes());
        }
        log.extend_from_slice(&out.stdout.tail);
        log.extend_from_slice(b"\n# stderr\n");
        log.extend_from_slice(&out.stderr.head);
        if out.stderr.omitted() > 0 {
            log.extend_from_slice(format!("\n…[{} bytes omitted]…\n", out.stderr.omitted()).as_bytes());
        }
        log.extend_from_slice(&out.stderr.tail);
        if let Err(Infra(e)) = self.upload_bytes(ctx, ArtifactKind::Log, log_name, "text/plain", &log).await {
            tracing::warn!(run = %ctx.a.run_id, error = %e, "{log_name} upload failed");
        }
        match out.exit {
            Exit::Code(c) => Ok(c),
            Exit::Signal(s) => Ok(128 + s),
            Exit::TimedOut => Ok(124),
            Exit::Killed(reason) => Err(Infra(format!("{log_name}: command killed ({reason:?})"))),
        }
    }

    // ----- TestBaseline -------------------------------------------------------------------------

    async fn test_baseline(&self, ctx: &mut Ctx<'_>) -> VerifyResult {
        let mut checks =
            FixChecks { applied_patch_sha256: None, boot: false, test_exit: None, lint_exit: None, source_applies: None, upstream_already_fixed: None };
        let env = match self.fork(ctx).await {
            Ok(env) => env,
            Err(Infra(reason)) => return failed_result(VerifyOutcome::Errored { reason }),
        };
        let step = async {
            self.boot_services(env.as_ref()).await?;
            self.tests_and_lint(ctx, env.as_ref(), false).await.map_err(ForkStop::Infra)
        }
        .await;
        self.upload_env_logs(ctx, env.as_ref(), "tests").await;
        self.destroy(env.as_ref()).await;
        match step {
            Ok((test_exit, _)) => {
                checks.boot = true;
                checks.test_exit = test_exit;
                VerifyResult { fix_checks: Some(checks), ..failed_result(VerifyOutcome::Completed) }
            }
            Err(stop) => VerifyResult { fix_checks: Some(checks), ..failed_result(stop_outcome(stop)) },
        }
    }
}

fn stop_outcome(stop: ForkStop) -> VerifyOutcome {
    match stop {
        ForkStop::Boot(reason) => VerifyOutcome::BootFailed { reason },
        ForkStop::Patch(reason) => VerifyOutcome::PatchRejected { reason },
        ForkStop::Infra(Infra(reason)) => VerifyOutcome::Errored { reason },
    }
}

#[cfg(test)]
mod tests;
