//! `AcceptFixResult` (fix.md F2 "Server: AcceptFixResult"): the authoritative server-side check of
//! a submitted patch, and the Fix gate judge. Implements `crate::app::runs::FixGate`; the run
//! manager owns the commit (gate row, Verify run, status, feedback).
//!
//! Steps: patch bytes vs the artifact row's sha256 → `Patch::parse` → `PatchPolicy::check` (deny
//! paths, limits, incl. the regression file) → `SecretScanner` over added lines and the regression
//! file → `ChangeSet` + Verify spec. Any violation is a failed gate attempt with feedback; secret
//! values are never echoed (findings name rule and location only).

use std::sync::Arc;

use async_trait::async_trait;
use mage_core::domain::fix::{ApplyMode, ChangeSet, Glob, Patch, PatchPolicy, RegressionContent, RegressionFile};
use mage_core::domain::recipe::{LintMode, RecipeSpec};
use mage_core::domain::{ArtifactKind, FixResult, GateFailure, OrgId, RunKind, VerifySpec};
use mage_core::ports::blob::{BlobKey, PutOptions};
use sha2::{Digest, Sha256};

use crate::app::fix::proof::{FixGateDetails, FixProof, cause_of};
use crate::app::fix::text::neutralize_mentions;
use crate::app::fix::{FixDeps, ScriptInfo, blob_err, recipe_fake_values};
use crate::app::runs::{FixAcceptance, FixGate};
use crate::error::AppError;
use crate::ports::runs::{GateRecord, NewArtifact, RunRecord, RunWrite};
use crate::ports::secretscan::{ScanInput, SecretFinding};

/// Largest repro script read back (the Script artifact cap is far below storage's).
pub(crate) const SCRIPT_MAX_BYTES: u64 = 4 << 20;
/// Feedback for the agent lists at most this many findings / violations.
const FEEDBACK_ITEMS: usize = 20;

#[derive(Clone)]
pub struct AcceptFixResult {
    d: Arc<FixDeps>,
}

/// A rejected submission: gate reason (short) and agent feedback (rendered).
struct Violation {
    reason: String,
    lines: Vec<String>,
}

impl Violation {
    fn one(reason: impl Into<String>, line: impl Into<String>) -> Self {
        Self { reason: reason.into(), lines: vec![line.into()] }
    }

    fn into_acceptance(self) -> FixAcceptance {
        let mut details = format!("Fix gate: your patch was rejected before verification ({}).\n", self.reason);
        for line in self.lines.iter().take(FEEDBACK_ITEMS) {
            details.push_str("- ");
            details.push_str(line);
            details.push('\n');
        }
        if self.lines.len() > FEEDBACK_ITEMS {
            details.push_str(&format!("- … and {} more\n", self.lines.len() - FEEDBACK_ITEMS));
        }
        details.push_str(
            "Undo these changes in your working tree (Mage diffs the fork against the base), then call submit_fix again. \
             This attempt counts toward the fix gate limit.",
        );
        FixAcceptance::Rejected { reason: self.reason, details }
    }
}

impl AcceptFixResult {
    pub fn new(d: Arc<FixDeps>) -> Self {
        Self { d }
    }

