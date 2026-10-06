//! `submit_triage`, `submit_recipe`, `submit_diagnosis`, `submit_fix` (executor.md "Tool schemas",
//! runs.md "Submit handling", fix.md "`submit_fix`"). Local checks first (no gate attempt is
//! consumed by a local rejection), then uploads, then `ServerLink::submit_result`; the outcome
//! goes to the session task. Submits are serialized; while a gate is pending they are refused.

use std::time::Duration;

use mage_core::domain::fix::{Patch, PatchPolicy};
use mage_core::domain::recipe::RecipeSpec;
use mage_core::domain::{
    ArtifactKind, Citation, DiagnoseResult, FailureSignature, FixResult, LineRange, Matcher, RecipeProposal, RepoPath, RunStatus, ScriptKind,
    SignatureKind,
};
use mage_core::wire::mcp::{SubmitDiagnosisInput, SubmitFixInput, SubmitRecipeInput};
use mage_core::wire::runner::{Rejection, SubmitAck, TriageSubmission, WireRunResult};
use serde_json::{Value, json};
use tokio::sync::SemaphorePermit;

use super::workspace::{RECIPE_PATH, issue_json, render_issues};
use super::{RunTool, RunTools, SubmitOutcome, ToolError, ToolOut, artifact_name};
use crate::app::runner::verify::{observe_browser, observe_exec, service_cursors, service_lines_since};
use crate::ports::browser::{BrowserScript, RecordOpts, ScriptSource};
use crate::ports::executor::{
    Area, Baseline, DiffOptions, Env, EnvPath, ExecError, ExecSpec, PatchOutput, PathGlob, ProcessRole, ReadRange, RelPath,
};
use crate::ports::link::{LinkConflict, LinkError};

/// `submit_diagnosis.script` cap.
const SCRIPT_MAX_BYTES: u64 = 256 * 1024;
/// Playwright precheck invocation timeout (the generated config's default `globalTimeout`).
const PRECHECK_PLAYWRIGHT_TIMEOUT: Duration = Duration::from_secs(180);

fn render_rejection(r: &Rejection) -> String {
    let mut out = r.message.clone();
    for item in &r.items {
        out.push_str(&format!("\n- {}: {}", item.path, item.reason));
    }
    out
}

impl RunTools {
    /// Serializes submits and refuses them while a gate is pending.
    async fn begin_submit(&self) -> Result<(SemaphorePermit<'_>, u32), ToolError> {
        let permit = self.submit_slot.acquire().await.map_err(|_| ToolError::new("the run is shutting down"))?;
        let mut state = self.state.lock();
        if state.gate_pending {
            return Err(ToolError::new("gate in progress: wait for its result before submitting again"));
        }
        state.submit_seq += 1;
        Ok((permit, state.submit_seq))
    }

    /// `POST /result` with a fresh submission id; maps the answer for the agent and reports it.
    async fn post_result(&self, tool: RunTool, result: WireRunResult) -> Result<SubmitAck, ToolError> {
        let submission_id = self.deps.ids.uuid_v4();
        match self.deps.link.submit_result(self.run_id, self.attempt, submission_id, &result).await {
            Ok(ack) => {
                if ack.status == RunStatus::AwaitingGate {
                    self.state.lock().gate_pending = true;
                }
                self.report(SubmitOutcome::Accepted { ack: ack.clone(), result });
                Ok(ack)
            }
            Err(LinkError::Rejected(rejection)) => {
                let message = render_rejection(&rejection);
                self.report(SubmitOutcome::Rejected(rejection));
                Err(ToolError::new(message))
            }
            Err(LinkError::Conflict(conflict)) => {
                self.report(SubmitOutcome::Conflict(conflict));
                Err(ToolError::new(match conflict {
                    LinkConflict::InvalidState(_) => "run no longer accepts results",
                    _ => "run cancelled",
                }))
            }
            Err(LinkError::Unavailable(_)) => Err(ToolError::new(format!("submission service unavailable, call {} again", tool.name()))),
            Err(e) => Err(ToolError::new(format!("submission failed: {e}"))),
        }
    }

    pub(super) async fn submit_triage(&self, input: TriageSubmission) -> ToolOut {
        let _submit = self.begin_submit().await?;
        let ack = self.post_result(RunTool::SubmitTriage, WireRunResult::Triage(input)).await?;
        Ok(Value::String(ack.message))
    }

