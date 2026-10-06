//! `PlaywrightRunner` over `FakeEnv`: the exec handler plays Playwright by writing the golden
//! 1.57.0 report, recordings, and HAR into the invocation dir. Real-toolchain run: opt-in test
//! at the bottom (`MAGE_TEST_PLAYWRIGHT=1`).

use std::sync::{Arc, Weak};
use std::time::Duration;

use super::*;
use crate::ports::browser::{BrowserStatus, RecordOpts, ScreenshotMode};
use crate::ports::executor::Command;
use crate::testing::executor::{FakeEnv, outcome};

const FAILED: &str = include_str!("testdata/failed.json");
const FAILED_HAR: &str = include_str!("testdata/failed.har");
const TOP_ERROR: &str = include_str!("testdata/top_level_error.json");

fn opts(outbox: &Path) -> RecordOpts {
    RecordOpts {
        video: true,
        trace: true,
        screenshots: ScreenshotMode::OnFailure,
        har: true,
        timeout: Duration::from_secs(180),
        test_timeout: Duration::from_secs(120),
        outbox: outbox.to_path_buf(),
        label: "explore-7".into(),
    }
}

fn runner() -> PlaywrightRunner {
    PlaywrightRunner::new(PlaywrightConfig {
        root: Some(PathBuf::from("/opt/mage/playwright")),
        node: Some(PathBuf::from("/opt/mage/toolchains/node-20/bin/node")),
        chromium_sandbox: true,
    })
}

fn is_cli(spec: &ExecSpec) -> bool {
    matches!(&spec.command, Command::Argv(argv) if argv.iter().any(|a| a.ends_with("cli.js")))
}

/// Plays Playwright for invocation `pw/1`: the report (golden paths rewritten into the fake's
/// scratch root), recordings, and HAR, then `exit`.
fn play(env: &Arc<FakeEnv>, report: &'static str, exit: Exit) {
    let weak: Weak<FakeEnv> = Arc::downgrade(env);
    env.on_exec(move |spec| {
        if matches!(&spec.command, Command::Argv(argv) if argv[0] == "ln") {
            return Some(Ok(outcome(Exit::Code(0), b"", b"")));
        }
        if !is_cli(spec) {
            return None;
        }
        let env = weak.upgrade()?;
        let set = |rel: &str, bytes: &[u8]| env.set_file(&EnvPath::scratch(rel).unwrap(), bytes);
        let out = "pw/1/out/repro-export-shows-the-customer-chromium";
        set("pw/1/report.json", report.replace("/mage/scratch/pw/3", "/fake/scratch/pw/1").as_bytes());
        set(&format!("{out}/video.webm"), b"webm-bytes");
        set(&format!("{out}/trace.zip"), b"zip-bytes");
        set(&format!("{out}/test-failed-1.png"), b"png-bytes");
        set(&format!("{out}/error-context.md"), b"md");
        set("pw/1/out/network.har", FAILED_HAR.as_bytes());
        Some(Ok(outcome(exit, b"\x1b[31m  1 failed\x1b[39m\n", b"")))
    });
}

fn workspace_env() -> Arc<FakeEnv> {
    let env = FakeEnv::empty();
    env.set_file(&EnvPath::ws("e2e/repro.spec.ts").unwrap(), b"import { test } from '@playwright/test';");
    env
}

