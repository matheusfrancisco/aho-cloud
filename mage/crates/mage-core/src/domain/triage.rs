//! Triage drafts and results (triage.md "submit_triage schema", Additions) and the pure
//! `merge_and_rank` (triage.md "Merge and rank").

use std::cmp::Ordering;
use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::domain::citation::{Confidence, ReproMethod};
use crate::domain::error::DomainError;
use crate::domain::evidence::{CauseEvidence, CitationCheck, EvidenceReport};
use crate::domain::path::{LineRange, RepoPath};

/// Maximum citations of a merged cause.
pub const MAX_MERGED_CITATIONS: usize = 8;

/// An agent-proposed cause, before (or, inside `TriageResult`, after) the evidence gate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
pub struct CauseDraft {
    pub title: String,
    pub explanation: String,
    pub evidence: Vec<CitationDraft>,
    pub repro_steps: Vec<String>,
    pub confirm_if: String,
    pub rule_out_if: String,
    pub confidence: Confidence,
    pub method: ReproMethod,
}

/// An agent citation; `quote` is how the gate verifies the agent read the code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
pub struct CitationDraft {
    pub path: RepoPath,
    pub lines: LineRange,
    #[serde(default)]
    pub quote: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

/// Result of a Triage run. `causes` are the accepted drafts after the gate, merge and ranking
/// (rank = position + 1), with adjusted line ranges applied.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
pub struct TriageResult {
    pub summary: String,
    pub causes: Vec<CauseDraft>,
    pub missing_info: Vec<String>,
}

/// A cause whose evidence report is `Valid`, reduced to its passing citations.
///
/// `evidence` keeps the passing citations in report order (deduped, final line ranges, original
/// quote and note), so re-checking the draft later yields `Exact` quotes at the same ranges.
#[derive(Debug, Clone, PartialEq)]
pub struct AcceptedCause {
    index: usize,
    draft: CauseDraft,
    verified: Vec<bool>,
}

impl AcceptedCause {
    /// `index` is the cause's position in the submission; `report` is `check(&draft.evidence, ..)`.
    ///
    /// `MissingEvidence` unless the report is `Valid`; `Invalid` if the report was not produced
    /// from `draft.evidence`.
    pub fn new(index: usize, mut draft: CauseDraft, report: &EvidenceReport) -> Result<Self, DomainError> {
        if report.cause() != CauseEvidence::Valid {
            return Err(DomainError::MissingEvidence);
        }
        let mismatch = || DomainError::invalid("evidence report does not belong to this cause draft");
        if report.items().len() != draft.evidence.len() {
            return Err(mismatch());
        }
        let mut slots: Vec<Option<CitationDraft>> = std::mem::take(&mut draft.evidence).into_iter().map(Some).collect();
        let mut evidence = Vec::with_capacity(report.citations().len());
        let mut verified = Vec::with_capacity(report.citations().len());
        for (citation, &source) in report.citations().iter().zip(report.sources()) {
            let mut cited = slots.get_mut(source).and_then(Option::take).ok_or_else(mismatch)?;
            let Some(CitationCheck::Ok { lines, quote }) = report.items().get(source) else {
                return Err(mismatch());
            };
            if cited.path != citation.path || *lines != citation.lines {
                return Err(mismatch());
            }
            cited.lines = *lines;
            evidence.push(cited);
            verified.push(quote.is_verified());
        }
        draft.evidence = evidence;
        Ok(Self { index, draft, verified })
    }

    pub fn index(&self) -> usize {
        self.index
    }

    pub fn draft(&self) -> &CauseDraft {
        &self.draft
    }

