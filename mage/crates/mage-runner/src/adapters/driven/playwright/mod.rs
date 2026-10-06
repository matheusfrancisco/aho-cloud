//! `BrowserRunner` on the pinned Playwright toolchain (executor.md "Playwright adapter").
//!
//! Every step goes through the `Env` (write the generated config and the script under
//! `<scratch>/pw/<n>/`, `exec` the Playwright CLI as `ProcessRole::Playwright`, `read` the JSON
//! report, `export` recordings to the outbox), so the adapter never touches `exec_host`
//! internals and works unchanged on every executor. The repo's own `node_modules` is never
//! used: the invocation dir links `<root>/node_modules`.

mod config;
mod report;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use mage_core::domain::{ArtifactKind, ScriptKind};
use serde::Deserialize;

pub use self::config::{ConfigInputs, render_config};
pub use self::report::{ReportSummary, har_error_statuses, merge_statuses, parse_report, status_of};
use crate::domain::observe::strip_ansi;
use crate::ports::browser::{BrowserError, BrowserOutcome, BrowserRunner, BrowserScript, BrowserStatus, LocalArtifact, RecordOpts, ScriptSource};
use crate::ports::executor::{Area, Env, EnvPath, ExecError, ExecOutcome, ExecSpec, Exit, ProcessRole, ReadRange, RelPath, WriteMode};

/// Largest repro script accepted (executor.md `submit_diagnosis`).
pub const MAX_SCRIPT_BYTES: u64 = 256 * 1024;
/// Largest `report.json` read back.
const MAX_REPORT_BYTES: u64 = 32 * 1024 * 1024;
/// Largest HAR parsed for statuses (content is omitted, so this is generous).
const MAX_HAR_BYTES: u64 = 64 * 1024 * 1024;
/// Margin the exec timeout gets over `globalTimeout`, so Playwright reports its own timeout.
const EXEC_MARGIN: Duration = Duration::from_secs(30);
/// `stdout_tail` cap.
const STDOUT_TAIL_BYTES: usize = 4 * 1024;

/// `[runner.playwright]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PlaywrightConfig {
    /// Provisioned toolchain dir (`package.json`, `node_modules/`, `browsers/`). `None` (dev
    /// only): `npx --no-install playwright test`, browsers from Playwright's default cache.
    pub root: Option<PathBuf>,
    /// Node binary; `None` uses `node` from the env's `PATH`.
    pub node: Option<PathBuf>,
    /// `false` sets `launchOptions.chromiumSandbox = false` (when Chromium's sandbox cannot start
    /// inside the enforced-mode namespaces).
    pub chromium_sandbox: bool,
}

impl Default for PlaywrightConfig {
    fn default() -> Self {
        Self {
            root: Some(PathBuf::from("/opt/mage/playwright")),
            node: Some(PathBuf::from("/opt/mage/toolchains/node-20/bin/node")),
            chromium_sandbox: true,
        }
    }
}

pub struct PlaywrightRunner {
    config: PlaywrightConfig,
    /// Invocation counter: `<scratch>/pw/<n>/` is unique per process.
    seq: AtomicU64,
}

impl PlaywrightRunner {
    pub fn new(config: PlaywrightConfig) -> Self {
        Self { config, seq: AtomicU64::new(0) }
    }

    /// The Playwright CLI argv for one invocation.
    fn command(&self, config_path: &str) -> Vec<String> {
        match &self.config.root {
            Some(root) => vec![
                self.config.node.as_ref().map_or_else(|| "node".to_owned(), |n| n.display().to_string()),
                root.join("node_modules/@playwright/test/cli.js").display().to_string(),
                "test".to_owned(),
                "--config".to_owned(),
                config_path.to_owned(),
            ],
            None => vec![
                "npx".to_owned(),
                "--no-install".to_owned(),
                "playwright".to_owned(),
                "test".to_owned(),
                "--config".to_owned(),
                config_path.to_owned(),
            ],
        }
    }