#[tokio::test]
async fn failed_run_parses_the_report_and_exports_recordings() {
    let env = workspace_env();
    play(&env, FAILED, Exit::Code(1));
    let outbox = tempfile::tempdir().unwrap();
    let script = BrowserScript { source: ScriptSource::Workspace(RelPath::new("e2e/repro.spec.ts").unwrap()), grep: Some("export".into()) };
    let out = runner().run(env.as_ref(), &script, &opts(outbox.path())).await.unwrap();

    assert_eq!(out.status, BrowserStatus::Failed);
    assert!(!out.passed);
    assert!(out.error_text.as_deref().unwrap().starts_with("Error: expect(locator).toHaveText(expected) failed"));
    assert!(out.error_text.as_deref().unwrap().ends_with(" at /fake/scratch/pw/1/tests/repro.spec.ts:6"));
    assert_eq!(out.failures.len(), 1);
    assert_eq!(out.http_statuses, vec![500, 404], "HAR 500/404/500 and the annotated 500, deduplicated");
    assert_eq!(out.stdout_tail, "  1 failed\n");

    let video = out.video.as_ref().unwrap();
    assert_eq!((video.kind, video.name.as_str(), video.content_type.as_str()), (ArtifactKind::Video, "explore-7.video.webm", "video/webm"));
    assert_eq!(std::fs::read(&video.path).unwrap(), b"webm-bytes");
    assert_eq!(video.bytes, 10);
    let trace = out.trace.as_ref().unwrap();
    assert_eq!((trace.kind, trace.name.as_str()), (ArtifactKind::Trace, "explore-7.trace.zip"));
    assert_eq!(out.screenshots.len(), 1);
    assert_eq!(out.screenshots[0].name, "explore-7.screenshot-1.png");
    assert_eq!(out.har.as_ref().unwrap().name, "explore-7.network.har");
    assert_eq!(out.report.as_ref().unwrap().kind, ArtifactKind::Log);
    assert_eq!(out.artifacts().count(), 5, "error-context is not an artifact");

    // Invocation: config + script written to scratch, toolchain linked, CLI argv/env/timeout.
    let config = String::from_utf8(env.file(&EnvPath::scratch("pw/1/mage.config.mjs").unwrap()).unwrap()).unwrap();
    assert!(config.contains("grep: new RegExp(\"export\"),"));
    assert!(config.contains("baseURL: undefined,"), "no [browser] in the recipe");
    assert!(config.contains("video: 'on',"));
    assert_eq!(env.file(&EnvPath::scratch("pw/1/tests/repro.spec.ts").unwrap()).unwrap(), b"import { test } from '@playwright/test';");
    let execs = env.execs();
    assert_eq!(
        execs[0].command,
        Command::Argv(vec!["ln".into(), "-s".into(), "/opt/mage/playwright/node_modules".into(), "/fake/scratch/pw/1/node_modules".into()])
    );
    let cli = &execs[1];
    assert_eq!(
        cli.command,
        Command::Argv(vec![
            "/opt/mage/toolchains/node-20/bin/node".into(),
            "/opt/mage/playwright/node_modules/@playwright/test/cli.js".into(),
            "test".into(),
            "--config".into(),
            "/fake/scratch/pw/1/mage.config.mjs".into(),
        ])
    );
    assert_eq!(cli.role, ProcessRole::Playwright);
    assert_eq!(cli.cwd, EnvPath::scratch("pw/1").unwrap());
    assert_eq!(cli.timeout, Duration::from_secs(210), "globalTimeout + 30 s");
    assert!(cli.env.contains(&("PLAYWRIGHT_BROWSERS_PATH".into(), "/opt/mage/playwright/browsers".into())));
    assert!(cli.env.contains(&("NO_COLOR".into(), "1".into())));
}

#[tokio::test]
async fn recordings_off_and_inline_scripts() {
    let env = FakeEnv::empty();
    play(&env, FAILED, Exit::Code(1));
    let outbox = tempfile::tempdir().unwrap();
    let script = BrowserScript { source: ScriptSource::Inline { file_name: "repro.spec.ts".into(), bytes: b"pinned".to_vec() }, grep: None };
    let off = RecordOpts { video: false, trace: false, screenshots: ScreenshotMode::Off, har: false, ..opts(outbox.path()) };
    let out = runner().run(env.as_ref(), &script, &off).await.unwrap();
    assert_eq!(env.file(&EnvPath::scratch("pw/1/tests/repro.spec.ts").unwrap()).unwrap(), b"pinned");
    let config = String::from_utf8(env.file(&EnvPath::scratch("pw/1/mage.config.mjs").unwrap()).unwrap()).unwrap();
    assert!(config.contains("video: 'off',") && config.contains("screenshot: 'off',") && !config.contains("recordHar"));
    assert!(out.har.is_none(), "HAR not exported when off");
    assert_eq!(out.http_statuses, vec![500], "annotations still count");
}

