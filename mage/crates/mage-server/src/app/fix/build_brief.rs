//! `BuildFixBrief` (fix.md F1 "Use case BuildFixBrief"): job `fix.brief {org_id, cause_id,
//! diagnose_run_id}`.
//!
//! 1. The cause must be `Reproduced` by `diagnose_run` (else the job completes without a brief).
//! 2. Re-derive `ReproGatePassed` from the stored `DiagnoseResult` + repro gate `VerifyResult`.
//! 3. Re-check every citation (`cause.evidence` ∪ `fix_sites`) with `evidence::check` at the
//!    ticket sha: a citation passes when its path and range still hold and its snippet hash is
//!    unchanged; failing ones are dropped and listed in a `Notice`. All evidence failing →
//!    `Unavailable` (retried after the index catches up).
//! 4. Inline the repro script (≤ 64 KiB) or link it.
//! 5. Render, 6. secret-scan and redact, 7. store the `brief-v{n}.md` artifact, 8. store the
//!    versioned `FixBrief` with `notify.ticket{FixBriefReady}` (outbox).

use std::collections::BTreeSet;
use std::sync::Arc;

use mage_core::domain::evidence::{self, MIN_QUOTE_CHARS};
use mage_core::domain::gates::{self, ReproGate};
use mage_core::domain::{
    ArtifactKind, CauseId, CauseStatus, Citation, CitationDraft, FixBrief, FixBriefInit, Notice, OrgId, RepoPath, RepoVersionKey, RunEventKind,
    RunId, RunResult, TicketSource,
};
use mage_core::ports::blob::{BlobKey, PutOptions};
use sha2::{Digest, Sha256};

use crate::app::fix::accept::SCRIPT_MAX_BYTES;
use crate::app::fix::brief::{self, BriefInput, BriefScript};
use crate::app::fix::proof::verify_result;
use crate::app::fix::{FixDeps, ScriptInfo, blob_err, notify, recipe_fake_values, repo_err};
use crate::app::triage::view::load_manifest_view;
use crate::error::AppError;
use crate::ports::runs::{NewArtifact, RunEventDraft, RunWrite};
use crate::ports::secretscan::{ScanInput, redact};

const BRIEF_CONTENT_TYPE: &str = "text/markdown; charset=utf-8";

/// What a `fix.brief` job did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BriefOutcome {
    Written { version: u32, dropped: usize, redacted: usize },
    /// `fix.brief` is off for the org.
    Disabled,
    /// The cause is not (or no longer) reproduced by this diagnose run.
    NotReproduced,
}

#[derive(Clone)]
pub struct BuildFixBrief {
    d: Arc<FixDeps>,
}

/// Outcome of the citation re-check.
struct Checked {
    evidence: Vec<Citation>,
    fix_sites: Vec<Citation>,
    proof: evidence::EvidenceChecked,
    dropped: Vec<String>,
}

impl BuildFixBrief {
    pub fn new(d: Arc<FixDeps>) -> Self {
        Self { d }
    }