    pub(super) async fn submit_recipe(&self, input: SubmitRecipeInput) -> ToolOut {
        let env = self.env()?;
        let _submit = self.begin_submit().await?;
        if input.toml.len() > mage_core::domain::recipe::RECIPE_MAX_BYTES {
            return Err(ToolError::new("toml is larger than 64 KiB"));
        }
        let spec = RecipeSpec::parse(&input.toml).map_err(|e| ToolError::new(format!("recipe does not parse: {e}")))?;
        let report = spec.validate();
        if !report.errors.is_empty() {
            return Err(ToolError::new(format!(
                "recipe has errors (no gate attempt used):\n{}",
                render_issues(&report.errors)
            )));
        }
        let mut warnings: Vec<Value> = report.warnings.iter().map(|w| issue_json(w, "warning")).collect();
        let workspace = match env.read(&EnvPath::ws(RECIPE_PATH)?, ReadRange::Whole { max_bytes: 64 * 1024 + 1 }).await {
            Ok(read) => Some(read.bytes),
            Err(ExecError::NotFound(_)) => None,
            Err(e) => return Err(e.into()),
        };
        if workspace.as_deref() != Some(input.toml.as_bytes()) {
            warnings.push(json!({
                "path": RECIPE_PATH,
                "code": "workspace_out_of_sync",
                "message": format!("the submitted toml differs from the workspace {RECIPE_PATH}; keep them in sync"),
                "severity": "warning",
            }));
        }
        let recipe_hash = spec.hash();
        let ack = self
            .post_result(RunTool::SubmitRecipe, WireRunResult::Learn(RecipeProposal { toml: input.toml, notes: input.notes }))
            .await?;
        Ok(json!({ "submitted": true, "recipe_hash": recipe_hash, "warnings": warnings, "message": ack.message }))
    }

    pub(super) async fn submit_diagnosis(&self, input: SubmitDiagnosisInput) -> ToolOut {
        let env = self.env()?;
        let (_submit, seq) = self.begin_submit().await?;
        let verdict = input.verdict().map_err(ToolError::new)?;
        let reproduced = matches!(verdict, mage_core::domain::Verdict::Reproduced);

        let script = match &input.script {
            Some(path) => Some(load_script(env.as_ref(), path).await?),
            None => None,
        };
        if let Some(signature) = &input.signature {
            check_signature(signature, script.as_ref().map(|s| s.kind))?;
        }
        if reproduced && let (Some(script), Some(signature)) = (&script, &input.signature) {
            self.precheck(env.as_ref(), script, signature, seq).await?;
        }
        let fix_sites = self.fix_sites(&input)?;

        let script_artifact = match &script {
            Some(s) => {
                let base = artifact_name(s.rel.file_name());
                let name = if seq == 1 { base } else { format!("s{seq}-{base}") };
                Some(self.upload_bytes(ArtifactKind::Script, &name, "text/plain", &s.bytes).await?)
            }
            None => None,
        };
        let result = DiagnoseResult {
            verdict,
            script: script_artifact,
            signature: input.signature.clone(),
            summary: input.summary.clone(),
            suggested_fix: input.suggested_fix.clone(),
            fix_sites,
        };
        result.validate().map_err(|e| ToolError::new(e.to_string()))?;
        let ack = self.post_result(RunTool::SubmitDiagnosis, WireRunResult::Diagnose(result)).await?;
        Ok(json!({
            "submitted": true,
            "gate": ack.gate.map(|g| g.kind),
            "script_artifact": script_artifact,
            "message": ack.message,
        }))
    }

