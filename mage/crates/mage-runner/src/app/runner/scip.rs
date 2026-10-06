//! Learn post-gate SCIP step (executor.md UC5, runs.md "Learn post-gate SCIP step"). Runs after
//! `GateFeedback { Boot, Passed }` on a fresh `Scip` fork of the proposed recipe's base: each
//! configured `[[scip]]` indexer (or the runner default for a detected language) writes
//! `<scratch>/scip/<language>.scip`, which is exported and uploaded as `ArtifactKind::ScipIndex`
//! named `<language>.scip`; the server enqueues `index.ingest_scip` per artifact. A failure here
//! is a `Notice`, never a run failure.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mage_core::domain::recipe::ScipSpec;
use mage_core::domain::{ArtifactId, ArtifactKind, RunId};
use mage_core::wire::runner::OutboundEventKind;
use serde::Deserialize;
use tokio::sync::mpsc;

use crate::app::runner::artifacts::upload_file;
use crate::app::runner::tools::artifact_name;
use crate::app::runner::tools::shape::{clean_bytes, tail_bytes};
use crate::ports::executor::{
    BaseRef, EgressProfile, EntryKind, Env, EnvPath, ExecError, ExecSpec, Executor, FileAccess, ForkOptions, ForkPurpose, ListOpts, ProcessRole,
};
use crate::ports::link::ServerLink;

/// Placeholder every indexer command must contain.
pub const SCIP_OUT: &str = "${scip.out}";
/// Largest index uploaded.
pub const SCIP_MAX_BYTES: u64 = 1 << 30;

/// `[runner.scip]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScipConfig {
    /// Language → command template (with `${scip.out}`), used when the recipe has no `[[scip]]`.
    pub defaults: BTreeMap<String, String>,
    /// Whole step.
    pub total_timeout_s: u64,
}

impl Default for ScipConfig {
    fn default() -> Self {
        Self {
            defaults: BTreeMap::from([("typescript".to_owned(), "scip-typescript index --output ${scip.out}".to_owned())]),
            total_timeout_s: 20 * 60,
        }
    }
}

/// Files at the workspace root that mark a language as present.
const LANGUAGE_MARKERS: &[(&str, &[&str])] = &[
    ("typescript", &["tsconfig.json", "package.json"]),
    ("javascript", &["package.json"]),
    ("python", &["pyproject.toml", "setup.py", "setup.cfg", "requirements.txt"]),
    ("go", &["go.mod"]),
    ("rust", &["Cargo.toml"]),
    ("java", &["pom.xml", "build.gradle", "build.gradle.kts"]),
    ("ruby", &["Gemfile"]),
    ("csharp", &["global.json"]),
];

/// Indexers to run: the recipe's `[[scip]]` entries, else the defaults for the detected
/// languages (`root_files`: names at the workspace root).
pub fn plan_indexers(recipe: &[ScipSpec], defaults: &BTreeMap<String, String>, root_files: &[String]) -> Vec<ScipSpec> {
    if !recipe.is_empty() {
        return recipe.to_vec();
    }
    defaults
        .iter()
        .filter(|(language, _)| {
            LANGUAGE_MARKERS
                .iter()
                .find(|(l, _)| l == language)
                .is_some_and(|(_, markers)| markers.iter().any(|m| root_files.iter().any(|f| f == m)))
        })
        .map(|(language, run)| ScipSpec { language: language.clone(), run: run.clone(), cwd: ".".to_owned(), timeout_s: 1800 })
        .collect()
}

/// Replaces `${scip.out}` (not the escaped `$${scip.out}`) with `out`.
pub fn substitute_out(template: &str, out: &str) -> String {
    let mut result = String::with_capacity(template.len() + out.len());
    let mut rest = template;
    while let Some(i) = rest.find(SCIP_OUT) {
        let escaped = i > 0 && rest.as_bytes()[i - 1] == b'$';
        result.push_str(&rest[..i]);
        if escaped {
            result.push_str(SCIP_OUT);
        } else {
            result.push_str(out);
        }
        rest = &rest[i + SCIP_OUT.len()..];
    }
    result.push_str(rest);
    result
}