    fn command_env(&self) -> Vec<(String, String)> {
        let mut env = vec![
            ("NO_COLOR".to_owned(), "1".to_owned()),
            ("FORCE_COLOR".to_owned(), "0".to_owned()),
            ("PW_TEST_HTML_REPORT_OPEN".to_owned(), "never".to_owned()),
        ];
        if let Some(root) = &self.config.root {
            env.push(("PLAYWRIGHT_BROWSERS_PATH".to_owned(), root.join("browsers").display().to_string()));
        }
        env
    }

    async fn script_bytes(env: &dyn Env, source: &ScriptSource) -> Result<Vec<u8>, BrowserError> {
        match source {
            ScriptSource::Inline { bytes, .. } => {
                if bytes.len() as u64 > MAX_SCRIPT_BYTES {
                    return Err(BrowserError::Invalid(format!("script is larger than {MAX_SCRIPT_BYTES} bytes")));
                }
                Ok(bytes.clone())
            }
            ScriptSource::Workspace(rel) => {
                let path = EnvPath::new(Area::Workspace, rel.clone());
                let read = env.read(&path, ReadRange::Whole { max_bytes: MAX_SCRIPT_BYTES }).await.map_err(|e| match e {
                    ExecError::NotFound(_) => BrowserError::NotFound(format!("script {rel} does not exist")),
                    e => BrowserError::Env(e),
                })?;
                if read.truncated || read.total_bytes > MAX_SCRIPT_BYTES {
                    return Err(BrowserError::Invalid(format!("script {rel} is larger than {MAX_SCRIPT_BYTES} bytes")));
                }
                if read.binary {
                    return Err(BrowserError::Invalid(format!("script {rel} is not a text file")));
                }
                Ok(read.bytes)
            }
        }
    }

    /// `Env::render(recipe.browser.base_url)`, when the recipe has a `[browser]` table.
    fn base_url(env: &dyn Env) -> Result<Option<String>, BrowserError> {
        let Some(browser) = env.recipe().and_then(|r| r.browser) else { return Ok(None) };
        env.render(&browser.base_url).map(Some).map_err(BrowserError::Env)
    }

    /// Exports `rel` (scratch) to `<outbox>/<name>` and describes it.
    async fn export(
        env: &dyn Env,
        rel: &str,
        outbox: &Path,
        kind: ArtifactKind,
        name: String,
        content_type: &str,
    ) -> Result<Option<LocalArtifact>, BrowserError> {
        let src = EnvPath::scratch(rel)?;
        let dest = outbox.join(&name);
        match env.export(&src, &dest).await {
            Ok(report) => Ok(Some(LocalArtifact {
                kind,
                name,
                path: dest,
                bytes: report.bytes,
                sha256: report.sha256,
                content_type: content_type.to_owned(),
            })),
            Err(ExecError::NotFound(_)) => Ok(None),
            Err(e) => Err(BrowserError::Env(e)),
        }
    }
}