    /// Runs the script once here, unrecorded, and requires the signature among the observations.
    async fn precheck(&self, env: &dyn Env, script: &LoadedScript, signature: &FailureSignature, seq: u32) -> Result<(), ToolError> {
        let cursors = if signature.kind == SignatureKind::ErrorString { service_cursors(env).await } else { Vec::new() };
        let observed = match script.kind {
            ScriptKind::Playwright => {
                let label = {
                    let mut state = self.state.lock();
                    state.precheck_seq += 1;
                    format!("precheck-{seq}-{}", state.precheck_seq)
                };
                let opts = RecordOpts::none(PRECHECK_PLAYWRIGHT_TIMEOUT, self.outbox.clone(), label);
                let run = BrowserScript { source: ScriptSource::Workspace(script.rel.clone()), grep: None };
                let outcome = self.deps.browser.run(env, &run, &opts).await?;
                for local in outcome.artifacts() {
                    super::remove_quietly(&local.path).await;
                }
                let lines = service_lines_since(env, &cursors).await;
                observe_browser(signature, &outcome, &lines)
            }
            ScriptKind::Shell => {
                let mut spec = ExecSpec::argv(
                    vec!["bash".to_owned(), format!("{}/{}", env.root().trim_end_matches('/'), script.rel.as_str())],
                    ProcessRole::Precheck,
                );
                spec.timeout = Duration::from_secs(self.config.script_timeout_s);
                let outcome = env.exec(spec, None).await?;
                let lines = service_lines_since(env, &cursors).await;
                observe_exec(signature, &outcome, &lines)
            }
        };
        if signature.matches_any(&observed) {
            return Ok(());
        }
        let values: Vec<&str> = observed.iter().map(|o| o.value.as_str()).collect();
        Err(ToolError::new(format!(
            "signature not observed when your script ran here; observed: {}",
            serde_json::to_string(&values).unwrap_or_default()
        )))
    }

    fn fix_sites(&self, input: &SubmitDiagnosisInput) -> Result<Vec<Citation>, ToolError> {
        if input.fix_sites.is_empty() {
            return Ok(Vec::new());
        }
        let (Some(repo_id), Some(sha)) = (self.repo_id, self.sha) else {
            return Err(ToolError::new("fix_sites need a repository run"));
        };
        input
            .fix_sites
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let path = RepoPath::new(c.path.clone()).map_err(|e| ToolError::new(format!("fix_sites[{i}].path: {e}")))?;
                let lines = LineRange::new(c.start_line, c.end_line).map_err(|e| ToolError::new(format!("fix_sites[{i}]: {e}")))?;
                Ok(Citation { repo_id, sha, path, lines, snippet_hash: None, note: c.note.clone() })
            })
            .collect()
    }

    pub(super) async fn submit_fix(&self, input: SubmitFixInput) -> ToolOut {
        let env = self.env()?;
        let (_submit, seq) = self.begin_submit().await?;
        let policy = &self.fix.as_ref().ok_or_else(|| ToolError::new("this run has no fix spec"))?.policy;
        let out = collect_patch(env.as_ref(), policy).await?;
        let patch = check_patch_output(&out, policy)?;
        let name = format!("fix-{seq}.patch");
        let patch_artifact = self.upload_bytes(ArtifactKind::Patch, &name, "text/x-diff", &out.patch).await?;
        let excluded: Vec<RepoPath> = out.excluded.iter().filter_map(|p| RepoPath::new(p.as_str().to_owned()).ok()).collect();
        let files: Vec<Value> = patch.files().iter().map(|f| json!({ "path": f.path(), "change": f.change() })).collect();
        let result = FixResult { patch: patch_artifact, summary: input.summary, excluded: excluded.clone(), stats: patch.stats() };
        let ack = self.post_result(RunTool::SubmitFix, WireRunResult::Fix(result)).await?;
        Ok(json!({
            "submitted": true,
            "patch_artifact": patch_artifact,
            "files": files,
            "excluded": excluded,
            "message": ack.message,
        }))
    }
}

/// A repro script read from the workspace.
struct LoadedScript {
    rel: RelPath,
    kind: ScriptKind,
    bytes: Vec<u8>,
}

/// Validation step 1: the script resolves inside the workspace, exists, is ≤ 256 KiB, and has a
/// known extension.
async fn load_script(env: &dyn Env, path: &str) -> Result<LoadedScript, ToolError> {
    let rel = RelPath::new(path)?;
    let kind = ScriptKind::from_file_name(rel.as_str()).ok_or_else(|| {
        ToolError::new("script must be *.spec.ts, *.spec.js, *.spec.mjs, *.test.ts, *.test.js (Playwright) or *.sh (shell)")
    })?;
    let read = env
        .read(&EnvPath::new(Area::Workspace, rel.clone()), ReadRange::Whole { max_bytes: SCRIPT_MAX_BYTES })
        .await
        .map_err(|e| match e {
            ExecError::NotFound(_) => ToolError::new(format!("script {rel} does not exist")),
            e => ToolError::from(e),
        })?;
    if read.truncated || read.total_bytes > SCRIPT_MAX_BYTES {
        return Err(ToolError::new(format!("script {rel} is larger than 256 KiB")));
    }
    if read.binary {
        return Err(ToolError::new(format!("script {rel} is not a text file")));
    }
    Ok(LoadedScript { rel, kind, bytes: read.bytes })
}