    /// fix.md AcceptFixResult steps 1–7 for gate attempt `attempt`.
    pub async fn evaluate(&self, org: OrgId, run: &RunRecord, attempt: u8, result: &FixResult) -> Result<FixAcceptance, AppError> {
        if run.run.kind != RunKind::Fix {
            return Err(AppError::Invalid(format!("run {} is not a fix run", run.id())));
        }
        let fix_spec = run.spec.fix.as_ref().ok_or_else(|| AppError::Internal(format!("fix run {} has no FixSpec", run.id())))?;
        let (repo_id, base_sha, recipe_hash) = match (run.spec.repo_id, run.spec.sha, run.spec.recipe_hash) {
            (Some(r), Some(s), Some(h)) => (r, s, h),
            _ => return Err(AppError::Internal(format!("fix run {} has no repo/sha/recipe", run.id()))),
        };
        let settings = self.d.orgs.fix_settings(org).await?;
        let recipe = self.d.recipe_spec(org, repo_id, recipe_hash).await?;

        // 1. Patch bytes match the artifact row.
        let rec = self.d.runs.artifact(org, result.patch).await?;
        if rec.artifact.kind != ArtifactKind::Patch || rec.artifact.run_id != run.id() {
            return Ok(Violation::one("patch artifact invalid", "the submitted patch is not a Patch artifact of this run").into_acceptance());
        }
        let limit = settings.limits.max_patch_bytes;
        if rec.artifact.bytes > limit {
            return Ok(Violation::one(
                "patch too large",
                format!("patch is {} bytes (limit {limit}); keep the change minimal", rec.artifact.bytes),
            )
            .into_acceptance());
        }
        let key = BlobKey::parse(&rec.artifact.key).map_err(blob_err)?;
        let raw = self.d.artifacts.get(&key, limit).await.map_err(blob_err)?;
        let sha: [u8; 32] = Sha256::digest(&raw).into();
        if sha != rec.artifact.sha256 {
            return Ok(Violation::one("patch digest mismatch", "the uploaded patch bytes do not match the recorded digest").into_acceptance());
        }

        // 2. Strict parse.
        let patch = match Patch::parse(&raw) {
            Ok(p) => p,
            Err(e) => return Ok(Violation::one("patch does not parse", format!("{e}")).into_acceptance()),
        };
        if patch.files().is_empty() {
            return Ok(Violation::one("empty patch", "the patch changes nothing").into_acceptance());
        }

        // 5 (needed by 3–4). Regression file placement and content.
        let cause_id = cause_of(run)?;
        let cause = self.d.tickets.get_cause(org, cause_id).await?;
        let ticket = self.d.tickets.get_ticket(org, cause.ticket_id).await?.value;
        let identifier = match &ticket.source {
            mage_core::domain::TicketSource::Linear { identifier, .. } => identifier.clone(),
        };
        // Repro scripts are small; the policy below bounds the regression file anyway.
        let (script_rec, script_bytes) = self.d.artifact_bytes(org, fix_spec.script, SCRIPT_MAX_BYTES).await?;
        let script = ScriptInfo::from_record(&script_rec)?;
        let regression = match fix_spec.regression.path(&identifier, &cause.title, &script.ext)? {
            None => None,
            Some(path) => {
                let bytes = regression_bytes(&script, &script_bytes, &identifier, &cause.title, &self.d.config.public_url, run.id());
                Some(RegressionContent { path, bytes })
            }
        };

        // 3. Patch policy.
        let generated: Vec<Glob> =
            recipe.fix.as_ref().map(|f| f.generated.iter().filter_map(|g| Glob::new(g.clone()).ok()).collect()).unwrap_or_default();
        let policy = PatchPolicy::from_settings(&settings, generated);
        if let Err(violations) = policy.check(&patch, regression.as_ref()) {
            let lines = violations.0.iter().map(ToString::to_string).collect();
            return Ok(Violation { reason: "patch policy violated".into(), lines }.into_acceptance());
        }

        // 4. Secret scan of added lines and the regression file.
        let findings = self.scan(&patch, regression.as_ref(), &recipe);
        if !findings.is_empty() {
            let lines = findings.iter().map(|f| format!("possible secret ({f}); remove it")).collect();
            return Ok(Violation { reason: "secret detected in the patch".into(), lines }.into_acceptance());
        }

        // 5. Change set (+ the regression artifact the gate writes into the fork).
        let regression_file = match &regression {
            None => None,
            Some(content) => Some(self.store_regression(org, run, attempt, &script, content).await?),
        };
        let change_set = ChangeSet { base_sha, patch: result.patch, patch_sha256: patch.sha256(), regression: regression_file.clone() };
        let lint_required = recipe.fix.as_ref().is_some_and(|f| f.lint.is_some() && f.lint_mode == LintMode::Required);
        let details = FixGateDetails { change_set, lint_required }.to_json()?;

        // 7. The Verify spec of the gate.
        let verify = VerifySpec::Fix {
            patch: result.patch,
            script: fix_spec.script,
            signature: fix_spec.signature.clone(),
            repetitions: settings.repetitions.max(1),
            regression: regression_file,
            mode: ApplyMode::Exact,
            upstream_check: false,
        };
        Ok(FixAcceptance::Accepted { verify, details })
    }