#[async_trait]
impl BrowserRunner for PlaywrightRunner {
    async fn run(&self, env: &dyn Env, script: &BrowserScript, opts: &RecordOpts) -> Result<BrowserOutcome, BrowserError> {
        let file_name = script.source.file_name().to_owned();
        if ScriptKind::from_file_name(&file_name) != Some(ScriptKind::Playwright) {
            return Err(BrowserError::Invalid(format!(
                "{file_name} is not a Playwright script (*.spec.ts, *.spec.js, *.spec.mjs, *.test.ts, *.test.js)"
            )));
        }
        // A file name must be one path segment.
        let test_rel = RelPath::new(&format!("{}/{}", config::TESTS_DIR, file_name))
            .map_err(|_| BrowserError::Invalid(format!("bad script file name {file_name:?}")))?;
        if test_rel.segments().count() != 2 {
            return Err(BrowserError::Invalid(format!("bad script file name {file_name:?}")));
        }
        let bytes = Self::script_bytes(env, &script.source).await?;
        let base_url = Self::base_url(env)?;

        let n = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let inv = format!("pw/{n}");
        let inv_abs = format!("{}/{inv}", env.scratch_root().trim_end_matches('/'));
        let rendered = render_config(
            opts,
            &ConfigInputs { base_url: base_url.as_deref(), grep: script.grep.as_deref(), chromium_sandbox: self.config.chromium_sandbox },
        );
        env.write(&EnvPath::scratch(&format!("{inv}/{}", config::CONFIG_FILE))?, rendered.as_bytes(), WriteMode::Overwrite).await?;
        env.write(&EnvPath::scratch(&format!("{inv}/{}", test_rel.as_str()))?, &bytes, WriteMode::Overwrite).await?;

        if let Some(root) = &self.config.root {
            // `Env::write` makes regular files only.
            let mut link = ExecSpec::argv(
                vec![
                    "ln".to_owned(),
                    "-s".to_owned(),
                    root.join("node_modules").display().to_string(),
                    format!("{inv_abs}/node_modules"),
                ],
                ProcessRole::Playwright,
            );
            link.cwd = EnvPath::scratch(&inv)?;
            link.timeout = Duration::from_secs(30);
            let out = env.exec(link, None).await?;
            if !out.exit.success() {
                return Err(BrowserError::Unavailable(format!(
                    "linking the Playwright toolchain failed ({}): {}",
                    out.exit,
                    text_tail(&out.stderr.head, &out.stderr.tail, 512)
                )));
            }
        }

        let mut spec = ExecSpec::argv(self.command(&format!("{inv_abs}/{}", config::CONFIG_FILE)), ProcessRole::Playwright);
        spec.cwd = EnvPath::scratch(&inv)?;
        spec.env = self.command_env();
        spec.timeout = opts.timeout + EXEC_MARGIN;
        let outcome = env.exec(spec, None).await?;

        let report_path = EnvPath::scratch(&format!("{inv}/{}", config::REPORT_FILE))?;
        let summary = match env.read(&report_path, ReadRange::Whole { max_bytes: MAX_REPORT_BYTES }).await {
            Ok(read) if !read.truncated => Some(
                parse_report(&read.bytes).map_err(|e| BrowserError::Env(ExecError::Invalid(e.to_string())))?,
            ),
            Ok(_) => return Err(BrowserError::Env(ExecError::Invalid("report.json is larger than 32 MiB".into()))),
            Err(ExecError::NotFound(_)) => None,
            Err(e) => return Err(BrowserError::Env(e)),
        };

        tokio::fs::create_dir_all(&opts.outbox).await.map_err(|e| BrowserError::Env(ExecError::from(e)))?;
        assemble(env, &inv, opts, &outcome, summary).await
    }
}