    pub async fn run(&self, org: OrgId, cause_id: CauseId, diagnose_run: RunId) -> Result<BriefOutcome, AppError> {
        let d = &self.d;
        let settings = d.orgs.fix_settings(org).await?;
        if !settings.brief {
            return Ok(BriefOutcome::Disabled);
        }
        let cause = d.tickets.get_cause(org, cause_id).await?;
        let CauseStatus::Reproduced { run, gate } = cause.status else {
            return Ok(BriefOutcome::NotReproduced);
        };
        if run != diagnose_run {
            return Ok(BriefOutcome::NotReproduced);
        }

        // 2. Re-derive the repro proof.
        let diag_run = d.runs.get(org, run).await?;
        let Some(RunResult::Diagnose(diagnosis)) = diag_run.run.result.clone() else {
            return Err(AppError::Internal(format!("diagnose run {run} has no result")));
        };
        let gate_rec = d.runs.gates(org, run).await?.into_iter().find(|g| g.gate.id == gate).ok_or_else(|| AppError::Internal(format!("repro gate {gate} missing")))?;
        let verify_id = gate_rec.gate.verify_run.ok_or_else(|| AppError::Internal(format!("repro gate {gate} has no verify run")))?;
        let verify_run = d.runs.get(org, verify_id).await?;
        let verify = verify_result(&verify_run)?;
        let spec = verify_run.spec.verify.as_ref().ok_or_else(|| AppError::Internal("repro verify run has no spec".into()))?;
        let repro = gates::repro(&ReproGate { gate, diagnose_run: run, cause: cause.id, spec }, &diagnosis, &verify)
            .map_err(|f| AppError::Internal(format!("stored repro gate no longer re-derives: {f}")))?;

        let ticket = d.tickets.get_ticket(org, cause.ticket_id).await?.value;
        let target = ticket.target.clone().ok_or_else(|| AppError::Invalid("the ticket has no target".into()))?;
        let repo = d.repos.repo(org, target.repo_id).await.map_err(repo_err)?;
        let recipe_hash = diag_run.spec.recipe_hash.ok_or_else(|| AppError::Internal("diagnose run has no recipe".into()))?;
        let recipe = d.recipe_spec(org, target.repo_id, recipe_hash).await?;

        // 3. Citations.
        let key = RepoVersionKey::new(target.repo_id, target.sha);
        let checked = self.check_citations(org, key, &cause.evidence, &diagnosis.fix_sites).await?;

        // 4. Script.
        let script_id = diagnosis.script.ok_or_else(|| AppError::Internal("reproduced diagnosis without a script".into()))?;
        let script_rec = d.runs.artifact(org, script_id).await?;
        let script = ScriptInfo::from_record(&script_rec)?;
        let inline = if script.bytes <= d.config.script_inline_max_bytes {
            let (_, bytes) = d.artifact_bytes(org, script_id, SCRIPT_MAX_BYTES).await?;
            String::from_utf8(bytes).ok()
        } else {
            None
        };
        let path = script.repro_path(cause.id);

        // Repro evidence recordings of the repro gate.
        let mut video = None;
        let mut trace = None;
        for id in &verify.artifacts {
            if let Ok(rec) = d.runs.artifact(org, *id).await {
                match rec.artifact.kind {
                    ArtifactKind::Video if video.is_none() => video = Some(*id),
                    ArtifactKind::Trace if trace.is_none() => trace = Some(*id),
                    _ => {}
                }
            }
        }

        let TicketSource::Linear { identifier, url, .. } = &ticket.source;
        let input = BriefInput {
            ui_url: &d.config.public_url,
            github_web: &d.config.github_web,
            ticket_identifier: identifier,
            ticket_url: url,
            ticket_title: &ticket.title,
            ticket_body: &ticket.body,
            cause_title: &cause.title,
            cause_explanation: &cause.explanation,
            method: cause.method,
            owner: &repo.owner,
            name: &repo.name,
            sha: target.sha,
            assumed: target.assumed(),
            diagnose_run: run,
            diagnose_summary: &diagnosis.summary,
            suggested_fix: diagnosis.suggested_fix.as_deref(),
            evidence: &checked.evidence,
            fix_sites: &checked.fix_sites,
            video,
            trace,
            signature: repro.signature(),
            script: BriefScript { artifact: script_id, name: &script.name, kind: script.kind, path: &path, body: inline.as_deref(), bytes: script.bytes },
            recipe: &recipe,
            allow_workflow_edits: settings.allow_workflow_edits,
            lockfiles: settings.lockfiles,
        };
        let rendered = brief::render(&input);

        // 6. Secret scan: redact, never block.
        let allowed = recipe_fake_values(&recipe);
        let findings = d.scanner.scan(&ScanInput { path: None, text: &rendered, known: &[], allowed: &allowed });
        let markdown = if findings.is_empty() { rendered } else { redact(&rendered, &findings) };

        // 7. Artifact `brief-v{n}.md` on the diagnose run.
        let version = d.tickets.fix_briefs(org, cause.id).await?.iter().map(|b| b.version).max().unwrap_or(0) + 1;
        let bytes = markdown.as_bytes().to_vec();
        let sha256: [u8; 32] = Sha256::digest(&bytes).into();
        let reserved = d
            .runs
            .reserve_artifact(
                org,
                NewArtifact {
                    run_id: run,
                    attempt: diag_run.run.attempt,
                    kind: ArtifactKind::FixBrief,
                    name: format!("brief-v{version}.md"),
                    bytes: bytes.len() as u64,
                    sha256,
                    content_type: BRIEF_CONTENT_TYPE.into(),
                },
            )
            .await?;
        let blob = BlobKey::parse(&reserved.artifact.key).map_err(blob_err)?;
        d.artifacts.put(&blob, bytes.clone(), &PutOptions { content_type: BRIEF_CONTENT_TYPE.into(), if_absent: false }).await.map_err(blob_err)?;

        let mut notices: Vec<String> = Vec::new();
        if !checked.dropped.is_empty() {
            notices.push(format!("Fix brief v{version}: dropped citations that no longer hold at {}: {}", target.sha, checked.dropped.join(", ")));
        }
        if !findings.is_empty() {
            let rules: BTreeSet<&str> = findings.iter().map(|f| f.rule).collect();
            notices.push(format!("Fix brief v{version}: redacted {} possible secret(s) ({})", findings.len(), rules.into_iter().collect::<Vec<_>>().join(", ")));
        }
        let now = d.clock.now();
        d.runs
            .commit(
                org,
                RunWrite {
                    complete_artifacts: vec![(reserved.artifact.id, bytes.len() as u64)],
                    events: notices
                        .into_iter()
                        .map(|text| RunEventDraft { run_id: run, attempt: diag_run.run.attempt, at: now, kind: RunEventKind::Notice(text) })
                        .collect(),
                    ..RunWrite::default()
                },
            )
            .await?;

        // 8. Versioned brief + notice, one transaction.
        let brief = FixBrief::new(&cause, checked.proof, FixBriefInit { id: d.ids.new_id(), version, artifact: reserved.artifact.id, markdown, created_at: now })?;
        let stored = d.tickets.put_fix_brief(org, brief, &[notify(org, ticket.id, Notice::FixBriefReady { cause_id: cause.id, version })]).await?;
        if stored.version != version {
            tracing::warn!(cause = %cause.id, expected = version, stored = stored.version, "fix brief version raced; artifact name and notice use the expected one");
        }
        Ok(BriefOutcome::Written { version: stored.version, dropped: checked.dropped.len(), redacted: findings.len() })
    }