#[tokio::test]
async fn base_url_comes_from_the_recipe() {
    use mage_core::domain::recipe::RecipeSpec;
    use crate::ports::executor::{BundleSource, Executor, ForkOptions, ForkPurpose, EgressProfile, NullObserver, PrepareRequest};
    use crate::testing::executor::FakeExecutor;
    use mage_core::domain::{CommitSha, OrgId, RepoId, RunId};

    let recipe = "version = 1\nports = [\"web\"]\n[services.web]\nrun = \"node s.js\"\nports = [\"web\"]\n[services.web.health]\ntcp = \"127.0.0.1:${port.web}\"\n[browser]\nbase_url = \"http://127.0.0.1:${port.web}\"\n";
    let exec = FakeExecutor::new();
    let req = PrepareRequest {
        org_id: OrgId::from_uuid(uuid::Uuid::from_u128(1)),
        repo_id: RepoId::from_uuid(uuid::Uuid::from_u128(2)),
        sha: CommitSha::from_bytes([1; 20]),
        recipe: Some(RecipeSpec::parse(recipe).unwrap()),
        source_bundle: BundleSource { url: "b".into(), sha256: [0; 32], bytes: 0 },
    };
    let base = exec.prepare(req, &NullObserver).await.unwrap();
    let opts_fork = ForkOptions { run_id: RunId::from_uuid(uuid::Uuid::from_u128(3)), purpose: ForkPurpose::Verify, egress: EgressProfile::Registries };
    let env_dyn = exec.fork(&base, opts_fork).await.unwrap();
    let env = exec.envs().pop().unwrap();
    play(&env, FAILED, Exit::Code(1));
    let outbox = tempfile::tempdir().unwrap();
    let script = BrowserScript { source: ScriptSource::Inline { file_name: "repro.spec.ts".into(), bytes: b"x".to_vec() }, grep: None };
    runner().run(env_dyn.as_ref(), &script, &opts(outbox.path())).await.unwrap();
    let port = env_dyn.ports().get("web").unwrap().port();
    let config = String::from_utf8(env.file(&EnvPath::scratch("pw/1/mage.config.mjs").unwrap()).unwrap()).unwrap();
    assert!(config.contains(&format!("baseURL: \"http://127.0.0.1:{port}\",")), "{config}");
}

#[tokio::test]
async fn crash_toolchain_and_script_errors() {
    let outbox = tempfile::tempdir().unwrap();
    let workspace = |p: &str| BrowserScript { source: ScriptSource::Workspace(RelPath::new(p).unwrap()), grep: None };

    // Top-level error in the report: Crashed with the syntax error.
    let env = workspace_env();
    play(&env, TOP_ERROR, Exit::Code(1));
    let out = runner().run(env.as_ref(), &workspace("e2e/repro.spec.ts"), &opts(outbox.path())).await.unwrap();
    assert_eq!(out.status, BrowserStatus::Crashed { exit: Exit::Code(1) });
    assert!(out.error_text.unwrap().starts_with("SyntaxError:"));

    // No report, non-zero exit: Crashed with the stderr tail.
    let env = workspace_env();
    env.on_exec(|spec| Some(Ok(if is_cli(spec) { outcome(Exit::Code(1), b"", b"Error: config is broken\n") } else { outcome(Exit::Code(0), b"", b"") })));
    let out = runner().run(env.as_ref(), &workspace("e2e/repro.spec.ts"), &opts(outbox.path())).await.unwrap();
    assert_eq!(out.status, BrowserStatus::Crashed { exit: Exit::Code(1) });
    assert_eq!(out.error_text.as_deref(), Some("Error: config is broken\n"));

    // No report after the exec timeout: TimedOut.
    let env = workspace_env();
    env.on_exec(|spec| Some(Ok(if is_cli(spec) { outcome(Exit::TimedOut, b"", b"") } else { outcome(Exit::Code(0), b"", b"") })));
    let out = runner().run(env.as_ref(), &workspace("e2e/repro.spec.ts"), &opts(outbox.path())).await.unwrap();
    assert_eq!(out.status, BrowserStatus::TimedOut);

    // Missing toolchain: Unavailable, not a script result.
    let env = workspace_env();
    env.on_exec(|spec| {
        Some(Ok(if is_cli(spec) {
            outcome(Exit::Code(1), b"", b"Error: Cannot find module '/opt/mage/playwright/node_modules/@playwright/test/cli.js'\n")
        } else {
            outcome(Exit::Code(0), b"", b"")
        }))
    });
    let err = runner().run(env.as_ref(), &workspace("e2e/repro.spec.ts"), &opts(outbox.path())).await.unwrap_err();
    assert!(matches!(err, BrowserError::Unavailable(_)), "{err:?}");

    let env = workspace_env();
    let err = runner().run(env.as_ref(), &workspace("e2e/missing.spec.ts"), &opts(outbox.path())).await.unwrap_err();
    assert!(matches!(err, BrowserError::NotFound(_)), "{err:?}");
    let err = runner().run(env.as_ref(), &workspace("e2e/repro.ts"), &opts(outbox.path())).await.unwrap_err();
    assert!(matches!(err, BrowserError::Invalid(_)), "{err:?}");
    assert!(env.execs().is_empty(), "nothing ran for bad scripts");
}