/// Builds the outcome from the exec result and the parsed report, exporting recordings.
async fn assemble(
    env: &dyn Env,
    inv: &str,
    opts: &RecordOpts,
    outcome: &ExecOutcome,
    summary: Option<ReportSummary>,
) -> Result<BrowserOutcome, BrowserError> {
    let stdout_tail = text_tail(&outcome.stdout.head, &outcome.stdout.tail, STDOUT_TAIL_BYTES);
    let exec_timed_out = outcome.exit == Exit::TimedOut;
    let Some(summary) = summary else {
        let stderr = text_tail(&outcome.stderr.head, &outcome.stderr.tail, report::ERROR_TEXT_MAX_BYTES);
        if toolchain_missing(&outcome.exit, &stderr) || toolchain_missing(&outcome.exit, &stdout_tail) {
            return Err(BrowserError::Unavailable(format!("Playwright toolchain is not installed: {}", first_line(&stderr))));
        }
        let status = if exec_timed_out { BrowserStatus::TimedOut } else { BrowserStatus::Crashed { exit: outcome.exit } };
        let error_text = if stderr.trim().is_empty() {
            format!("Playwright produced no report ({})", outcome.exit)
        } else {
            stderr
        };
        return Ok(BrowserOutcome {
            passed: false,
            status,
            error_text: Some(error_text),
            failures: Vec::new(),
            video: None,
            trace: None,
            screenshots: Vec::new(),
            har: None,
            report: None,
            http_statuses: Vec::new(),
            stdout_tail,
            duration: outcome.duration,
        });
    };

    if let Some(missing) = summary.failures.iter().find(|f| f.message.contains("Executable doesn't exist")) {
        return Err(BrowserError::Unavailable(format!("Playwright browsers are not installed: {}", first_line(&missing.message))));
    }

    let status = status_of(&summary, &outcome.exit, exec_timed_out);
    let label = &opts.label;
    let mut video = None;
    let mut trace = None;
    let mut screenshots = Vec::new();
    for att in &summary.attachments {
        let Some(rel) = scratch_rel(env.scratch_root(), inv, &att.path) else { continue };
        match (att.name.as_str(), att.content_type.as_str()) {
            ("video", "video/webm") if video.is_none() => {
                video = export_scratch(env, &rel, opts, ArtifactKind::Video, format!("{label}.video.webm"), "video/webm").await?;
            }
            ("trace", "application/zip") if trace.is_none() => {
                trace = export_scratch(env, &rel, opts, ArtifactKind::Trace, format!("{label}.trace.zip"), "application/zip").await?;
            }
            ("screenshot", "image/png") => {
                let name = format!("{label}.screenshot-{}.png", screenshots.len() + 1);
                if let Some(shot) = export_scratch(env, &rel, opts, ArtifactKind::Screenshot, name, "image/png").await? {
                    screenshots.push(shot);
                }
            }
            _ => {}
        }
    }

    let har = if opts.har {
        export_scratch(env, &format!("{inv}/{}", config::HAR_FILE), opts, ArtifactKind::Log, format!("{label}.network.har"), "application/json")
            .await?
    } else {
        None
    };
    let har_statuses = match &har {
        Some(local) if local.bytes <= MAX_HAR_BYTES => match tokio::fs::read(&local.path).await {
            Ok(bytes) => har_error_statuses(&bytes),
            Err(e) => return Err(BrowserError::Env(ExecError::from(e))),
        },
        _ => Vec::new(),
    };
    let report = export_scratch(
        env,
        &format!("{inv}/{}", config::REPORT_FILE),
        opts,
        ArtifactKind::Log,
        format!("{label}.report.json"),
        "application/json",
    )
    .await?;

    Ok(BrowserOutcome {
        passed: status == BrowserStatus::Passed,
        error_text: if status == BrowserStatus::Passed { None } else { report::error_text(&summary) },
        status,
        failures: summary.failures.clone(),
        video,
        trace,
        screenshots,
        har,
        report,
        http_statuses: merge_statuses(&har_statuses, &summary.annotated_statuses),
        stdout_tail,
        duration: outcome.duration,
    })
}

async fn export_scratch(
    env: &dyn Env,
    rel: &str,
    opts: &RecordOpts,
    kind: ArtifactKind,
    name: String,
    content_type: &str,
) -> Result<Option<LocalArtifact>, BrowserError> {
    PlaywrightRunner::export(env, rel, &opts.outbox, kind, name, content_type).await
}

/// Scratch-relative path of a report attachment. Playwright writes absolute paths as env
/// processes see them (`<scratch_root>/pw/<n>/out/...`); relative paths are relative to the
/// invocation dir. Anything outside the scratch root is ignored.
fn scratch_rel(scratch_root: &str, inv: &str, path: &str) -> Option<String> {
    let root = scratch_root.trim_end_matches('/');
    let rel = if let Some(rest) = path.strip_prefix(root).and_then(|r| r.strip_prefix('/')) {
        rest.to_owned()
    } else if !path.starts_with('/') {
        format!("{inv}/{path}")
    } else {
        return None;
    };
    RelPath::new(&rel).ok().map(|r| r.as_str().to_owned())
}

/// Missing Node or `@playwright/test`: an infrastructure problem, not a script result.
fn toolchain_missing(exit: &Exit, text: &str) -> bool {
    *exit == Exit::Code(127)
        || text.contains("Cannot find module '@playwright/test")
        || text.contains("Cannot find package '@playwright/test'")
        || text.contains("node_modules/@playwright/test/cli.js")
}

fn first_line(text: &str) -> &str {
    text.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim()
}

/// Last `max` bytes of a captured stream (head + tail), lossy UTF-8, ANSI stripped.
fn text_tail(head: &[u8], tail: &[u8], max: usize) -> String {
    let mut bytes = Vec::with_capacity(head.len() + tail.len());
    bytes.extend_from_slice(head);
    bytes.extend_from_slice(tail);
    let text = String::from_utf8_lossy(&bytes);
    let text = strip_ansi(&text).into_owned();
    if text.len() <= max {
        return text;
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_owned()
}

#[cfg(test)]
mod tests;
