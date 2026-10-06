//! `Citation`, `Confidence`, `ReproMethod` (domain.md "Cause", triage.md Additions).

use serde::{Deserialize, Serialize};

use crate::domain::error::DomainError;
use crate::domain::ids::{CommitSha, RepoId, RepoVersionKey, hex_array};
use crate::domain::path::{LineRange, RepoPath};

/// Maximum `Citation.note` length in Unicode scalar values.
pub const CITATION_NOTE_MAX_CHARS: usize = 200;

/// A checked location in the code at a commit.
///
/// `snippet_hash` is SHA-256 over the cited lines (no terminators) joined with `\n`; always `Some`
/// for citations produced by the evidence gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
pub struct Citation {
    pub repo_id: RepoId,
    pub sha: CommitSha,
    pub path: RepoPath,
    pub lines: LineRange,
    #[serde(with = "hex_array::option", default)]
    #[ts(type = "string | null")]
    pub snippet_hash: Option<[u8; 32]>,
    #[serde(default)]
    pub note: Option<String>,
}

impl Citation {
    pub fn version(&self) -> RepoVersionKey {
        RepoVersionKey::new(self.repo_id, self.sha)
    }
}

/// Probability-like confidence in `0.0..=1.0` (never NaN).
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize, Deserialize, ts_rs::TS)]
#[serde(try_from = "f64", into = "f64")]
pub struct Confidence(f64);

impl Confidence {
    pub fn new(value: f64) -> Result<Self, DomainError> {
        if (0.0..=1.0).contains(&value) { Ok(Self(value)) } else { Err(DomainError::invalid(format!("confidence {value} outside 0..=1"))) }
    }

    pub const fn get(&self) -> f64 {
        self.0
    }
}

impl TryFrom<f64> for Confidence {
    type Error = DomainError;

    fn try_from(value: f64) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<Confidence> for f64 {
    fn from(c: Confidence) -> f64 {
        c.0
    }
}

/// How a cause is expected to be reproduced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "lowercase")]
pub enum ReproMethod {
    Ui,
    Api,
    Job,
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confidence_bounds() {
        assert!(Confidence::new(0.0).is_ok());
        assert!(Confidence::new(1.0).is_ok());
        assert!(Confidence::new(-0.0001).is_err());
        assert!(Confidence::new(1.0001).is_err());
        assert!(Confidence::new(f64::NAN).is_err());
        assert!(serde_json::from_str::<Confidence>("1.5").is_err());
        assert_eq!(serde_json::from_str::<Confidence>("0.25").unwrap().get(), 0.25);
    }

    #[test]
    fn repro_method_serde_lowercase() {
        assert_eq!(serde_json::to_string(&ReproMethod::Ui).unwrap(), "\"ui\"");
        assert_eq!(serde_json::from_str::<ReproMethod>("\"unknown\"").unwrap(), ReproMethod::Unknown);
    }
}