#[test]
fn attachment_paths_map_into_scratch() {
    assert_eq!(scratch_rel("/mage/scratch", "pw/2", "/mage/scratch/pw/2/out/x/video.webm").as_deref(), Some("pw/2/out/x/video.webm"));
    assert_eq!(scratch_rel("/mage/scratch/", "pw/2", "out/x/trace.zip").as_deref(), Some("pw/2/out/x/trace.zip"));
    assert_eq!(scratch_rel("/mage/scratch", "pw/2", "/etc/passwd"), None);
    assert_eq!(scratch_rel("/mage/scratch", "pw/2", "/mage/scratch/../ws/x"), None);
    assert_eq!(scratch_rel("/mage/scratch", "pw/2", "/mage/scratchy/x"), None);
}

/// Real Playwright (opt-in: `MAGE_TEST_PLAYWRIGHT=1` and `MAGE_TEST_PLAYWRIGHT_ROOT=<dir with
/// node_modules/@playwright/test 1.57.0>`; Chromium from Playwright's default browser cache or
/// `<root>/browsers`): runs the generated config with the pinned CLI on a passing and a failing
/// spec, then parses the real `report.json` the way `run()` does.
#[tokio::test]
async fn real_playwright_runs_the_generated_config() {
    if std::env::var("MAGE_TEST_PLAYWRIGHT").as_deref() != Ok("1") {
        eprintln!("skipped: set MAGE_TEST_PLAYWRIGHT=1 and MAGE_TEST_PLAYWRIGHT_ROOT to run against the pinned toolchain");
        return;
    }
    let root = PathBuf::from(std::env::var("MAGE_TEST_PLAYWRIGHT_ROOT").expect("MAGE_TEST_PLAYWRIGHT_ROOT"));
    let cases = [
        ("pass.spec.ts", "await page.setContent('<h1>hi</h1>');\n  await expect(page.locator('h1')).toHaveText('hi');", BrowserStatus::Passed),
        ("fail.spec.ts", "await page.setContent('<h1>hi</h1>');\n  await expect(page.locator('h1')).toHaveText('bye', { timeout: 500 });", BrowserStatus::Failed),
    ];
    for (file, body, want) in cases {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("tests")).unwrap();
        let spec = format!("import {{ test, expect }} from '@playwright/test';\ntest('case', async ({{ page }}) => {{\n  {body}\n}});\n");
        std::fs::write(dir.path().join("tests").join(file), spec).unwrap();
        std::os::unix::fs::symlink(root.join("node_modules"), dir.path().join("node_modules")).unwrap();
        let cfg = render_config(&opts(dir.path()), &ConfigInputs { base_url: None, grep: None, chromium_sandbox: true });
        std::fs::write(dir.path().join(config::CONFIG_FILE), cfg).unwrap();
        let mut cmd = tokio::process::Command::new("node");
        cmd.arg(root.join("node_modules/@playwright/test/cli.js")).args(["test", "--config"]).arg(dir.path().join(config::CONFIG_FILE));
        cmd.current_dir(dir.path()).env("NO_COLOR", "1").env("FORCE_COLOR", "0");
        if root.join("browsers").is_dir() {
            cmd.env("PLAYWRIGHT_BROWSERS_PATH", root.join("browsers"));
        }
        let out = cmd.output().await.unwrap();
        let exit = Exit::Code(out.status.code().unwrap_or(-1));
        let report = std::fs::read(dir.path().join(config::REPORT_FILE)).expect("report.json written");
        let summary = parse_report(&report).unwrap();
        assert_eq!(status_of(&summary, &exit, false), want, "{file}: {}", String::from_utf8_lossy(&out.stdout));
        let names: Vec<&str> = summary.attachments.iter().map(|a| a.name.as_str()).collect();
        assert!(names.contains(&"video") && names.contains(&"trace"), "{file}: {names:?}");
        assert!(dir.path().join(config::HAR_FILE).is_file(), "{file}: HAR recorded by the test runner");
        if want == BrowserStatus::Failed {
            assert!(report::error_text(&summary).unwrap().contains("toHaveText"));
        }
    }
}
