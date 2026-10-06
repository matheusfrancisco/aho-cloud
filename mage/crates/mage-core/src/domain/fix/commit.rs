//! `CommitPlan` (fix.md Additions, "Use case OpenPullRequest" step 3): the only input
//! `PullRequests::create_commit` accepts. Its sole constructor takes a `FixGatePassed` and
//! re-checks the change-set digest of what is about to be committed, so an ungated or swapped
//! patch cannot reach GitHub.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

use serde::{Deserialize, Serialize};

use crate::domain::error::DomainError;
use crate::domain::fix::change_set::{ChangeSetDigest, RegressionContent};
use crate::domain::fix::patch::{ApplyMode, FileMode, Patch, PatchChange};
use crate::domain::gates::FixGatePassed;
use crate::domain::ids::CommitSha;
use crate::domain::path::RepoPath;

/// Content and mode of a path at the commit parent (`PullRequests::read_files`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BaseFile {
    Present { path: RepoPath, mode: FileMode, bytes: Vec<u8> },
    Absent { path: RepoPath },
}

impl BaseFile {
    pub fn path(&self) -> &RepoPath {
        match self {
            BaseFile::Present { path, .. } | BaseFile::Absent { path } => path,
        }
    }
}

/// What the commit does to one path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitChange {
    Write { mode: FileMode, bytes: Vec<u8> },
    Delete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitFile {
    pub path: RepoPath,
    pub change: CommitChange,
}

/// Post-images of a gated change set on top of its base commit. Not deserializable: the only
/// way to obtain one is `CommitPlan::build`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitPlan {
    parent: CommitSha,
    files: Vec<CommitFile>,
    message: String,
}

impl CommitPlan {
    /// Requires `digest(gate.base_sha(), patch.sha256(), regression path + sha256(bytes))` to
    /// equal `gate.digest()`, then applies `patch` to `base_files` (`ApplyMode::AllowOffset`
    /// semantics: offsets allowed, zero fuzz) and writes the regression file last, as the gate
    /// did. `base_files` must cover every path the patch touches. Files come out sorted by path.
    pub fn build(
        gate: &FixGatePassed,
        base_files: Vec<BaseFile>,
        patch: &Patch,
        regression: Option<RegressionContent>,
        message: String,
    ) -> Result<CommitPlan, DomainError> {
        let parent = gate.base_sha();
        check_digest(gate.digest(), &parent, patch, regression.as_ref())?;
        if message.trim().is_empty() {
            return Err(DomainError::invalid("commit message is empty"));
        }

        let mut base: BTreeMap<RepoPath, Option<(FileMode, Vec<u8>)>> = BTreeMap::new();
        for file in base_files {
            let (path, content) = match file {
                BaseFile::Present { path, mode, bytes } => (path, Some((mode, bytes))),
                BaseFile::Absent { path } => (path, None),
            };
            match base.entry(path) {
                Entry::Vacant(slot) => {
                    slot.insert(content);
                }
                Entry::Occupied(slot) => {
                    return Err(DomainError::invalid(format!("base file {} given twice", slot.key())));
                }
            }
        }

        let mut out: BTreeMap<RepoPath, CommitChange> = BTreeMap::new();
        for file in patch.files() {
            let path = file.path();
            let Some(current) = base.remove(path) else {
                return Err(DomainError::invalid(format!("no base file for {path}")));
            };
            let change = match (file.change(), current) {
                // Pure `±x`: same bytes, new mode; no copy.
                (PatchChange::ModeOnly, Some((_, bytes))) => {
                    let Some(mode) = file.new_mode() else {
                        return Err(DomainError::invalid(format!("{path}: mode change without a new mode")));
                    };
                    CommitChange::Write { mode, bytes }
                }
                (_, current) => {
                    let applied = file
                        .apply(current.as_ref().map(|(_, bytes)| bytes.as_slice()), ApplyMode::AllowOffset)
                        .map_err(|e| DomainError::invalid(e.to_string()))?;
                    match applied {
                        None => CommitChange::Delete,
                        Some(bytes) => {
                            // Base mode is preserved unless the patch changes it; new files take
                            // the patch's mode.
                            let mode = match (file.new_mode(), &current) {
                                (Some(new), Some((base_mode, _))) if file.old_mode() == Some(new) => *base_mode,
                                (Some(new), _) => new,
                                (None, _) => {
                                    return Err(DomainError::invalid(format!("{path}: no mode for the post-image")));
                                }
                            };
                            CommitChange::Write { mode, bytes }
                        }
                    }
                }
            };
            out.insert(path.clone(), change);
        }
        if let Some(reg) = regression {
            out.insert(reg.path, CommitChange::Write { mode: FileMode::Regular, bytes: reg.bytes });
        }
        Ok(CommitPlan { parent, files: out.into_iter().map(|(path, change)| CommitFile { path, change }).collect(), message })
    }