    /// Re-check citations at `key`. Pass 1 (no quotes) finds the citations whose path and range
    /// still hold and whose snippet hash is unchanged; pass 2 checks the survivors with their
    /// current cited text as the quote, which yields the `EvidenceChecked` proof for exactly the
    /// citations printed in the brief.
    async fn check_citations(&self, org: OrgId, key: RepoVersionKey, evidence_in: &[Citation], sites_in: &[Citation]) -> Result<Checked, AppError> {
        let d = &self.d;
        let mut all: Vec<(bool, Citation)> = evidence_in.iter().map(|c| (true, c.clone())).collect();
        for site in sites_in {
            if !all.iter().any(|(_, c)| c.path == site.path && c.lines == site.lines) {
                all.push((false, site.clone()));
            }
        }
        let paths: Vec<RepoPath> = all.iter().map(|(_, c)| c.path.clone()).collect();
        let view = load_manifest_view(d.knowledge.as_ref(), &d.reader, org, key, &paths).await?;

        let draft = |c: &Citation, quote: Option<String>| CitationDraft { path: c.path.clone(), lines: c.lines, quote, note: c.note.clone() };
        let drafts: Vec<CitationDraft> = all.iter().map(|(_, c)| draft(c, None)).collect();
        let first = evidence::check(&drafts, &view);
        let mut survivors: Vec<(bool, Citation)> = Vec::new();
        let mut dropped = Vec::new();
        for (i, (is_evidence, c)) in all.iter().enumerate() {
            let label = format!("{}:{}", c.path, c.lines);
            let passed = first.sources().iter().position(|s| *s == i).map(|at| &first.citations()[at]);
            match passed {
                Some(now) if c.snippet_hash.is_none() || now.snippet_hash == c.snippet_hash => survivors.push((*is_evidence, now.clone())),
                _ => dropped.push(label),
            }
        }
        if !survivors.iter().any(|(e, _)| *e) {
            return Err(AppError::Unavailable(format!("every evidence citation of the cause fails at {key}; waiting for the index")));
        }

        let mut quoted = Vec::with_capacity(survivors.len());
        for (_, c) in &survivors {
            let lines = d.reader.read_lines(org, &key, &c.path, c.lines).await?;
            let text = lines.lines.join("\n");
            let quote = (text.chars().filter(|ch| !ch.is_whitespace()).count() >= MIN_QUOTE_CHARS).then_some(text);
            quoted.push(draft(c, quote));
        }
        let report = evidence::check(&quoted, &view);
        for (i, item) in report.items().iter().enumerate() {
            if matches!(item, evidence::CitationCheck::Rejected(_)) {
                dropped.push(format!("{}:{}", quoted[i].path, quoted[i].lines));
            }
        }
        let citations = report.citations().to_vec();
        let sources = report.sources().to_vec();
        let proof = report.into_checked().map_err(|_| AppError::Unavailable(format!("no verifiable evidence citation at {key}")))?;
        let (mut evidence, mut fix_sites) = (Vec::new(), Vec::new());
        for (c, source) in citations.into_iter().zip(sources) {
            if survivors.get(source).is_some_and(|(e, _)| *e) {
                evidence.push(c);
            } else {
                fix_sites.push(c);
            }
        }
        Ok(Checked { evidence, fix_sites, proof, dropped })
    }
}
