//! UC3 Evidence gate (triage.md "Server-side validation", "Evidence gate algorithm", "Decision
//! table", "Salvage"). Called synchronously by `runs::SubmitResult` for every `submit_triage`.
//!
//! The use case loads the manifest view and "did you mean" hints; validation, the check, the
//! decision and the feedback text are pure functions so the decision table is testable without
//! I/O.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use mage_core::domain::evidence::{self, CauseEvidence, CitationCheck, CitationDefect, ManifestView, QuoteCheck};
use mage_core::domain::triage::{AcceptedCause, merge_and_rank};
use mage_core::domain::{
    CauseDraft, CitationDraft, Confidence, GateKind, OrgId, RepoPath, RepoVersionKey, Run, RunKind, RunStatus, TriageResult,
};
use mage_core::ports::KnowledgeStore;
use mage_core::wire::runner::{CauseDraftDto, CitationDraftDto, GateTicket, Rejection, RejectionItem, TriageSubmission};

use crate::app::knowledge_query::SourceReader;
use crate::app::runs::{EvidenceDecision, TriageGate};
use crate::app::triage::prompt::single_line;
use crate::app::triage::view::load_manifest_view;
use crate::domain::triage::TriageSpec;
use crate::error::AppError;
use crate::ports::runs::{GateRecord, RunRecord};

// Schema bounds (`schemas/submit_triage.json`), in Unicode scalar values.
const SUMMARY_MAX: usize = 2000;
const CAUSES_MAX: usize = 5;
const MISSING_INFO_MAX: usize = 5;
const MISSING_INFO_ITEM_MAX: usize = 300;
const TITLE_MAX: usize = 120;
const EXPLANATION_MAX: usize = 4000;
const EVIDENCE_MAX: usize = 8;
const REPRO_STEPS_MAX: usize = 12;
const REPRO_STEP_MAX: usize = 500;
const CONDITION_MAX: usize = 1000;
const PATH_MAX: usize = 1024;
const QUOTE_MAX: usize = 2000;
const NOTE_MAX: usize = 200;
/// Similar-path hints per missing path.
const SIMILAR_PATHS: u16 = 3;

/// Gate verdict for one submitted cause, in submission order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CauseCheck {
    /// 0-based submission index.
    pub index: u8,
    pub title: String,
    pub verdict: CauseEvidence,
    /// One per submitted citation, in submission order.
    pub citations: Vec<CitationCheck>,
}

/// Stored as `GateRecord.details` of every Evidence gate row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvidenceGateDetails {
    pub causes: Vec<CauseCheck>,
    /// `Reject` only: the valid causes, merged and ranked, for `salvage`.
    pub salvage: Option<TriageResult>,
    /// Run notices of `merge_and_rank` (committed with `Accept`).
    #[serde(default)]
    pub notices: Vec<String>,
}

impl EvidenceGateDetails {
    /// Problems located by JSON path into the submission: each invalid cause, and each failing or
    /// unverified citation of an invalid cause, plus dropped citations of valid causes.
    pub fn rejection_items(&self) -> Vec<RejectionItem> {
        let mut items = Vec::new();
        for cause in &self.causes {
            let invalid = cause.verdict != CauseEvidence::Valid;
            if invalid {
                items.push(RejectionItem { path: format!("causes[{}]", cause.index), reason: verdict_text(cause.verdict).to_owned() });
            }
            for (j, check) in cause.citations.iter().enumerate() {
                let reason = match check {
                    CitationCheck::Rejected(defect) => defect.to_string(),
                    CitationCheck::Ok { quote: QuoteCheck::Absent, .. } if invalid => "no quote".to_owned(),
                    CitationCheck::Ok { .. } => continue,
                };
                items.push(RejectionItem { path: format!("causes[{}].evidence[{j}]", cause.index), reason });
            }
        }
        items
    }

    /// Citations whose quote was found elsewhere and whose range was moved.
    pub fn adjusted_citations(&self) -> usize {
        self.causes
            .iter()
            .flat_map(|c| &c.citations)
            .filter(|c| matches!(c, CitationCheck::Ok { quote: QuoteCheck::Adjusted { .. }, .. }))
            .count()
    }

    /// Submitted causes that failed the gate.
    pub fn invalid_causes(&self) -> usize {
        self.causes.iter().filter(|c| c.verdict != CauseEvidence::Valid).count()
    }