    fn scan(&self, patch: &Patch, regression: Option<&RegressionContent>, recipe: &RecipeSpec) -> Vec<SecretFinding> {
        let allowed = recipe_fake_values(recipe);
        let mut findings = Vec::new();
        for file in patch.files() {
            // Added lines joined; finding lines map back to post-image line numbers.
            let mut text = String::new();
            let mut numbers = Vec::new();
            for (line_no, bytes) in file.added_lines() {
                text.push_str(&String::from_utf8_lossy(bytes).replace('\n', ""));
                text.push('\n');
                numbers.push(line_no);
            }
            let input = ScanInput { path: Some(file.path()), text: &text, known: &[], allowed: &allowed };
            for mut f in self.d.scanner.scan(&input) {
                if let Some(n) = usize::try_from(f.line).ok().and_then(|l| numbers.get(l.wrapping_sub(1))) {
                    f.line = *n;
                }
                findings.push(f);
            }
        }
        if let Some(reg) = regression {
            let text = String::from_utf8_lossy(&reg.bytes);
            findings.extend(self.d.scanner.scan(&ScanInput { path: Some(&reg.path), text: &text, known: &[], allowed: &allowed }));
        }
        findings
    }

    /// The regression file as an artifact of the Fix run (the gate downloads and places it).
    async fn store_regression(
        &self,
        org: OrgId,
        run: &RunRecord,
        attempt: u8,
        script: &ScriptInfo,
        content: &RegressionContent,
    ) -> Result<RegressionFile, AppError> {
        let sha256 = content.sha256();
        let reserved = self
            .d
            .runs
            .reserve_artifact(
                org,
                NewArtifact {
                    run_id: run.id(),
                    attempt: run.run.attempt,
                    kind: ArtifactKind::Script,
                    name: format!("regression-{attempt}.{}", script.ext),
                    bytes: content.bytes.len() as u64,
                    sha256,
                    content_type: "text/plain; charset=utf-8".into(),
                },
            )
            .await?;
        let key = BlobKey::parse(&reserved.artifact.key).map_err(blob_err)?;
        self.d
            .artifacts
            .put(&key, content.bytes.clone(), &PutOptions { content_type: "text/plain; charset=utf-8".into(), if_absent: false })
            .await
            .map_err(blob_err)?;
        self.d
            .runs
            .commit(org, RunWrite { complete_artifacts: vec![(reserved.artifact.id, content.bytes.len() as u64)], ..RunWrite::default() })
            .await?;
        Ok(RegressionFile { path: content.path.clone(), script: reserved.artifact.id, script_sha256: sha256 })
    }
}

/// The repro script with a header naming the ticket and the Mage run (fix.md "Regression test
/// placement"); a shebang stays the first line.
pub fn regression_bytes(script: &ScriptInfo, body: &[u8], identifier: &str, title: &str, public_url: &url::Url, fix_run: mage_core::domain::RunId) -> Vec<u8> {
    let title = neutralize_mentions(&title.replace(['\r', '\n'], " "));
    let run_url = crate::app::fix::text::ui_link(public_url, &format!("runs/{fix_run}"));
    let comment = match script.kind {
        mage_core::domain::ScriptKind::Playwright => "//",
        mage_core::domain::ScriptKind::Shell => "#",
    };
    let header = format!(
        "{comment} Mage regression test for {identifier}: {title}\n{comment} It reproduced the bug before the fix and passes after it. Mage run: {run_url}\n"
    );
    let mut out = Vec::with_capacity(header.len() + body.len());
    let (shebang, rest) = if body.starts_with(b"#!") {
        let end = body.iter().position(|b| *b == b'\n').map_or(body.len(), |i| i + 1);
        body.split_at(end)
    } else {
        (&body[..0], body)
    };
    out.extend_from_slice(shebang);
    if !shebang.is_empty() && !shebang.ends_with(b"\n") {
        out.push(b'\n');
    }
    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(rest);
    out
}

#[async_trait]
impl FixGate for AcceptFixResult {
    async fn accept(&self, org: OrgId, run: &RunRecord, attempt: u8, result: &FixResult) -> Result<FixAcceptance, AppError> {
        self.evaluate(org, run, attempt, result).await
    }

    async fn judge(&self, _org: OrgId, fix_run: &RunRecord, gate: &GateRecord, verify_run: &RunRecord) -> Result<Result<(), GateFailure>, AppError> {
        Ok(FixProof::evaluate_gate(fix_run, gate, verify_run)?.map(|_| ()))
    }
}
