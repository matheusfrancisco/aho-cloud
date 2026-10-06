//! `FixBrief` (domain.md "FixBrief and Resolution"; fix.md Additions).

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::domain::cause::{Cause, CauseStatus};
use crate::domain::error::DomainError;
use crate::domain::evidence::EvidenceChecked;
use crate::domain::ids::{ArtifactId, CauseId, FixBriefId, OrgId, RunId};

/// Markdown brief for a reproduced cause; one row per version (regeneration bumps `version`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FixBrief {
    pub id: FixBriefId,
    pub org_id: OrgId,
    pub cause_id: CauseId,
    /// 1-based; assigned by the store as max + 1 per cause.
    pub version: u32,
    /// The Diagnose run whose repro gate passed.
    pub diagnose_run: RunId,
    /// The `ArtifactKind::FixBrief` artifact holding `markdown`.
    pub artifact: ArtifactId,
    pub markdown: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// Fields of a brief that are not derived from the cause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixBriefInit {
    pub id: FixBriefId,
    pub version: u32,
    pub artifact: ArtifactId,
    pub markdown: String,
    pub created_at: OffsetDateTime,
}

impl FixBrief {
    /// Only for a `Reproduced` cause, and only with an evidence proof over the citations printed
    /// in the brief (F1's gate).
    pub fn new(cause: &Cause, evidence: EvidenceChecked, init: FixBriefInit) -> Result<FixBrief, DomainError> {
        let CauseStatus::Reproduced { run, .. } = cause.status else {
            return Err(DomainError::invalid(format!("fix brief needs a reproduced cause, cause is {}", cause.status.name())));
        };
        if evidence.citations().is_empty() {
            return Err(DomainError::MissingEvidence);
        }
        if init.version == 0 {
            return Err(DomainError::invalid("fix brief versions start at 1"));
        }
        Ok(FixBrief {
            id: init.id,
            org_id: cause.org_id,
            cause_id: cause.id,
            version: init.version,
            diagnose_run: run,
            artifact: init.artifact,
            markdown: init.markdown,
            created_at: init.created_at,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::cause::CauseData;
    use crate::domain::citation::{Confidence, ReproMethod};
    use crate::domain::evidence::{ManifestView, check};
    use crate::domain::ids::{BlobOid, CommitSha, GateId, RepoId, RepoVersionKey, TicketId};
    use crate::domain::manifest::{EntryKind, ManifestEntry};
    use crate::domain::path::{FileClass, LineRange, RepoPath};
    use crate::domain::triage::CitationDraft;
    use time::macros::datetime;
    use uuid::Uuid;

    fn cause(status: CauseStatus) -> Cause {
        Cause::rehydrate(CauseData {
            id: CauseId::from_uuid(Uuid::from_u128(1)),
            ticket_id: TicketId::from_uuid(Uuid::from_u128(2)),
            org_id: OrgId::from_uuid(Uuid::from_u128(3)),
            triage_run: RunId::from_uuid(Uuid::from_u128(4)),
            rank: 1,
            title: "t".into(),
            explanation: "e".into(),
            evidence: vec![],
            repro_steps: vec![],
            confirm_if: "c".into(),
            rule_out_if: "r".into(),
            confidence: Confidence::new(0.5).unwrap(),
            method: ReproMethod::Api,
            status,
            superseded_at: None,
        })
    }

    fn proof() -> EvidenceChecked {
        let blob = BlobOid::from_bytes([1; 20]);
        let lines: Vec<String> = (1..=5).map(|i| format!("let total_{i} = invoice.customer.id;")).collect();
        let view = ManifestView::new(
            RepoVersionKey::new(RepoId::from_uuid(Uuid::from_u128(5)), CommitSha::from_bytes([2; 20])),
            vec![ManifestEntry {
                path: RepoPath::new("a.ts").unwrap(),
                kind: EntryKind::File,
                executable: false,
                blob,
                lang: None,
                class: FileClass::Source,
                size: 100,
                is_text: true,
                line_count: 5,
            }],
            vec![(blob, lines)],
        );
        let drafts = [CitationDraft {
            path: RepoPath::new("a.ts").unwrap(),
            lines: LineRange::new(2, 2).unwrap(),
            quote: Some("let total_2 = invoice.customer.id;".into()),
            note: None,
        }];
        check(&drafts, &view).into_checked().unwrap()
    }

    fn init() -> FixBriefInit {
        FixBriefInit {
            id: FixBriefId::from_uuid(Uuid::from_u128(9)),
            version: 1,
            artifact: ArtifactId::from_uuid(Uuid::from_u128(10)),
            markdown: "# Fix brief".into(),
            created_at: datetime!(2026-05-01 00:00 UTC),
        }
    }

    #[test]
    fn brief_only_for_reproduced_cause() {
        let diag = RunId::from_uuid(Uuid::from_u128(6));
        let reproduced = cause(CauseStatus::Reproduced { run: diag, gate: GateId::from_uuid(Uuid::from_u128(7)) });
        let brief = FixBrief::new(&reproduced, proof(), init()).unwrap();
        assert_eq!((brief.diagnose_run, brief.org_id, brief.cause_id), (diag, reproduced.org_id, reproduced.id));
        for status in [
            CauseStatus::Untested,
            CauseStatus::Running(diag),
            CauseStatus::NotReproduced { run: diag },
            CauseStatus::Inconclusive { run: diag, reason: "x".into() },
        ] {
            assert!(FixBrief::new(&cause(status), proof(), init()).is_err());
        }
        let v0 = FixBriefInit { version: 0, ..init() };
        assert!(FixBrief::new(&reproduced, proof(), v0).is_err());
    }
}