    pub fn from_value(value: &serde_json::Value) -> Result<Self, AppError> {
        serde_json::from_value(value.clone()).map_err(|e| AppError::Internal(format!("evidence gate details: {e}")))
    }

    pub fn to_value(&self) -> Result<serde_json::Value, AppError> {
        serde_json::to_value(self).map_err(|e| AppError::Internal(format!("evidence gate details: {e}")))
    }
}

/// Outcome of one `submit_triage` (triage.md "Decision table").
#[derive(Debug, Clone, PartialEq)]
pub enum TriageGateDecision {
    /// Fails server-side validation: no gate row, no attempt consumed.
    Invalid { problems: Vec<RejectionItem> },
    Accept { result: TriageResult, details: EvidenceGateDetails, message: String },
    Reject { details: EvidenceGateDetails, feedback: String, attempts_left: u8 },
    /// Final attempt and no valid cause.
    Fail { details: EvidenceGateDetails, message: String },
}

impl TriageGateDecision {
    /// The 422 `rejected` body for `Invalid`, `Reject` and `Fail`; `None` for `Accept`.
    pub fn rejection(&self, gate: Option<GateTicket>) -> Option<Rejection> {
        match self {
            Self::Accept { .. } => None,
            Self::Invalid { problems } => Some(Rejection { gate: None, message: invalid_message(problems), items: problems.clone() }),
            Self::Reject { details, feedback, .. } => Some(Rejection { gate, message: feedback.clone(), items: details.rejection_items() }),
            Self::Fail { details, message } => Some(Rejection { gate, message: message.clone(), items: details.rejection_items() }),
        }
    }

    /// `GateStatus::Failed { reason }` for `Reject` and `Fail`.
    pub fn gate_failure_reason(&self) -> Option<String> {
        match self {
            Self::Reject { details, .. } => {
                Some(format!("{} of {} causes without verified evidence", details.invalid_causes(), details.causes.len()))
            }
            Self::Fail { .. } => Some("no cause with verified evidence".to_owned()),
            Self::Invalid { .. } | Self::Accept { .. } => None,
        }
    }

    /// The `runs::TriageGate` shape (details as opaque JSON).
    pub fn into_runs_decision(self) -> Result<EvidenceDecision, AppError> {
        let reason = self.gate_failure_reason();
        let rejection = self.rejection(None);
        Ok(match self {
            Self::Invalid { .. } => EvidenceDecision::Invalid { rejection: rejection.unwrap_or_else(empty_rejection) },
            Self::Accept { result, details, message } => {
                let notices = details.notices.clone();
                EvidenceDecision::Accept { result, details: details.to_value()?, message, notices }
            }
            Self::Reject { details, attempts_left, .. } => EvidenceDecision::Reject {
                details: details.to_value()?,
                rejection: rejection.unwrap_or_else(empty_rejection),
                reason: reason.unwrap_or_default(),
                attempts_left,
            },
            Self::Fail { details, .. } => EvidenceDecision::Fail {
                details: details.to_value()?,
                rejection: rejection.unwrap_or_else(empty_rejection),
                reason: reason.unwrap_or_default(),
            },
        })
    }
}

fn empty_rejection() -> Rejection {
    Rejection { gate: None, message: String::new(), items: Vec::new() }
}

fn invalid_message(problems: &[RejectionItem]) -> String {
    let mut message =
        String::from("INVALID submission (no gate attempt used). Fix every problem and call submit_triage again with the complete list:");
    for p in problems {
        message.push_str(&format!("\n- {}: {}", p.path, p.reason));
    }
    message
}

/// The evidence gate use case.
pub struct EvidenceGate {
    knowledge: Arc<dyn KnowledgeStore>,
    reader: Arc<SourceReader>,
}

impl EvidenceGate {
    pub fn new(knowledge: Arc<dyn KnowledgeStore>, reader: Arc<SourceReader>) -> Self {
        Self { knowledge, reader }
    }