    pub fn into_draft(self) -> CauseDraft {
        self.draft
    }
}

/// A cause after merge and ranking.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RankedCause {
    /// 1-based, contiguous within one triage run.
    pub rank: u16,
    /// Submission index of the winning cause.
    pub index: usize,
    /// Submission indexes of the causes merged into this one.
    pub merged: Vec<usize>,
    pub draft: CauseDraft,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MergeOutcome {
    /// Rank order.
    pub causes: Vec<RankedCause>,
    /// One run `Notice` text per merged cause.
    pub notices: Vec<String>,
}

impl MergeOutcome {
    /// Drafts in rank order, as stored in `TriageResult.causes`.
    pub fn into_drafts(self) -> Vec<CauseDraft> {
        self.causes.into_iter().map(|c| c.draft).collect()
    }
}

/// What duplicate detection compares, borrowed from an `AcceptedCause`.
struct Footprint<'a> {
    /// First citation with a verified quote.
    primary: Option<(&'a RepoPath, LineRange)>,
    lines: BTreeSet<(&'a RepoPath, u32)>,
}

impl<'a> Footprint<'a> {
    fn of(cause: &'a AcceptedCause) -> Self {
        let primary = cause.draft.evidence.iter().zip(&cause.verified).find(|(_, verified)| **verified).map(|(c, _)| (&c.path, c.lines));
        let lines = cause.draft.evidence.iter().flat_map(|c| (c.lines.start()..=c.lines.end()).map(move |line| (&c.path, line))).collect();
        Self { primary, lines }
    }

    /// Same primary path with overlapping primary ranges, and Jaccard of line sets ≥ 0.5.
    fn duplicates(&self, other: &Self) -> bool {
        let (Some((path_a, lines_a)), Some((path_b, lines_b))) = (self.primary, other.primary) else {
            return false;
        };
        if path_a != path_b || !lines_a.overlaps(&lines_b) {
            return false;
        }
        let shared = self.lines.intersection(&other.lines).count();
        let union = self.lines.len() + other.lines.len() - shared;
        union > 0 && 2 * shared >= union
    }
}

/// Merges duplicate causes and ranks the rest (triage.md "Merge and rank").
///
/// Causes are visited by confidence descending, then submission index; each joins the first
/// already-kept cause it duplicates (compared on the original citations), else is kept. A kept
/// cause keeps its text fields and gets the union of citations (its own first, deduped by
/// `(path, lines)`, at most 8).
pub fn merge_and_rank(causes: Vec<AcceptedCause>) -> MergeOutcome {
    let mut order: Vec<usize> = (0..causes.len()).collect();
    order.sort_by(|&a, &b| by_priority(&causes[a], &causes[b]));

    let mut groups: Vec<(usize, Vec<usize>)> = Vec::new();
    {
        let footprints: Vec<Footprint<'_>> = causes.iter().map(Footprint::of).collect();
        for &candidate in &order {
            match groups.iter_mut().find(|(kept, _)| footprints[*kept].duplicates(&footprints[candidate])) {
                Some((_, absorbed)) => absorbed.push(candidate),
                None => groups.push((candidate, Vec::new())),
            }
        }
    }

    let mut slots: Vec<Option<AcceptedCause>> = causes.into_iter().map(Some).collect();
    let mut notices = Vec::new();
    let mut ranked = Vec::with_capacity(groups.len());
    for (kept, absorbed) in groups {
        let Some(winner) = slots.get_mut(kept).and_then(Option::take) else {
            continue;
        };
        let mut draft = winner.draft;
        draft.evidence.truncate(MAX_MERGED_CITATIONS);
        let mut merged = Vec::with_capacity(absorbed.len());
        for position in absorbed {
            let Some(loser) = slots.get_mut(position).and_then(Option::take) else {
                continue;
            };
            notices.push(format!(
                "merged cause {} \"{}\" into cause {} \"{}\"",
                loser.index + 1,
                loser.draft.title,
                winner.index + 1,
                draft.title
            ));
            merged.push(loser.index);
            for citation in loser.draft.evidence {
                if draft.evidence.len() >= MAX_MERGED_CITATIONS {
                    break;
                }
                if !draft.evidence.iter().any(|c| c.path == citation.path && c.lines == citation.lines) {
                    draft.evidence.push(citation);
                }
            }
        }
        let rank = u16::try_from(ranked.len() + 1).unwrap_or(u16::MAX);
        ranked.push(RankedCause { rank, index: winner.index, merged, draft });
    }
    MergeOutcome { causes: ranked, notices }
}

fn by_priority(a: &AcceptedCause, b: &AcceptedCause) -> Ordering {
    b.draft.confidence.get().total_cmp(&a.draft.confidence.get()).then(a.index.cmp(&b.index))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::evidence::{ManifestView, check};
    use crate::domain::ids::{BlobOid, CommitSha, RepoId, RepoVersionKey};
    use crate::domain::manifest::{EntryKind, ManifestEntry};
    use crate::domain::path::FileClass;
    use uuid::Uuid;

    fn line_text(path: &str, line: u32) -> String {
        format!("{path} statement number {line};")
    }

    fn view() -> ManifestView {
        let key = RepoVersionKey::new(RepoId::from_uuid(Uuid::from_u128(1)), CommitSha([1; 20]));
        let mut entries = Vec::new();
        let mut texts = Vec::new();
        for (i, path) in ["a.rs", "b.rs"].into_iter().enumerate() {
            let blob = BlobOid([u8::try_from(i + 1).unwrap(); 20]);
            entries.push(ManifestEntry {
                path: RepoPath::new(path).unwrap(),
                kind: EntryKind::File,
                executable: false,
                blob,
                lang: None,
                class: FileClass::Source,
                size: 1,
                is_text: true,
                line_count: 300,
            });
            texts.push((blob, (1..=300).map(|l| line_text(path, l)).collect()));
        }
        ManifestView::new(key, entries, texts)
    }

    /// `(path, start, end, quoted)`; a quoted citation quotes its first line.
    type Spec = (&'static str, u32, u32, bool);

    fn draft(title: &str, confidence: f64, evidence: &[Spec]) -> CauseDraft {
        CauseDraft {
            title: title.to_owned(),
            explanation: format!("{title} explained"),
            evidence: evidence
                .iter()
                .map(|&(path, start, end, quoted)| CitationDraft {
                    path: RepoPath::new(path).unwrap(),
                    lines: LineRange::new(start, end).unwrap(),
                    quote: quoted.then(|| line_text(path, start)),
                    note: None,
                })
                .collect(),
            repro_steps: vec![format!("{title} step")],
            confirm_if: "confirm".to_owned(),
            rule_out_if: "rule out".to_owned(),
            confidence: Confidence::new(confidence).unwrap(),
            method: ReproMethod::Api,
        }
    }

    fn accepted(causes: Vec<CauseDraft>) -> Vec<AcceptedCause> {
        let view = view();
        causes
            .into_iter()
            .enumerate()
            .map(|(i, d)| {
                let report = check(&d.evidence, &view);
                AcceptedCause::new(i, d, &report).unwrap()
            })
            .collect()
    }

    fn summary(out: &MergeOutcome) -> Vec<(u16, usize, Vec<usize>)> {
        out.causes.iter().map(|c| (c.rank, c.index, c.merged.clone())).collect()
    }

    fn spans(draft: &CauseDraft) -> Vec<(&str, u32, u32)> {
        draft.evidence.iter().map(|c| (c.path.as_str(), c.lines.start(), c.lines.end())).collect()
    }

    #[test]
    fn accepted_cause_keeps_passing_adjusted_citations() {
        let mut d = draft("c", 0.5, &[("a.rs", 1, 3, false), ("missing.rs", 1, 1, false), ("b.rs", 10, 10, false)]);
        // Quote of line 40 cited at 35: adjusted to 40.
        d.evidence.push(CitationDraft {
            path: RepoPath::new("a.rs").unwrap(),
            lines: LineRange::new(35, 35).unwrap(),
            quote: Some(line_text("a.rs", 40)),
            note: Some("why".to_owned()),
        });
        let view = view();
        let report = check(&d.evidence, &view);
        let cause = AcceptedCause::new(3, d, &report).unwrap();
        assert_eq!(spans(cause.draft()), vec![("a.rs", 1, 3), ("b.rs", 10, 10), ("a.rs", 40, 40)]);
        assert_eq!(cause.draft().evidence[2].note.as_deref(), Some("why"));
        // Re-checking the accepted draft passes with the quote now exact.
        let again = check(&cause.draft().evidence, &view);
        assert_eq!(again.citations(), report.citations());
    }

    #[test]
    fn accepted_cause_requires_valid_matching_report() {
        let view = view();
        let unquoted = draft("u", 0.5, &[("a.rs", 1, 3, false)]);
        let report = check(&unquoted.evidence, &view);
        assert_eq!(AcceptedCause::new(0, unquoted, &report), Err(DomainError::MissingEvidence));

        let a = draft("a", 0.5, &[("a.rs", 1, 3, true)]);
        let b = draft("b", 0.5, &[("b.rs", 1, 3, true)]);
        let report_a = check(&a.evidence, &view);
        assert!(matches!(AcceptedCause::new(0, b, &report_a), Err(DomainError::Invalid(_))));
    }

    #[test]
    fn distinct_causes_ordered_by_confidence_then_index() {
        let out = merge_and_rank(accepted(vec![
            draft("c0", 0.5, &[("a.rs", 1, 10, true)]),
            draft("c1", 0.9, &[("a.rs", 50, 60, true)]),
            draft("c2", 0.5, &[("b.rs", 1, 10, true)]),
            draft("c3", 0.7, &[("b.rs", 100, 110, true)]),
        ]));
        assert_eq!(summary(&out), vec![(1, 1, vec![]), (2, 3, vec![]), (3, 0, vec![]), (4, 2, vec![])]);
        assert!(out.notices.is_empty());
    }

    #[test]
    fn duplicate_merged_into_higher_confidence() {
        let out = merge_and_rank(accepted(vec![
            draft("low", 0.4, &[("a.rs", 1, 10, true)]),
            draft("high", 0.8, &[("a.rs", 1, 10, true), ("b.rs", 5, 5, false)]),
            draft("other", 0.6, &[("b.rs", 100, 110, true)]),
        ]));
        assert_eq!(summary(&out), vec![(1, 1, vec![0]), (2, 2, vec![])]);
        let winner = &out.causes[0].draft;
        assert_eq!(winner.title, "high");
        assert_eq!(winner.explanation, "high explained");
        assert_eq!(winner.confidence.get(), 0.8);
        assert_eq!(spans(winner), vec![("a.rs", 1, 10), ("b.rs", 5, 5)]);
        assert_eq!(out.notices.len(), 1);
        assert!(out.notices[0].contains("cause 1 \"low\""), "{}", out.notices[0]);
    }

    #[test]
    fn equal_confidence_duplicate_merged_into_earlier() {
        let out =
            merge_and_rank(accepted(vec![draft("first", 0.5, &[("a.rs", 1, 10, true)]), draft("second", 0.5, &[("a.rs", 2, 10, true)])]));
        assert_eq!(summary(&out), vec![(1, 0, vec![1])]);
        assert_eq!(out.causes[0].draft.title, "first");
        assert_eq!(spans(&out.causes[0].draft), vec![("a.rs", 1, 10), ("a.rs", 2, 10)]);
    }

    #[test]
    fn merged_citations_winner_first_deduped_capped_at_8() {
        let out = merge_and_rank(accepted(vec![
            draft(
                "winner",
                0.9,
                &[("a.rs", 1, 100, true), ("b.rs", 1, 1, false), ("b.rs", 2, 2, false), ("b.rs", 3, 3, false), ("b.rs", 4, 4, false)],
            ),
            draft(
                "loser",
                0.3,
                &[
                    ("a.rs", 1, 100, true),
                    ("b.rs", 10, 10, false),
                    ("b.rs", 11, 11, false),
                    ("b.rs", 12, 12, false),
                    ("b.rs", 13, 13, false),
                    ("b.rs", 14, 14, false),
                ],
            ),
        ]));
        assert_eq!(summary(&out), vec![(1, 0, vec![1])]);
        assert_eq!(
            spans(&out.causes[0].draft),
            vec![
                ("a.rs", 1, 100),
                ("b.rs", 1, 1),
                ("b.rs", 2, 2),
                ("b.rs", 3, 3),
                ("b.rs", 4, 4),
                ("b.rs", 10, 10),
                ("b.rs", 11, 11),
                ("b.rs", 12, 12),
            ]
        );
    }

    #[test]
    fn jaccard_threshold_is_inclusive() {
        // 5 shared of 10 lines: exactly 0.5.
        let out = merge_and_rank(accepted(vec![draft("a", 0.6, &[("a.rs", 1, 10, true)]), draft("b", 0.5, &[("a.rs", 1, 5, true)])]));
        assert_eq!(summary(&out), vec![(1, 0, vec![1])]);
        // 5 shared of 11 lines: just below.
        let out = merge_and_rank(accepted(vec![
            draft("a", 0.6, &[("a.rs", 1, 10, true)]),
            draft("b", 0.5, &[("a.rs", 1, 5, true), ("b.rs", 1, 1, false)]),
        ]));
        assert_eq!(summary(&out), vec![(1, 0, vec![]), (2, 1, vec![])]);
    }

    #[test]
    fn primaries_must_share_path_and_overlap() {
        // Jaccard 21/33 but primaries 1-10 and 11-12 are disjoint.
        let out = merge_and_rank(accepted(vec![
            draft("a", 0.6, &[("a.rs", 1, 10, true), ("a.rs", 20, 40, false)]),
            draft("b", 0.5, &[("a.rs", 11, 12, true), ("a.rs", 20, 40, false)]),
        ]));
        assert_eq!(summary(&out), vec![(1, 0, vec![]), (2, 1, vec![])]);
        // Identical line sets, primaries in different files.
        let out = merge_and_rank(accepted(vec![
            draft("a", 0.6, &[("a.rs", 1, 10, true), ("b.rs", 1, 10, false)]),
            draft("b", 0.5, &[("b.rs", 1, 10, true), ("a.rs", 1, 10, false)]),
        ]));
        assert_eq!(summary(&out), vec![(1, 0, vec![]), (2, 1, vec![])]);
    }

    #[test]
    fn primary_is_first_verified_citation() {
        // b's first citation is unquoted; its primary is a.rs 1-10, matching a's.
        let out = merge_and_rank(accepted(vec![
            draft("a", 0.6, &[("a.rs", 1, 10, true), ("b.rs", 1, 10, false)]),
            draft("b", 0.5, &[("b.rs", 1, 10, false), ("a.rs", 1, 10, true)]),
        ]));
        assert_eq!(summary(&out), vec![(1, 0, vec![1])]);
    }

    #[test]
    fn into_drafts_in_rank_order() {
        let out = merge_and_rank(accepted(vec![draft("c0", 0.2, &[("a.rs", 1, 10, true)]), draft("c1", 0.9, &[("b.rs", 1, 10, true)])]));
        let titles: Vec<String> = out.into_drafts().into_iter().map(|d| d.title).collect();
        assert_eq!(titles, vec!["c1", "c0"]);
    }
}
