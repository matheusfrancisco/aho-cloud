//! Change set ↔ gate binding (fix.md Additions: `ChangeSet`, `ChangeSetDigest`) and regression
//! test placement (fix.md "Regression test placement").

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::domain::error::DomainError;
use crate::domain::fix::settings::RegressionMode;
use crate::domain::ids::{ArtifactId, CommitSha, hex_array, hex_bytes_id};
use crate::domain::path::RepoPath;

hex_bytes_id!(
    /// SHA-256 binding a change set (base, patch bytes, regression file) to its fix gate.
    ChangeSetDigest, 32
);

impl ChangeSetDigest {
    /// SHA-256 over `base_sha (20) ‖ patch_sha256 (32) ‖ 0x00` without a regression file, or
    /// `… ‖ 0x01 ‖ u32 BE path length ‖ path UTF-8 ‖ script_sha256 (32)` with one.
    pub fn compute(base_sha: &CommitSha, patch_sha256: &[u8; 32], regression: Option<(&RepoPath, &[u8; 32])>) -> ChangeSetDigest {
        let mut hasher = Sha256::new();
        hasher.update(base_sha.as_bytes());
        hasher.update(patch_sha256);
        match regression {
            None => hasher.update([0x00]),
            Some((path, script_sha256)) => {
                let path = path.as_str().as_bytes();
                hasher.update([0x01]);
                // `RepoPath` is at most 4096 bytes, so the length always fits.
                hasher.update(u32::try_from(path.len()).unwrap_or(u32::MAX).to_be_bytes());
                hasher.update(path);
                hasher.update(script_sha256);
            }
        }
        ChangeSetDigest(hasher.finalize().into())
    }
}

/// The repro script committed as a regression test (placement decided when the change set is
/// built, so the gate runs with the file in place).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegressionFile {
    pub path: RepoPath,
    pub script: ArtifactId,
    #[serde(with = "hex_array")]
    pub script_sha256: [u8; 32],
}

/// Bytes of the regression file at its placed path, as committed (`CommitPlan::build`) and
/// counted toward the size limits (`PatchPolicy::check`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegressionContent {
    pub path: RepoPath,
    pub bytes: Vec<u8>,
}

impl RegressionContent {
    /// SHA-256 of `bytes` (the `script_sha256` the change set binds).
    pub fn sha256(&self) -> [u8; 32] {
        Sha256::digest(&self.bytes).into()
    }
}

/// Exactly what the fix gate verified and what a PR may commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeSet {
    pub base_sha: CommitSha,
    pub patch: ArtifactId,
    #[serde(with = "hex_array")]
    pub patch_sha256: [u8; 32],
    pub regression: Option<RegressionFile>,
}

impl ChangeSet {
    pub fn digest(&self) -> ChangeSetDigest {
        ChangeSetDigest::compute(&self.base_sha, &self.patch_sha256, self.regression.as_ref().map(|r| (&r.path, &r.script_sha256)))
    }
}

/// Where the regression test goes (fix.md table), carried in `FixSpec`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RegressionPlacement {
    Off,
    /// `.mage/regressions/{identifier}-{slug}.{ext}`
    #[default]
    MageDir,
    /// `{dir}/mage-{identifier}-{slug}.{ext}`, `dir` from recipe `[fix].regression_dir`.
    RepoTests {
        dir: RepoPath,
    },
}

/// Longest slug kept in a regression file name.
const SLUG_MAX: usize = 48;

impl RegressionPlacement {
    /// Placement for the org setting and the recipe's `[fix].regression_dir`. `RepoTests` without
    /// a recipe directory falls back to the default `MageDir`.
    pub fn resolve(mode: RegressionMode, regression_dir: Option<&RepoPath>) -> RegressionPlacement {
        match (mode, regression_dir) {
            (RegressionMode::Off, _) => RegressionPlacement::Off,
            (RegressionMode::RepoTests, Some(dir)) => RegressionPlacement::RepoTests { dir: dir.clone() },
            (RegressionMode::MageDir | RegressionMode::RepoTests, _) => RegressionPlacement::MageDir,
        }
    }

    /// Path of the regression file, `None` when `Off`.
    /// `identifier`: ticket identifier (lowercased; `[a-z0-9._-]` after lowercasing);
    /// `title`: slugified (`[a-z0-9]` runs joined by `-`, at most 48 bytes, `repro` if empty);
    /// `ext`: script extension without the leading dot (e.g. `spec.ts`, `sh`).
    pub fn path(&self, identifier: &str, title: &str, ext: &str) -> Result<Option<RepoPath>, DomainError> {
        if *self == RegressionPlacement::Off {
            return Ok(None);
        }
        let identifier = identifier.to_ascii_lowercase();
        if identifier.is_empty() || !identifier.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b)) {
            return Err(DomainError::invalid(format!("ticket identifier {identifier:?} is not usable in a file name")));
        }
        let ext_ok = !ext.is_empty() && ext.split('.').all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_alphanumeric()));
        if !ext_ok {
            return Err(DomainError::invalid(format!("script extension {ext:?} is invalid")));
        }
        let slug = slugify(title);
        let path = match self {
            RegressionPlacement::Off => return Ok(None),
            RegressionPlacement::MageDir => format!(".mage/regressions/{identifier}-{slug}.{ext}"),
            RegressionPlacement::RepoTests { dir } => format!("{dir}/mage-{identifier}-{slug}.{ext}"),
        };
        RepoPath::new(path).map(Some)
    }
}