/// Validation step 2: well-formed and compatible with the script kind.
fn check_signature(signature: &FailureSignature, kind: Option<ScriptKind>) -> Result<(), ToolError> {
    signature.validate().map_err(|e| ToolError::new(format!("signature: {e}")))?;
    if let Some(kind) = kind
        && !kind.supports(signature.kind)
    {
        return Err(ToolError::new(match signature.kind {
            SignatureKind::AssertionText => "signature kind assertion_text needs a Playwright script",
            SignatureKind::ExitCode => "signature kind exit_code needs a shell script",
            _ => "signature kind does not fit the script",
        }));
    }
    if signature.kind == SignatureKind::HttpStatus {
        let three_digits = signature.value.len() == 3 && signature.value.bytes().all(|b| b.is_ascii_digit());
        let in_range = signature.value.parse::<u16>().is_ok_and(|s| (100..=599).contains(&s));
        if !three_digits || !in_range || signature.matcher != Matcher::Exact {
            return Err(ToolError::new("http_status signatures are three digits in 100..=599 with matcher exact"));
        }
    }
    Ok(())
}

/// fix.md `submit_fix` step 2: the diff against the prepared baseline with the policy's
/// exclusions. Lockfiles under `WithManifest` depend on the changed set, so a second diff drops
/// them once the first one showed what changed.
pub async fn collect_patch(env: &dyn Env, policy: &PatchPolicy) -> Result<PatchOutput, ToolError> {
    let mut exclude = policy.exclude_globs().into_iter().map(PathGlob::new).collect::<Result<Vec<_>, _>>()?;
    let first = env.diff_against_base(&DiffOptions { against: Baseline::Prepared, exclude: exclude.clone() }).await?;
    let changed: Vec<RepoPath> = first.files.iter().filter_map(|f| RepoPath::new(f.path.as_str().to_owned()).ok()).collect();
    let late: Vec<PathGlob> = policy
        .exclusions(&changed)
        .into_iter()
        .filter(|x| !exclude.iter().any(|g| g.matches(x.path.as_str())))
        .filter_map(|x| PathGlob::new(x.path.as_str()).ok())
        .collect();
    if late.is_empty() {
        return Ok(first);
    }
    exclude.extend(late);
    let mut second = env.diff_against_base(&DiffOptions { against: Baseline::Prepared, exclude }).await?;
    for path in first.excluded {
        if !second.excluded.contains(&path) {
            second.excluded.push(path);
        }
    }
    second.excluded.sort();
    Ok(second)
}

/// fix.md `submit_fix` step 3 (local fast checks mirroring the server's `PatchPolicy`): empty
/// diff, binary files, symlinks / gitlinks / odd modes (`Patch::parse`), deny paths and limits.
pub fn check_patch_output(out: &PatchOutput, policy: &PatchPolicy) -> Result<Patch, ToolError> {
    if !out.binary.is_empty() {
        let paths: Vec<&str> = out.binary.iter().map(RelPath::as_str).collect();
        return Err(ToolError::new(format!("binary files cannot be submitted; undo them: {}", paths.join(", "))));
    }
    if out.patch.is_empty() || out.files.is_empty() {
        let mut message = "no changes: the workspace equals the prepared baseline".to_owned();
        if !out.excluded.is_empty() {
            let paths: Vec<&str> = out.excluded.iter().map(RelPath::as_str).collect();
            message.push_str(&format!(" after dropping excluded paths ({})", paths.join(", ")));
        }
        return Err(ToolError::new(message));
    }
    let patch = Patch::parse(&out.patch).map_err(|e| ToolError::new(format!("the change cannot be submitted: {e}")))?;
    policy.check(&patch, None).map_err(|v| {
        let lines: Vec<String> = v.0.iter().map(|x| format!("- {x}")).collect();
        ToolError::new(format!("the change breaks the patch policy; undo these before submitting:\n{}", lines.join("\n")))
    })?;
    Ok(patch)
}