    /// Validates, checks every citation against the manifest at the ticket's target, and decides.
    /// `attempt` is the number of Evidence gate rows of the run plus one. Infrastructure errors are
    /// returned as errors (5xx: no gate attempt consumed).
    pub async fn evaluate_submission(
        &self,
        org: OrgId,
        run: &Run,
        spec: &TriageSpec,
        attempt: u8,
        sub: TriageSubmission,
    ) -> Result<TriageGateDecision, AppError> {
        if run.kind != RunKind::Triage {
            return Err(AppError::Invalid(format!("run {} is not a triage run", run.id)));
        }
        let problems = validate_submission(&sub, spec.max_causes);
        if !problems.is_empty() {
            return Ok(TriageGateDecision::Invalid { problems });
        }
        let parsed = ParsedSubmission::from_valid(sub);
        let key = RepoVersionKey::new(spec.input.target.repo_id, spec.input.target.sha);
        let view = load_manifest_view(self.knowledge.as_ref(), &self.reader, org, key, &parsed.cited_paths()).await?;
        let checked = check_parsed(&parsed, &view);
        let mut hints = FeedbackHints { short_sha: short_sha(&key), similar: BTreeMap::new() };
        for path in missing_paths(&parsed, &checked) {
            match self.knowledge.paths_by_basename(org, &key, path.file_name(), SIMILAR_PATHS + 1).await {
                Ok(found) => {
                    let similar: Vec<RepoPath> = found.into_iter().filter(|p| *p != path).take(usize::from(SIMILAR_PATHS)).collect();
                    hints.similar.insert(path, similar);
                }
                Err(e) => tracing::warn!(error = %e, "evidence gate: basename hints unavailable"),
            }
        }
        let decision = decide(attempt, run.budget.max_gate_attempts, parsed, checked, &hints);
        let outcome = match &decision {
            TriageGateDecision::Invalid { .. } => "invalid",
            TriageGateDecision::Accept { .. } => "accept",
            TriageGateDecision::Reject { .. } => "reject",
            TriageGateDecision::Fail { .. } => "fail",
        };
        tracing::info!(target: "mage.triage", metric = "mage.triage.gate_rejections", run = %run.id, attempt, outcome, "evidence gate decided");
        Ok(decision)
    }
}

#[async_trait]
impl TriageGate for EvidenceGate {
    async fn evaluate(&self, org: OrgId, run: &RunRecord, attempt: u8, submission: TriageSubmission) -> Result<EvidenceDecision, AppError> {
        let spec = run.spec.triage.as_ref().ok_or_else(|| AppError::Internal(format!("triage run {} has no triage spec", run.run.id)))?;
        self.evaluate_submission(org, &run.run, spec, attempt, submission).await?.into_runs_decision()
    }

    fn salvage(&self, run: &RunRecord, gates: &[GateRecord]) -> Option<TriageResult> {
        match newest_evidence_details(gates) {
            Ok(details) => salvage(run, details.as_ref()),
            Err(e) => {
                tracing::error!(run = %run.run.id, error = %e, "evidence gate details unreadable; no salvage");
                None
            }
        }
    }
}

/// Details of the newest Evidence gate row, if any carries details.
pub fn newest_evidence_details(gates: &[GateRecord]) -> Result<Option<EvidenceGateDetails>, AppError> {
    gates
        .iter()
        .filter(|g| g.gate.kind == GateKind::Evidence)
        .max_by_key(|g| g.gate.attempt)
        .and_then(|g| g.details.as_ref())
        .map(EvidenceGateDetails::from_value)
        .transpose()
}

/// triage.md "Salvage": the valid causes of the newest attempt, when that attempt was a `Reject`
/// with at least one valid cause and the run is a still-`Running` Triage run without a result.
pub fn salvage(run: &RunRecord, last_evidence: Option<&EvidenceGateDetails>) -> Option<TriageResult> {
    if run.run.kind != RunKind::Triage || run.run.status != RunStatus::Running || run.run.result.is_some() {
        return None;
    }
    last_evidence?.salvage.clone().filter(|result| !result.causes.is_empty())
}

// ---------------------------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------------------------