/// Lowercase ASCII alphanumeric runs joined by single `-`, at most `SLUG_MAX` bytes.
fn slugify(title: &str) -> String {
    let mut slug = String::with_capacity(title.len().min(SLUG_MAX));
    for c in title.chars() {
        if slug.len() == SLUG_MAX {
            break;
        }
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        slug.push_str("repro");
    }
    slug
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn change_set(regression: Option<RegressionFile>) -> ChangeSet {
        ChangeSet {
            base_sha: CommitSha::from_bytes([4; 20]),
            patch: ArtifactId::from_uuid(Uuid::from_u128(10)),
            patch_sha256: [7; 32],
            regression,
        }
    }

    fn regression(path: &str, sha: u8) -> RegressionFile {
        RegressionFile { path: RepoPath::new(path).unwrap(), script: ArtifactId::from_uuid(Uuid::from_u128(1)), script_sha256: [sha; 32] }
    }

    #[test]
    fn digest_is_the_specified_sha256_layout() {
        let none = change_set(None);
        let mut want = Vec::new();
        want.extend_from_slice(&[4; 20]);
        want.extend_from_slice(&[7; 32]);
        want.push(0);
        assert_eq!(none.digest().0, <[u8; 32]>::from(Sha256::digest(&want)));

        let with = change_set(Some(regression(".mage/regressions/eng-1-x.spec.ts", 3)));
        let path = b".mage/regressions/eng-1-x.spec.ts";
        let mut want = Vec::new();
        want.extend_from_slice(&[4; 20]);
        want.extend_from_slice(&[7; 32]);
        want.push(1);
        want.extend_from_slice(&(path.len() as u32).to_be_bytes());
        want.extend_from_slice(path);
        want.extend_from_slice(&[3; 32]);
        assert_eq!(with.digest().0, <[u8; 32]>::from(Sha256::digest(&want)));
    }

    #[test]
    fn digest_is_deterministic_and_binds_every_component() {
        let base = change_set(Some(regression(".mage/regressions/eng-1-x.spec.ts", 3)));
        assert_eq!(base.digest(), base.clone().digest());
        let variants = [
            change_set(None),
            ChangeSet { base_sha: CommitSha::from_bytes([5; 20]), ..base.clone() },
            ChangeSet { patch_sha256: [8; 32], ..base.clone() },
            ChangeSet { regression: Some(regression(".mage/regressions/eng-1-y.spec.ts", 3)), ..base.clone() },
            ChangeSet { regression: Some(regression(".mage/regressions/eng-1-x.spec.ts", 9)), ..base.clone() },
        ];
        for v in &variants {
            assert_ne!(v.digest(), base.digest(), "{v:?}");
        }
        // The patch artifact id is not part of the digest (the bytes are, via their sha256).
        let other_artifact = ChangeSet { patch: ArtifactId::from_uuid(Uuid::from_u128(99)), ..base.clone() };
        assert_eq!(other_artifact.digest(), base.digest());
    }

    #[test]
    fn change_set_serde_round_trip() {
        let cs = change_set(Some(regression("tests/mage-eng-1-x.spec.ts", 3)));
        let json = serde_json::to_value(&cs).unwrap();
        assert_eq!(json["patch_sha256"], serde_json::json!(hex::encode([7u8; 32])));
        assert_eq!(serde_json::from_value::<ChangeSet>(json).unwrap(), cs);
        let digest = cs.digest();
        let s = serde_json::to_string(&digest).unwrap();
        assert_eq!(serde_json::from_str::<ChangeSetDigest>(&s).unwrap(), digest);
    }

    #[test]
    fn regression_paths() {
        let mage = RegressionPlacement::MageDir;
        assert_eq!(
            mage.path("ENG-412", "Null total on empty cart!", "spec.ts").unwrap().unwrap().as_str(),
            ".mage/regressions/eng-412-null-total-on-empty-cart.spec.ts"
        );
        let tests = RegressionPlacement::RepoTests { dir: RepoPath::new("e2e/regressions").unwrap() };
        assert_eq!(tests.path("ENG-412", "Crash", "sh").unwrap().unwrap().as_str(), "e2e/regressions/mage-eng-412-crash.sh");
        assert_eq!(RegressionPlacement::Off.path("ENG-412", "x", "sh").unwrap(), None);
        // Empty / symbol-only titles fall back to `repro`; long titles are capped at 48 bytes.
        assert_eq!(mage.path("ENG-1", "—!?", "sh").unwrap().unwrap().as_str(), ".mage/regressions/eng-1-repro.sh");
        let long = mage.path("ENG-1", &"word ".repeat(30), "sh").unwrap().unwrap();
        let slug = long.as_str().trim_start_matches(".mage/regressions/eng-1-").trim_end_matches(".sh");
        assert!(slug.len() <= SLUG_MAX && !slug.ends_with('-'), "{slug}");
        // Unusable identifier / extension.
        assert!(mage.path("ENG 1", "x", "sh").is_err());
        assert!(mage.path("", "x", "sh").is_err());
        assert!(mage.path("ENG-1", "x", "").is_err());
        assert!(mage.path("ENG-1", "x", "spec..ts").is_err());
        assert!(mage.path("ENG-1", "x", "../sh").is_err());
    }

    #[test]
    fn placement_resolution() {
        let dir = RepoPath::new("tests/e2e").unwrap();
        assert_eq!(RegressionPlacement::resolve(RegressionMode::Off, Some(&dir)), RegressionPlacement::Off);
        assert_eq!(RegressionPlacement::resolve(RegressionMode::MageDir, Some(&dir)), RegressionPlacement::MageDir);
        assert_eq!(
            RegressionPlacement::resolve(RegressionMode::RepoTests, Some(&dir)),
            RegressionPlacement::RepoTests { dir: dir.clone() }
        );
        assert_eq!(RegressionPlacement::resolve(RegressionMode::RepoTests, None), RegressionPlacement::MageDir);
    }
}