/// What the step produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScipReport {
    pub uploaded: Vec<(String, ArtifactId)>,
    /// Language and why it was skipped.
    pub skipped: Vec<(String, String)>,
}

pub struct ScipStep {
    executor: Arc<dyn Executor>,
    link: Arc<dyn ServerLink>,
    config: ScipConfig,
}

impl ScipStep {
    pub fn new(executor: Arc<dyn Executor>, link: Arc<dyn ServerLink>, config: ScipConfig) -> Self {
        Self { executor, link, config }
    }

    pub async fn run(
        &self,
        run_id: RunId,
        attempt: u16,
        base: &BaseRef,
        outbox: &Path,
        events: &mpsc::UnboundedSender<OutboundEventKind>,
    ) -> ScipReport {
        let notice = |text: String| {
            let _ = events.send(OutboundEventKind::Notice(text));
        };
        let mut report = ScipReport::default();
        let env = match self.executor.fork(base, ForkOptions { run_id, purpose: ForkPurpose::Scip, egress: EgressProfile::Registries }).await {
            Ok(env) => env,
            Err(e) => {
                notice(format!("scip: fork failed, no code-intelligence index this time: {e}"));
                return report;
            }
        };
        let deadline = Instant::now() + Duration::from_secs(self.config.total_timeout_s);
        let indexers = match self.indexers(env.as_ref()).await {
            Ok(indexers) => indexers,
            Err(e) => {
                notice(format!("scip: cannot list the workspace: {e}"));
                Vec::new()
            }
        };
        if indexers.is_empty() {
            notice("scip: no indexer configured for this repository's languages".to_owned());
        }
        if let Err(e) = tokio::fs::create_dir_all(outbox).await {
            notice(format!("scip: outbox: {e}"));
        }
        let scip_dir = EnvPath::scratch("scip");
        match scip_dir {
            Ok(dir) => match env.make_dir(&dir, FileAccess::AgentWritable).await {
                Ok(()) | Err(ExecError::Conflict(_)) => {}
                Err(e) => notice(format!("scip: cannot create the output dir: {e}")),
            },
            Err(e) => notice(format!("scip: {e}")),
        }
        for spec in indexers {
            let language = artifact_name(&spec.language);
            match self.index(env.as_ref(), &spec, &language, deadline, outbox, run_id, attempt).await {
                Ok(id) => report.uploaded.push((spec.language.clone(), id)),
                Err(reason) => {
                    notice(format!("scip {}: skipped: {reason}", spec.language));
                    report.skipped.push((spec.language.clone(), reason));
                }
            }
        }
        if let Err(e) = env.destroy().await {
            tracing::warn!(env = %env.id(), error = %e, "scip fork destroy failed; the reaper retries");
        }
        report
    }

    async fn indexers(&self, env: &dyn Env) -> Result<Vec<ScipSpec>, ExecError> {
        let recipe = env.recipe().map(|r| r.scip).unwrap_or_default();
        if !recipe.is_empty() {
            return Ok(recipe);
        }
        let listing = env.list(&EnvPath::ws(".")?, ListOpts { depth: 1, hidden: true, ..ListOpts::default() }).await?;
        let root_files: Vec<String> = listing.entries.iter().filter(|e| e.kind == EntryKind::File).map(|e| e.path.as_str().to_owned()).collect();
        Ok(plan_indexers(&[], &self.config.defaults, &root_files))
    }