/// Schema bounds and server rules (triage.md "Server-side validation"). Empty = valid.
pub fn validate_submission(sub: &TriageSubmission, max_causes: u8) -> Vec<RejectionItem> {
    let mut v = Validator::default();
    v.text("summary", &sub.summary, SUMMARY_MAX, Prose::Yes);
    if sub.causes.len() > CAUSES_MAX {
        v.problem("causes", format!("at most {CAUSES_MAX} causes"));
    } else if sub.causes.len() > usize::from(max_causes) {
        v.problem("causes", format!("at most {max_causes} causes for this triage"));
    }
    if sub.missing_info.len() > MISSING_INFO_MAX {
        v.problem("missing_info", format!("at most {MISSING_INFO_MAX} questions"));
    }
    if sub.causes.is_empty() && sub.missing_info.is_empty() {
        v.problem("missing_info", "required when no cause is submitted: list what to ask the reporter".to_owned());
    }
    for (i, q) in sub.missing_info.iter().enumerate() {
        v.text(&format!("missing_info[{i}]"), q, MISSING_INFO_ITEM_MAX, Prose::Yes);
    }
    for (i, cause) in sub.causes.iter().enumerate() {
        validate_cause(&mut v, i, cause);
    }
    v.items
}

fn validate_cause(v: &mut Validator, i: usize, c: &CauseDraftDto) {
    let at = |field: &str| format!("causes[{i}].{field}");
    v.text(&at("title"), &c.title, TITLE_MAX, Prose::Yes);
    v.text(&at("explanation"), &c.explanation, EXPLANATION_MAX, Prose::Yes);
    v.text(&at("confirm_if"), &c.confirm_if, CONDITION_MAX, Prose::Yes);
    v.text(&at("rule_out_if"), &c.rule_out_if, CONDITION_MAX, Prose::Yes);
    if c.repro_steps.len() > REPRO_STEPS_MAX {
        v.problem(&at("repro_steps"), format!("at most {REPRO_STEPS_MAX} steps"));
    }
    for (k, step) in c.repro_steps.iter().enumerate() {
        v.text(&at(&format!("repro_steps[{k}]")), step, REPRO_STEP_MAX, Prose::Yes);
    }
    if !(c.confidence.is_finite() && (0.0..=1.0).contains(&c.confidence)) {
        v.problem(&at("confidence"), "must be a number from 0 to 1".to_owned());
    }
    if c.evidence.is_empty() {
        v.problem(&at("evidence"), "at least one citation is required".to_owned());
    } else if c.evidence.len() > EVIDENCE_MAX {
        v.problem(&at("evidence"), format!("at most {EVIDENCE_MAX} citations"));
    }
    for (j, cit) in c.evidence.iter().enumerate() {
        validate_citation(v, &at(&format!("evidence[{j}]")), cit);
    }
}

