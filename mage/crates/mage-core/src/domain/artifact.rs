//! `Artifact` and `ArtifactKind` (domain.md "Artifact").

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::domain::ids::{ArtifactId, OrgId, RunId, hex_array};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Video,
    Gif,
    Trace,
    Screenshot,
    Log,
    Script,
    Patch,
    Transcript,
    ScipIndex,
    SourceBundle,
    FixBrief,
}

impl ArtifactKind {
    /// Path segment in `org/{org}/runs/{run}/{kind}/{name}` and the storage value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Video => "video",
            Self::Gif => "gif",
            Self::Trace => "trace",
            Self::Screenshot => "screenshot",
            Self::Log => "log",
            Self::Script => "script",
            Self::Patch => "patch",
            Self::Transcript => "transcript",
            Self::ScipIndex => "scip_index",
            Self::SourceBundle => "source_bundle",
            Self::FixBrief => "fix_brief",
        }
    }
}

/// Stored bytes produced by a run. `key` follows `org/{org}/runs/{run}/{kind}/{name}`
/// (built by `mage_core::ports::blob::BlobKey::run_artifact`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    pub id: ArtifactId,
    pub org_id: OrgId,
    pub run_id: RunId,
    pub kind: ArtifactKind,
    pub key: String,
    pub bytes: u64,
    #[serde(with = "hex_array")]
    pub sha256: [u8; 32],
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_str_matches_serde_name() {
        for kind in [
            ArtifactKind::Video,
            ArtifactKind::Gif,
            ArtifactKind::Trace,
            ArtifactKind::Screenshot,
            ArtifactKind::Log,
            ArtifactKind::Script,
            ArtifactKind::Patch,
            ArtifactKind::Transcript,
            ArtifactKind::ScipIndex,
            ArtifactKind::SourceBundle,
            ArtifactKind::FixBrief,
        ] {
            assert_eq!(serde_json::to_value(kind).unwrap(), serde_json::json!(kind.as_str()));
        }
    }
}