    #[allow(clippy::too_many_arguments)]
    async fn index(
        &self,
        env: &dyn Env,
        spec: &ScipSpec,
        language: &str,
        deadline: Instant,
        outbox: &Path,
        run_id: RunId,
        attempt: u16,
    ) -> Result<ArtifactId, String> {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err("the 20 min SCIP budget is spent".to_owned());
        }
        if !spec.run.contains(SCIP_OUT) {
            return Err(format!("the command does not contain {SCIP_OUT}"));
        }
        let rel = format!("scip/{language}.scip");
        let out_abs = format!("{}/{rel}", env.scratch_root().trim_end_matches('/'));
        let command = env.render(&substitute_out(&spec.run, &out_abs)).map_err(|e| e.to_string())?;
        let mut exec = ExecSpec::shell(command, ProcessRole::Scip);
        exec.cwd = EnvPath::ws(&spec.cwd).map_err(|e| e.to_string())?;
        exec.timeout = Duration::from_secs(u64::from(spec.timeout_s)).min(left);
        let outcome = env.exec(exec, None).await.map_err(|e| e.to_string())?;
        if !outcome.exit.success() {
            let mut stderr = outcome.stderr.head.clone();
            stderr.extend_from_slice(&outcome.stderr.tail);
            return Err(format!("{} ({}): {}", outcome.log, outcome.exit, tail_bytes(&clean_bytes(&stderr), 1024)));
        }
        let name = format!("{language}.scip");
        let dest = outbox.join(&name);
        let exported = env.export(&EnvPath::scratch(&rel).map_err(|e| e.to_string())?, &dest).await.map_err(|e| match e {
            ExecError::NotFound(_) => format!("the indexer exited 0 but wrote no {rel}"),
            e => e.to_string(),
        })?;
        if exported.bytes > SCIP_MAX_BYTES {
            let _ = tokio::fs::remove_file(&dest).await;
            return Err(format!("index is {} bytes, over the 1 GiB cap", exported.bytes));
        }
        let uploaded =
            upload_file(self.link.as_ref(), run_id, attempt, ArtifactKind::ScipIndex, &name, "application/octet-stream", &dest).await;
        let _ = tokio::fs::remove_file(&dest).await;
        uploaded.map_err(|e| format!("upload failed: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(language: &str, run: &str) -> ScipSpec {
        ScipSpec { language: language.into(), run: run.into(), cwd: ".".into(), timeout_s: 1800 }
    }

    #[test]
    fn recipe_entries_win_over_defaults() {
        let recipe = vec![spec("python", "scip-python index . --output ${scip.out}")];
        let defaults = ScipConfig::default().defaults;
        assert_eq!(plan_indexers(&recipe, &defaults, &["package.json".into()]), recipe);
    }

    #[test]
    fn defaults_only_for_detected_languages() {
        let defaults = BTreeMap::from([
            ("typescript".to_owned(), "scip-typescript index --output ${scip.out}".to_owned()),
            ("python".to_owned(), "scip-python index . --output ${scip.out}".to_owned()),
            ("cobol".to_owned(), "scip-cobol ${scip.out}".to_owned()),
        ]);
        let plan = plan_indexers(&[], &defaults, &["tsconfig.json".into(), "README.md".into()]);
        let languages: Vec<&str> = plan.iter().map(|s| s.language.as_str()).collect();
        assert_eq!(languages, ["typescript"]);
        assert!(plan_indexers(&[], &defaults, &["README.md".into()]).is_empty());
        let both = plan_indexers(&[], &defaults, &["package.json".into(), "pyproject.toml".into()]);
        assert_eq!(both.len(), 2);
    }

    #[test]
    fn substitute_out_respects_the_escape() {
        assert_eq!(substitute_out("scip index --output ${scip.out}", "/s/scip/ts.scip"), "scip index --output /s/scip/ts.scip");
        assert_eq!(substitute_out("a ${scip.out} b ${scip.out}", "X"), "a X b X");
        assert_eq!(substitute_out("echo $${scip.out}", "X"), "echo $${scip.out}");
        assert_eq!(substitute_out("none", "X"), "none");
    }
}