fn validate_citation(v: &mut Validator, at: &str, c: &CitationDraftDto) {
    let n = c.path.chars().count();
    if n == 0 {
        v.problem(&format!("{at}.path"), "must not be empty".to_owned());
    } else if n > PATH_MAX {
        v.problem(&format!("{at}.path"), format!("longer than {PATH_MAX} characters"));
    }
    if c.start_line == 0 {
        v.problem(&format!("{at}.start_line"), "lines are 1-based".to_owned());
    }
    if c.end_line == 0 {
        v.problem(&format!("{at}.end_line"), "lines are 1-based".to_owned());
    }
    if let Some(quote) = &c.quote {
        v.text(&format!("{at}.quote"), quote, QUOTE_MAX, Prose::Quote);
    }
    if let Some(note) = &c.note {
        v.text(&format!("{at}.note"), note, NOTE_MAX, Prose::Yes);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Prose {
    /// No C0 controls except `\n`, `\t`.
    Yes,
    /// Quotes may also carry `\r` (CRLF files; the gate normalizes line ends).
    Quote,
}

#[derive(Default)]
struct Validator {
    items: Vec<RejectionItem>,
}

impl Validator {
    fn problem(&mut self, path: &str, reason: String) {
        self.items.push(RejectionItem { path: path.to_owned(), reason });
    }

    fn text(&mut self, path: &str, value: &str, max: usize, prose: Prose) {
        if value.trim().is_empty() {
            self.problem(path, "must not be empty".to_owned());
            return;
        }
        if value.chars().count() > max {
            self.problem(path, format!("longer than {max} characters"));
        }
        let allowed = |c: char| c == '\n' || c == '\t' || (prose == Prose::Quote && c == '\r');
        if value.chars().any(|c| c < ' ' && !allowed(c)) {
            self.problem(path, "contains control characters".to_owned());
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Parse and check (pure)
// ---------------------------------------------------------------------------------------------

/// A validated submission converted to domain drafts. Citations failing gate step 1 (path syntax)
/// or the range shape never reach the draft; they keep their defect here.
#[derive(Debug, Clone)]
pub(crate) struct ParsedSubmission {
    pub summary: String,
    pub missing_info: Vec<String>,
    pub causes: Vec<ParsedCause>,
}

#[derive(Debug, Clone)]
pub(crate) struct ParsedCause {
    pub draft: CauseDraft,
    pub citations: Vec<ParsedCitation>,
}

#[derive(Debug, Clone)]
pub(crate) struct ParsedCitation {
    pub raw_path: String,
    pub start_line: u32,
    pub end_line: u32,
    /// Index into `draft.evidence`, or the defect found before the manifest check.
    pub slot: Result<usize, CitationDefect>,
}

impl ParsedSubmission {
    /// Requires `validate_submission(&sub, _)` to be empty (confidence in range).
    pub(crate) fn from_valid(sub: TriageSubmission) -> Self {
        let causes = sub.causes.into_iter().map(ParsedCause::from_dto).collect();
        Self {
            summary: sub.summary.trim().to_owned(),
            missing_info: sub.missing_info.iter().map(|q| q.trim().to_owned()).collect(),
            causes,
        }
    }

    fn cited_paths(&self) -> Vec<RepoPath> {
        self.causes.iter().flat_map(|c| c.draft.evidence.iter().map(|e| e.path.clone())).collect()
    }
}

impl ParsedCause {
    fn from_dto(dto: CauseDraftDto) -> Self {
        let mut evidence = Vec::with_capacity(dto.evidence.len());
        let mut citations = Vec::with_capacity(dto.evidence.len());
        for c in dto.evidence {
            let slot = evidence::normalize_cited_path(&c.path).and_then(|path| {
                let lines = evidence::cited_range(c.start_line, c.end_line)?;
                evidence.push(CitationDraft {
                    path,
                    lines,
                    quote: c.quote.clone(),
                    note: c.note.as_deref().map(str::trim).map(str::to_owned),
                });
                Ok(evidence.len() - 1)
            });
            citations.push(ParsedCitation { raw_path: c.path, start_line: c.start_line, end_line: c.end_line, slot });
        }
        // Validation guarantees the range; clamp defensively instead of panicking.
        let confidence = Confidence::new(dto.confidence.clamp(0.0, 1.0)).unwrap_or_else(|_| Confidence::new(0.0).expect("0 is valid"));
        let draft = CauseDraft {
            title: dto.title.trim().to_owned(),
            explanation: dto.explanation.trim().to_owned(),
            evidence,
            repro_steps: dto.repro_steps.iter().map(|s| s.trim().to_owned()).collect(),
            confirm_if: dto.confirm_if.trim().to_owned(),
            rule_out_if: dto.rule_out_if.trim().to_owned(),
            confidence,
            method: dto.method,
        };
        Self { draft, citations }
    }
}

/// One checked cause: its stored verdict and, when valid, the accepted cause for ranking.
pub(crate) struct CheckedCause {
    pub check: CauseCheck,
    pub accepted: Option<AcceptedCause>,
}

/// Runs `evidence::check` on every cause and maps the report back onto the submitted citations.
pub(crate) fn check_parsed(parsed: &ParsedSubmission, view: &ManifestView) -> Vec<CheckedCause> {
    parsed
        .causes
        .iter()
        .enumerate()
        .map(|(i, cause)| {
            let report = evidence::check(&cause.draft.evidence, view);
            let citations = cause
                .citations
                .iter()
                .map(|c| match c.slot {
                    Ok(k) => report.items().get(k).copied().unwrap_or(CitationCheck::Rejected(CitationDefect::PathNotFound)),
                    Err(defect) => CitationCheck::Rejected(defect),
                })
                .collect();
            let verdict = report.cause();
            let accepted = (verdict == CauseEvidence::Valid).then(|| AcceptedCause::new(i, cause.draft.clone(), &report).ok()).flatten();
            let verdict = if accepted.is_some() || verdict != CauseEvidence::Valid { verdict } else { CauseEvidence::NoValidCitation };
            CheckedCause {
                check: CauseCheck { index: u8::try_from(i).unwrap_or(u8::MAX), title: cause.draft.title.clone(), verdict, citations },
                accepted,
            }
        })
        .collect()
}

/// Normalized paths of citations that failed with `PathNotFound`.
fn missing_paths(parsed: &ParsedSubmission, checked: &[CheckedCause]) -> Vec<RepoPath> {
    let mut out: Vec<RepoPath> = Vec::new();
    for (cause, checked) in parsed.causes.iter().zip(checked) {
        for (c, check) in cause.citations.iter().zip(&checked.check.citations) {
            if let (Ok(k), CitationCheck::Rejected(CitationDefect::PathNotFound)) = (c.slot, check) {
                let path = &cause.draft.evidence[k].path;
                if !out.contains(path) {
                    out.push(path.clone());
                }
            }
        }
    }
    out
}

/// Feedback-only data the pure check does not have.
#[derive(Debug, Clone, Default)]
pub(crate) struct FeedbackHints {
    /// First 8 hex digits of the target sha.
    pub short_sha: String,
    /// Same-basename manifest paths per missing path.
    pub similar: BTreeMap<RepoPath, Vec<RepoPath>>,
}

fn short_sha(key: &RepoVersionKey) -> String {
    key.sha.to_string().chars().take(8).collect()
}

// ---------------------------------------------------------------------------------------------
// Decision (pure)
// ---------------------------------------------------------------------------------------------

/// triage.md "Decision table". `attempt` is 1-based; an attempt at or past `max_attempts` is
/// final.
pub(crate) fn decide(
    attempt: u8,
    max_attempts: u8,
    parsed: ParsedSubmission,
    checked: Vec<CheckedCause>,
    hints: &FeedbackHints,
) -> TriageGateDecision {
    let submitted = checked.len();
    let is_final = attempt >= max_attempts;
    let attempts_left = max_attempts.saturating_sub(attempt);
    let checks: Vec<CauseCheck> = checked.iter().map(|c| c.check.clone()).collect();
    let feedback_lines = cause_lines(&parsed, &checks, hints);
    let valid: Vec<AcceptedCause> = checked.into_iter().filter_map(|c| c.accepted).collect();
    let valid_count = valid.len();
    let invalid_count = submitted - valid_count;

    let merged = merge_and_rank(valid);
    let notices = merged.notices.clone();
    let merged_away = valid_count - merged.causes.len();
    let ranked = merged.into_drafts();
    let result = TriageResult { summary: parsed.summary.clone(), causes: ranked, missing_info: parsed.missing_info.clone() };

    if invalid_count == 0 || (is_final && valid_count > 0) {
        let message = accept_message(result.causes.len(), invalid_count, merged_away, result.missing_info.len());
        let details = EvidenceGateDetails { causes: checks, salvage: None, notices };
        return TriageGateDecision::Accept { result, details, message };
    }
    if !is_final {
        let salvage = (valid_count > 0).then_some(result);
        let feedback = format!(
            "REJECTED (attempt {attempt} of {max_attempts}). Fix the problems and call submit_triage again with the COMPLETE list of causes, including the ones marked ok.\n{feedback_lines}"
        );
        let details = EvidenceGateDetails { causes: checks, salvage, notices };
        return TriageGateDecision::Reject { details, feedback, attempts_left };
    }
    let message = format!(
        "REJECTED (attempt {attempt} of {max_attempts}, final). No cause has a citation with a verified quote, so the triage failed. Do not call submit_triage again; end your turn.\n{feedback_lines}"
    );
    TriageGateDecision::Fail { details: EvidenceGateDetails { causes: checks, salvage: None, notices: Vec::new() }, message }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn accept_message(accepted: usize, dropped: usize, merged: usize, questions: usize) -> String {
    if accepted == 0 && dropped == 0 {
        return format!("Accepted: no causes, {} for the reporter. End your turn now.", plural(questions, "question", "questions"));
    }
    let mut message = format!("Accepted {}", plural(accepted, "cause", "causes"));
    if dropped > 0 {
        message.push_str(&format!("; {} without verified evidence dropped", plural(dropped, "cause", "causes")));
    }
    if merged > 0 {
        message.push_str(&format!("; {} merged", plural(merged, "duplicate cause", "duplicate causes")));
    }
    message.push_str(". End your turn now.");
    message
}

fn verdict_text(verdict: CauseEvidence) -> &'static str {
    match verdict {
        CauseEvidence::Valid => "ok",
        CauseEvidence::NoValidCitation => "no valid citation",
        CauseEvidence::NoVerifiedQuote => "no citation with a verified quote",
    }
}

/// One header line per cause; evidence lines for invalid causes; dropped citations of valid
/// causes summarized on the header.
fn cause_lines(parsed: &ParsedSubmission, checks: &[CauseCheck], hints: &FeedbackHints) -> String {
    let mut out = Vec::new();
    for (cause, check) in parsed.causes.iter().zip(checks) {
        let title = single_line(&cause.draft.title, TITLE_MAX);
        let n = usize::from(check.index) + 1;
        if check.verdict == CauseEvidence::Valid {
            let dropped: Vec<String> = cause
                .citations
                .iter()
                .zip(&check.citations)
                .filter_map(|(c, check)| match check {
                    CitationCheck::Rejected(defect) => Some(dropped_text(c, *defect, cause, hints)),
                    CitationCheck::Ok { .. } => None,
                })
                .collect();
            if dropped.is_empty() {
                out.push(format!("cause {n} \"{title}\": ok"));
            } else {
                out.push(format!(
                    "cause {n} \"{title}\": ok ({} dropped: {})",
                    plural(dropped.len(), "citation", "citations"),
                    dropped.join("; ")
                ));
            }
            continue;
        }
        out.push(format!("cause {n} \"{title}\": INVALID ({})", verdict_text(check.verdict)));
        for (j, (c, check)) in cause.citations.iter().zip(&check.citations).enumerate() {
            let reason = match check {
                CitationCheck::Rejected(defect) => defect_text(c, *defect, cause, hints),
                CitationCheck::Ok { quote: QuoteCheck::Absent, .. } => {
                    "no quote; add a quote copied verbatim (1 to 10 lines) from these lines".to_owned()
                }
                CitationCheck::Ok { .. } => continue,
            };
            out.push(format!("  evidence {} {}: {reason}", j + 1, location(c)));
        }
    }
    out.join("\n")
}

fn location(c: &ParsedCitation) -> String {
    format!("{}:{}-{}", single_line(&c.raw_path, PATH_MAX), c.start_line, c.end_line)
}

fn defect_text(c: &ParsedCitation, defect: CitationDefect, cause: &ParsedCause, hints: &FeedbackHints) -> String {
    match defect {
        CitationDefect::BadPath => "invalid path; use a repo-relative path without ./, .., \\ or a trailing /".to_owned(),
        CitationDefect::PathNotFound => {
            let mut text = format!("path not found at {}", hints.short_sha);
            let similar = c.slot.ok().and_then(|k| cause.draft.evidence.get(k)).and_then(|d| hints.similar.get(&d.path));
            if let Some(similar) = similar.filter(|s| !s.is_empty()) {
                let names: Vec<&str> = similar.iter().map(RepoPath::as_str).collect();
                text.push_str(&format!("; similar: {}", names.join(", ")));
            }
            text
        }
        CitationDefect::NotAFile => "not a regular file (symlink or submodule)".to_owned(),
        CitationDefect::NotText => "not a stored text file (binary, empty or too large)".to_owned(),
        CitationDefect::BadRange => "invalid line range; lines are 1-based and start_line <= end_line".to_owned(),
        CitationDefect::LineOutOfRange { line_count } => format!("beyond end of file ({line_count} lines)"),
        CitationDefect::SpanTooLarge => format!("more than {} lines cited; cite only the relevant lines", evidence::MAX_CITATION_SPAN),
        CitationDefect::QuoteTooShort => {
            format!("quote has fewer than {} non-whitespace characters; quote a longer passage", evidence::MIN_QUOTE_CHARS)
        }
        CitationDefect::QuoteTruncated => "quote contains a read_file truncation marker; quote complete text only".to_owned(),
        CitationDefect::QuoteMismatch => "quote not found in or near these lines; re-read the file".to_owned(),
        CitationDefect::QuoteAmbiguous => "quote matches several places; cite the exact lines".to_owned(),
    }
}

fn dropped_text(c: &ParsedCitation, defect: CitationDefect, cause: &ParsedCause, hints: &FeedbackHints) -> String {
    match defect {
        CitationDefect::LineOutOfRange { line_count } => {
            format!("lines {}-{} beyond end of file, {line_count} lines", c.start_line, c.end_line)
        }
        other => format!("{} {}", location(c), defect_text(c, other, cause, hints)),
    }
}

#[cfg(test)]
#[path = "gate_tests.rs"]
mod tests;