    /// Commit parent: the base sha the gate verified.
    pub fn parent(&self) -> CommitSha {
        self.parent
    }

    pub fn files(&self) -> &[CommitFile] {
        &self.files
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

fn check_digest(
    expected: ChangeSetDigest,
    base_sha: &CommitSha,
    patch: &Patch,
    regression: Option<&RegressionContent>,
) -> Result<(), DomainError> {
    let script_sha256 = regression.map(RegressionContent::sha256);
    let actual = ChangeSetDigest::compute(base_sha, &patch.sha256(), regression.zip(script_sha256.as_ref()).map(|(r, sha)| (&r.path, sha)));
    if actual == expected {
        Ok(())
    } else {
        Err(DomainError::invalid(format!("change set digest mismatch: gated {expected}, about to commit {actual}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::fix::change_set::{ChangeSet, RegressionFile};
    use crate::domain::gates::testing::fix_gate_passed;
    use crate::domain::ids::{ArtifactId, CauseId, GateId, RunId};
    use uuid::Uuid;

    const A: &str = "1111111111111111111111111111111111111111";
    const B: &str = "2222222222222222222222222222222222222222";
    const Z: &str = "0000000000000000000000000000000000000000";
    const BASE: CommitSha = CommitSha([4; 20]);

    fn path(p: &str) -> RepoPath {
        RepoPath::new(p).unwrap()
    }

    /// Modify `src/app.ts`, add `src/new.ts`, delete `old.txt`, `+x` on `run.sh`.
    fn patch_bytes() -> String {
        format!(
            "diff --git a/old.txt b/old.txt\ndeleted file mode 100644\nindex {A}..{Z}\n--- a/old.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-bye\n\
             diff --git a/run.sh b/run.sh\nold mode 100644\nnew mode 100755\n\
             diff --git a/src/app.ts b/src/app.ts\nindex {A}..{B} 100644\n--- a/src/app.ts\n+++ b/src/app.ts\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n\
             diff --git a/src/new.ts b/src/new.ts\nnew file mode 100755\nindex {Z}..{A}\n--- /dev/null\n+++ b/src/new.ts\n@@ -0,0 +1 @@\n+new\n"
        )
    }

    fn base_files() -> Vec<BaseFile> {
        vec![
            BaseFile::Present { path: path("src/app.ts"), mode: FileMode::Executable, bytes: b"a\nb\nc\n".to_vec() },
            BaseFile::Absent { path: path("src/new.ts") },
            BaseFile::Present { path: path("old.txt"), mode: FileMode::Regular, bytes: b"bye\n".to_vec() },
            BaseFile::Present { path: path("run.sh"), mode: FileMode::Regular, bytes: b"echo\n".to_vec() },
        ]
    }

    fn regression() -> RegressionContent {
        RegressionContent { path: path(".mage/regressions/eng-1-x.sh"), bytes: b"exit 0\n".to_vec() }
    }

    fn token(base_sha: CommitSha, patch_sha256: [u8; 32], regression: Option<&RegressionContent>) -> FixGatePassed {
        let change_set = ChangeSet {
            base_sha,
            patch: ArtifactId::from_uuid(Uuid::from_u128(10)),
            patch_sha256,
            regression: regression.map(|r| RegressionFile {
                path: r.path.clone(),
                script: ArtifactId::from_uuid(Uuid::from_u128(11)),
                script_sha256: r.sha256(),
            }),
        };
        fix_gate_passed(
            GateId::from_uuid(Uuid::from_u128(1)),
            RunId::from_uuid(Uuid::from_u128(2)),
            CauseId::from_uuid(Uuid::from_u128(3)),
            change_set,
        )
    }

    fn build(gate: &FixGatePassed, patch: &Patch, regression: Option<RegressionContent>) -> Result<CommitPlan, DomainError> {
        CommitPlan::build(gate, base_files(), patch, regression, "fix: thing (ENG-1)".into())
    }

    #[test]
    fn builds_post_images_for_a_matching_digest() {
        let patch = Patch::parse(patch_bytes().as_bytes()).unwrap();
        let gate = token(BASE, patch.sha256(), Some(&regression()));
        let plan = build(&gate, &patch, Some(regression())).unwrap();
        assert_eq!(plan.parent(), BASE);
        assert_eq!(plan.message(), "fix: thing (ENG-1)");
        let write = |mode, bytes: &[u8]| CommitChange::Write { mode, bytes: bytes.to_vec() };
        assert_eq!(
            plan.files(),
            &[
                CommitFile { path: path(".mage/regressions/eng-1-x.sh"), change: write(FileMode::Regular, b"exit 0\n") },
                CommitFile { path: path("old.txt"), change: CommitChange::Delete },
                CommitFile { path: path("run.sh"), change: write(FileMode::Executable, b"echo\n") },
                // Base mode (executable) preserved: the patch does not change it.
                CommitFile { path: path("src/app.ts"), change: write(FileMode::Executable, b"a\nB\nc\n") },
                CommitFile { path: path("src/new.ts"), change: write(FileMode::Executable, b"new\n") },
            ]
        );
    }

    #[test]
    fn applies_with_offsets_like_the_source_applicability_check() {
        let input = format!("diff --git a/f b/f\nindex {A}..{B} 100644\n--- a/f\n+++ b/f\n@@ -3,3 +3,3 @@\n c\n-d\n+D\n e\n");
        let patch = Patch::parse(input.as_bytes()).unwrap();
        let gate = token(BASE, patch.sha256(), None);
        let base = vec![BaseFile::Present { path: path("f"), mode: FileMode::Regular, bytes: b"x\na\nb\nc\nd\ne\nf\n".to_vec() }];
        let plan = CommitPlan::build(&gate, base, &patch, None, "m".into()).unwrap();
        assert_eq!(
            plan.files(),
            &[CommitFile {
                path: path("f"),
                change: CommitChange::Write { mode: FileMode::Regular, bytes: b"x\na\nb\nc\nD\ne\nf\n".to_vec() }
            }]
        );
    }

    #[test]
    fn rejects_digest_mismatch() {
        let patch = Patch::parse(patch_bytes().as_bytes()).unwrap();
        let mismatch = |r: Result<CommitPlan, DomainError>| {
            assert!(matches!(&r, Err(DomainError::Invalid(m)) if m.starts_with("change set digest mismatch")), "{r:?}");
        };
        // Patch bytes differ from the gated ones.
        let other = Patch::parse(patch_bytes().replace("+B\n", "+X\n").as_bytes()).unwrap();
        let gate = token(BASE, other.sha256(), Some(&regression()));
        mismatch(build(&gate, &patch, Some(regression())));
        // Regression script bytes differ.
        let gate = token(BASE, patch.sha256(), Some(&regression()));
        let tampered = RegressionContent { bytes: b"exit 1\n".to_vec(), ..regression() };
        mismatch(build(&gate, &patch, Some(tampered)));
        // Regression path differs.
        let moved = RegressionContent { path: path("tests/mage-eng-1-x.sh"), ..regression() };
        mismatch(build(&gate, &patch, Some(moved)));
        // Regression dropped, or added when the gate ran without one.
        mismatch(build(&gate, &patch, None));
        let gate_without = token(BASE, patch.sha256(), None);
        mismatch(build(&gate_without, &patch, Some(regression())));
        // Base differs from the gated one.
        let digest = token(BASE, patch.sha256(), None).digest();
        assert!(check_digest(digest, &BASE, &patch, None).is_ok());
        assert!(check_digest(digest, &CommitSha([5; 20]), &patch, None).is_err());
    }

    #[test]
    fn rejects_incomplete_or_mismatched_base_files() {
        let patch = Patch::parse(patch_bytes().as_bytes()).unwrap();
        let gate = token(BASE, patch.sha256(), None);
        // A touched path without a base entry.
        let mut missing = base_files();
        missing.retain(|f| f.path().as_str() != "src/app.ts");
        assert!(CommitPlan::build(&gate, missing, &patch, None, "m".into()).is_err());
        // The same path twice.
        let mut twice = base_files();
        twice.push(BaseFile::Absent { path: path("src/app.ts") });
        assert!(CommitPlan::build(&gate, twice, &patch, None, "m".into()).is_err());
        // Content no longer matches the hunk context.
        let mut changed = base_files();
        changed[0] = BaseFile::Present { path: path("src/app.ts"), mode: FileMode::Regular, bytes: b"a\nq\nc\n".to_vec() };
        assert!(CommitPlan::build(&gate, changed, &patch, None, "m".into()).is_err());
        // The added file already exists.
        let mut exists = base_files();
        exists[1] = BaseFile::Present { path: path("src/new.ts"), mode: FileMode::Regular, bytes: vec![] };
        assert!(CommitPlan::build(&gate, exists, &patch, None, "m".into()).is_err());
        // Empty message.
        assert!(CommitPlan::build(&gate, base_files(), &patch, None, "  \n".into()).is_err());
    }
}
