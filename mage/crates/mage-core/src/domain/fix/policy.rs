//! Patch policy (fix.md "Path policy", "Server: AcceptFixResult" step 3): which paths are
//! silently excluded from the diff, which are denied, and the size limits. Pure; the runner uses
//! the exclusion side to build `DiffOptions.exclude` and runs `check` as its fast local check,
//! the server runs `check` authoritatively.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::domain::fix::change_set::RegressionContent;
use crate::domain::fix::glob::{Glob, glob_match};
use crate::domain::fix::patch::Patch;
use crate::domain::fix::settings::FixSettings;
use crate::domain::path::RepoPath;

/// What happens to lockfile changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
pub enum LockfilePolicy {
    /// Always dropped from the diff.
    Exclude,
    /// Kept only when a matching manifest in the lockfile's directory (or below it, for
    /// workspaces) also changed.
    #[default]
    WithManifest,
    /// Always kept.
    Include,
}

/// Size limits per change set (patch plus regression file). A value equal to the limit passes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ts_rs::TS)]
#[serde(default)]
pub struct PatchLimits {
    /// Raw diff bytes (plus regression file bytes).
    pub max_patch_bytes: u64,
    pub max_files: u32,
    /// Added + removed lines (regression file lines count as added).
    pub max_changed_lines: u32,
    /// Post-image bytes per file (see `FilePatch::post_image_bytes`).
    pub max_file_bytes: u64,
}

impl Default for PatchLimits {
    fn default() -> Self {
        Self { max_patch_bytes: 1 << 20, max_files: 50, max_changed_lines: 5_000, max_file_bytes: 256 << 10 }
    }
}

/// Policy carried to the runner in `FixSpec.policy`; built from the org's `FixSettings` and the
/// recipe's `[fix].generated` globs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatchPolicy {
    pub allow_workflow_edits: bool,
    pub lockfiles: LockfilePolicy,
    /// Recipe `[fix].generated` (plus any `linguist-generated` globs the runner adds).
    #[serde(default)]
    pub generated: Vec<Glob>,
    #[serde(default)]
    pub limits: PatchLimits,
}

/// Why a changed path is dropped from the diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExcludeReason {
    /// `.mage/**`: recipe changes go through Learn; the repro script lives here.
    MageDir,
    /// Built-in or recipe generated globs.
    Generated,
    /// Lockfile per `LockfilePolicy`.
    Lockfile,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Exclusion {
    pub path: RepoPath,
    pub reason: ExcludeReason,
}

/// A deny rule or limit the change set breaks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PolicyViolation {
    #[error("`{path}`: workflow and action files may not be edited")]
    WorkflowPath { path: RepoPath },
    #[error("`{path}`: paths inside `.git` may not be edited")]
    GitDir { path: RepoPath },
    #[error("patch is {bytes} bytes (limit {limit})")]
    PatchTooLarge { bytes: u64, limit: u64 },
    #[error("{files} files changed (limit {limit})")]
    TooManyFiles { files: u64, limit: u32 },
    #[error("{lines} lines changed (limit {limit})")]
    TooManyChangedLines { lines: u64, limit: u32 },
    #[error("`{path}` is {bytes} bytes after the change (limit {limit})")]
    FileTooLarge { path: RepoPath, bytes: u64, limit: u64 },
}

/// Every violation found, in patch order, then limits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PolicyViolations(pub Vec<PolicyViolation>);

impl fmt::Display for PolicyViolations {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("patch policy violated: ")?;
        for (i, v) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str("; ")?;
            }
            write!(f, "{v}")?;
        }
        Ok(())
    }
}

impl std::error::Error for PolicyViolations {}

/// Always excluded.
const MAGE_DIR: &str = ".mage/**";

/// Built-in generated paths (`dist/`, `build/`, `target/`, `.next/`, `node_modules/`,
/// `*.min.js`, `__generated__/` at any depth).
const BUILTIN_GENERATED: [&str; 7] =
    ["**/dist/**", "**/build/**", "**/target/**", "**/.next/**", "**/node_modules/**", "**/*.min.js", "**/__generated__/**"];

/// Lockfile name, its glob (for `LockfilePolicy::Exclude`), and the manifests that justify it.
const LOCKFILES: [(&str, &str, &[&str]); 21] = [
    ("package-lock.json", "**/package-lock.json", &["package.json"]),
    ("npm-shrinkwrap.json", "**/npm-shrinkwrap.json", &["package.json"]),
    ("yarn.lock", "**/yarn.lock", &["package.json"]),
    ("pnpm-lock.yaml", "**/pnpm-lock.yaml", &["package.json"]),
    ("bun.lock", "**/bun.lock", &["package.json"]),
    ("deno.lock", "**/deno.lock", &["deno.json", "deno.jsonc"]),
    ("Cargo.lock", "**/Cargo.lock", &["Cargo.toml"]),
    ("go.sum", "**/go.sum", &["go.mod"]),
    ("go.work.sum", "**/go.work.sum", &["go.work"]),
    ("Gemfile.lock", "**/Gemfile.lock", &["Gemfile"]),
    ("poetry.lock", "**/poetry.lock", &["pyproject.toml"]),
    ("uv.lock", "**/uv.lock", &["pyproject.toml"]),
    ("pdm.lock", "**/pdm.lock", &["pyproject.toml"]),
    ("Pipfile.lock", "**/Pipfile.lock", &["Pipfile"]),
    ("composer.lock", "**/composer.lock", &["composer.json"]),
    ("mix.lock", "**/mix.lock", &["mix.exs"]),
    ("pubspec.lock", "**/pubspec.lock", &["pubspec.yaml"]),
    ("Podfile.lock", "**/Podfile.lock", &["Podfile"]),
    ("Package.resolved", "**/Package.resolved", &["Package.swift"]),
    ("flake.lock", "**/flake.lock", &["flake.nix"]),
    ("gradle.lockfile", "**/gradle.lockfile", &["build.gradle", "build.gradle.kts"]),
];

fn lockfile_manifests(file_name: &str) -> Option<&'static [&'static str]> {
    LOCKFILES.iter().find(|(name, _, _)| *name == file_name).map(|(_, _, manifests)| *manifests)
}

/// `.git` as any segment (git itself refuses such paths; case-insensitive for safety).
fn is_git_dir(path: &RepoPath) -> bool {
    path.segments().any(|s| s.eq_ignore_ascii_case(".git"))
}

/// `.github/workflows/**` or `.github/actions/**` (case-insensitive for safety).
fn is_workflow(path: &RepoPath) -> bool {
    let mut segments = path.segments();
    matches!(
        (segments.next(), segments.next()),
        (Some(first), Some(second))
            if first.eq_ignore_ascii_case(".github")
                && (second.eq_ignore_ascii_case("workflows") || second.eq_ignore_ascii_case("actions"))
    )
}

fn count_lines(bytes: &[u8]) -> u64 {
    let newlines = bytes.iter().filter(|&&b| b == b'\n').count() as u64;
    newlines + u64::from(bytes.last().is_some_and(|&b| b != b'\n'))
}

impl PatchPolicy {
    pub fn from_settings(settings: &FixSettings, generated: Vec<Glob>) -> PatchPolicy {
        PatchPolicy {
            allow_workflow_edits: settings.allow_workflow_edits,
            lockfiles: settings.lockfiles,
            generated,
            limits: settings.limits,
        }
    }

    /// Deny paths, per-file and total size limits over the patch plus the regression file the
    /// change set will add. Symlinks, gitlinks, non-`±x` modes, binary stanzas, absolute and `..`
    /// paths never get here: `Patch::parse` rejects them.
    pub fn check(&self, patch: &Patch, regression: Option<&RegressionContent>) -> Result<(), PolicyViolations> {
        let mut found = Vec::new();
        let stats = patch.stats();
        let mut files = patch.files().len() as u64;
        let mut lines = u64::from(stats.added) + u64::from(stats.removed);
        let mut bytes = patch.len_bytes();
        for file in patch.files() {
            self.check_path(file.path(), &mut found);
            if file.post_image_bytes() > self.limits.max_file_bytes {
                found.push(PolicyViolation::FileTooLarge {
                    path: file.path().clone(),
                    bytes: file.post_image_bytes(),
                    limit: self.limits.max_file_bytes,
                });
            }
        }
        if let Some(reg) = regression {
            self.check_path(&reg.path, &mut found);
            let len = reg.bytes.len() as u64;
            if len > self.limits.max_file_bytes {
                found.push(PolicyViolation::FileTooLarge { path: reg.path.clone(), bytes: len, limit: self.limits.max_file_bytes });
            }
            if !patch.paths().any(|p| *p == reg.path) {
                files += 1;
            }
            lines += count_lines(&reg.bytes);
            bytes += len;
        }
        if bytes > self.limits.max_patch_bytes {
            found.push(PolicyViolation::PatchTooLarge { bytes, limit: self.limits.max_patch_bytes });
        }
        if files > u64::from(self.limits.max_files) {
            found.push(PolicyViolation::TooManyFiles { files, limit: self.limits.max_files });
        }
        if lines > u64::from(self.limits.max_changed_lines) {
            found.push(PolicyViolation::TooManyChangedLines { lines, limit: self.limits.max_changed_lines });
        }
        if found.is_empty() { Ok(()) } else { Err(PolicyViolations(found)) }
    }

    fn check_path(&self, path: &RepoPath, found: &mut Vec<PolicyViolation>) {
        if is_git_dir(path) {
            found.push(PolicyViolation::GitDir { path: path.clone() });
        } else if is_workflow(path) && !self.allow_workflow_edits {
            found.push(PolicyViolation::WorkflowPath { path: path.clone() });
        }
    }

    /// Exclusions that do not depend on the rest of the change: `.mage/**`, built-in and recipe
    /// generated globs, and lockfiles under `LockfilePolicy::Exclude`. `WithManifest` lockfiles
    /// need the changed path list: see `exclusion`.
    pub fn exclude_globs(&self) -> Vec<&str> {
        let mut globs = Vec::with_capacity(1 + BUILTIN_GENERATED.len() + self.generated.len() + LOCKFILES.len());
        globs.push(MAGE_DIR);
        globs.extend(BUILTIN_GENERATED);
        globs.extend(self.generated.iter().map(Glob::as_str));
        if self.lockfiles == LockfilePolicy::Exclude {
            globs.extend(LOCKFILES.iter().map(|(_, glob, _)| *glob));
        }
        globs
    }

    /// Why `path` is excluded, given every path the agent changed (`changed`, before exclusion).
    pub fn exclusion(&self, path: &RepoPath, changed: &[RepoPath]) -> Option<ExcludeReason> {
        let p = path.as_str();
        if glob_match(MAGE_DIR, p) {
            return Some(ExcludeReason::MageDir);
        }
        if BUILTIN_GENERATED.iter().any(|g| glob_match(g, p)) || self.generated.iter().any(|g| g.matches(p)) {
            return Some(ExcludeReason::Generated);
        }
        let manifests = lockfile_manifests(path.file_name())?;
        let keep = match self.lockfiles {
            LockfilePolicy::Include => true,
            LockfilePolicy::Exclude => false,
            LockfilePolicy::WithManifest => {
                changed.iter().any(|c| manifests.contains(&c.file_name()) && path.parent().is_none_or(|dir| c.is_within(dir)))
            }
        };
        (!keep).then_some(ExcludeReason::Lockfile)
    }

    /// `exclusion` for every changed path; the runner passes these paths as `exclude`.
    pub fn exclusions(&self, changed: &[RepoPath]) -> Vec<Exclusion> {
        changed.iter().filter_map(|path| self.exclusion(path, changed).map(|reason| Exclusion { path: path.clone(), reason })).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "1111111111111111111111111111111111111111";
    const B: &str = "2222222222222222222222222222222222222222";
    const Z: &str = "0000000000000000000000000000000000000000";

    fn path(p: &str) -> RepoPath {
        RepoPath::new(p).unwrap()
    }

    fn modify(p: &str) -> String {
        format!("diff --git a/{p} b/{p}\nindex {A}..{B} 100644\n--- a/{p}\n+++ b/{p}\n@@ -1 +1 @@\n-a\n+b\n")
    }

    /// New file at `p` with `n` lines of `width` bytes each (newline included).
    fn add(p: &str, n: usize, width: usize) -> String {
        let line = format!("+{}\n", "x".repeat(width - 1));
        format!(
            "diff --git a/{p} b/{p}\nnew file mode 100644\nindex {Z}..{A}\n--- /dev/null\n+++ b/{p}\n@@ -0,0 +1,{n} @@\n{}",
            line.repeat(n)
        )
    }

    fn policy() -> PatchPolicy {
        PatchPolicy::from_settings(&FixSettings::default(), vec![Glob::new("src/gen/**").unwrap()])
    }

    fn check(policy: &PatchPolicy, patch: &str) -> Result<(), PolicyViolations> {
        policy.check(&Patch::parse(patch.as_bytes()).unwrap(), None)
    }

    #[test]
    fn deny_table_per_path_class() {
        let allowed = PatchPolicy { allow_workflow_edits: true, ..policy() };
        // (path, denied by default, denied with allow_workflow_edits)
        let cases: &[(&str, Option<&str>, Option<&str>)] = &[
            ("src/app.ts", None, None),
            (".github/CODEOWNERS", None, None),
            (".github/workflows/ci.yml", Some("workflow"), None),
            (".github/actions/setup/action.yml", Some("workflow"), None),
            (".GitHub/Workflows/ci.yml", Some("workflow"), None),
            ("docs/.github/workflows/ci.yml", None, None),
            (".git/config", Some("git"), Some("git")),
            ("vendor/lib/.git/HEAD", Some("git"), Some("git")),
            (".GIT/hooks/pre-commit", Some("git"), Some("git")),
            (".gitignore", None, None),
        ];
        let class = |r: Result<(), PolicyViolations>| -> Option<&'static str> {
            match r {
                Ok(()) => None,
                Err(PolicyViolations(v)) => match v.as_slice() {
                    [PolicyViolation::WorkflowPath { .. }] => Some("workflow"),
                    [PolicyViolation::GitDir { .. }] => Some("git"),
                    other => panic!("unexpected {other:?}"),
                },
            }
        };
        for &(p, default, with_allow) in cases {
            assert_eq!(class(check(&policy(), &modify(p))), default, "{p}");
            assert_eq!(class(check(&allowed, &modify(p))), with_allow, "{p} (allowed)");
        }
    }

    #[test]
    fn file_count_limit() {
        let tight = PatchPolicy { limits: PatchLimits { max_files: 3, ..PatchLimits::default() }, ..policy() };
        let three: String = (0..3).map(|i| modify(&format!("f{i}"))).collect();
        assert_eq!(check(&tight, &three), Ok(()));
        let four: String = (0..4).map(|i| modify(&format!("f{i}"))).collect();
        assert_eq!(check(&tight, &four), Err(PolicyViolations(vec![PolicyViolation::TooManyFiles { files: 4, limit: 3 }])));
        // The regression file counts as one more file unless the patch already touches its path.
        let patch = Patch::parse(three.as_bytes()).unwrap();
        let reg = RegressionContent { path: path(".mage/regressions/eng-1-x.sh"), bytes: b"exit 0\n".to_vec() };
        assert_eq!(tight.check(&patch, Some(&reg)), Err(PolicyViolations(vec![PolicyViolation::TooManyFiles { files: 4, limit: 3 }])));
        let same_path = RegressionContent { path: path("f0"), bytes: b"exit 0\n".to_vec() };
        assert_eq!(tight.check(&patch, Some(&same_path)), Ok(()));
    }

    #[test]
    fn changed_lines_limit() {
        let tight = PatchPolicy { limits: PatchLimits { max_changed_lines: 10, ..PatchLimits::default() }, ..policy() };
        assert_eq!(check(&tight, &add("a", 10, 4)), Ok(()));
        assert_eq!(
            check(&tight, &add("a", 11, 4)),
            Err(PolicyViolations(vec![PolicyViolation::TooManyChangedLines { lines: 11, limit: 10 }]))
        );
        // Added + removed: a one-line modify is 2 changed lines.
        let two = PatchPolicy { limits: PatchLimits { max_changed_lines: 2, ..PatchLimits::default() }, ..policy() };
        assert_eq!(check(&two, &modify("f")), Ok(()));
        let one = PatchPolicy { limits: PatchLimits { max_changed_lines: 1, ..PatchLimits::default() }, ..policy() };
        assert!(check(&one, &modify("f")).is_err());
        // Regression lines count (last line without newline included).
        let patch = Patch::parse(modify("f").as_bytes()).unwrap();
        let reg = RegressionContent { path: path("t/x.sh"), bytes: b"a\nb".to_vec() };
        let four = PatchPolicy { limits: PatchLimits { max_changed_lines: 4, ..PatchLimits::default() }, ..policy() };
        assert_eq!(four.check(&patch, Some(&reg)), Ok(()));
        let three = PatchPolicy { limits: PatchLimits { max_changed_lines: 3, ..PatchLimits::default() }, ..policy() };
        assert!(three.check(&patch, Some(&reg)).is_err());
    }

    #[test]
    fn patch_bytes_limit() {
        let input = modify("f");
        let len = input.len() as u64;
        let at = PatchPolicy { limits: PatchLimits { max_patch_bytes: len, ..PatchLimits::default() }, ..policy() };
        assert_eq!(check(&at, &input), Ok(()));
        let below = PatchPolicy { limits: PatchLimits { max_patch_bytes: len - 1, ..PatchLimits::default() }, ..policy() };
        assert_eq!(check(&below, &input), Err(PolicyViolations(vec![PolicyViolation::PatchTooLarge { bytes: len, limit: len - 1 }])));
    }

    #[test]
    fn per_file_post_image_limit() {
        // 4 lines of 8 bytes = 32 bytes post-image.
        let at = PatchPolicy { limits: PatchLimits { max_file_bytes: 32, ..PatchLimits::default() }, ..policy() };
        assert_eq!(check(&at, &add("big.txt", 4, 8)), Ok(()));
        let below = PatchPolicy { limits: PatchLimits { max_file_bytes: 31, ..PatchLimits::default() }, ..policy() };
        assert_eq!(
            check(&below, &add("big.txt", 4, 8)),
            Err(PolicyViolations(vec![PolicyViolation::FileTooLarge { path: path("big.txt"), bytes: 32, limit: 31 }]))
        );
        // The regression file is held to the same limit.
        let patch = Patch::parse(modify("f").as_bytes()).unwrap();
        let reg = RegressionContent { path: path("t/x.sh"), bytes: vec![b'x'; 32] };
        assert_eq!(at.check(&patch, Some(&reg)), Ok(()));
        assert_eq!(
            below.check(&patch, Some(&reg)),
            Err(PolicyViolations(vec![PolicyViolation::FileTooLarge { path: path("t/x.sh"), bytes: 32, limit: 31 }]))
        );
    }

    #[test]
    fn default_limits_at_boundary() {
        let p = policy();
        // 50 files pass, 51 fail.
        let fifty: String = (0..50).map(|i| modify(&format!("f{i}"))).collect();
        assert_eq!(check(&p, &fifty), Ok(()));
        let fifty_one: String = (0..51).map(|i| modify(&format!("f{i}"))).collect();
        assert!(
            matches!(check(&p, &fifty_one), Err(PolicyViolations(v)) if v == vec![PolicyViolation::TooManyFiles { files: 51, limit: 50 }])
        );
        // 5 000 changed lines pass, 5 001 fail.
        assert_eq!(check(&p, &add("a", 5_000, 2)), Ok(()));
        assert!(check(&p, &add("a", 5_001, 2)).is_err());
        // 256 KiB post-image passes, one byte more fails (1024 lines of 256 bytes).
        assert_eq!(check(&p, &add("a", 1024, 256)), Ok(()));
        let over = format!("{}+\n", add("a", 1024, 256)).replace("+1,1024 @@", "+1,1025 @@");
        let err = check(&p, &over).unwrap_err();
        assert!(err.0.contains(&PolicyViolation::FileTooLarge { path: path("a"), bytes: (256 << 10) + 1, limit: 256 << 10 }));
    }

    #[test]
    fn all_violations_are_reported() {
        let tight = PatchPolicy { limits: PatchLimits { max_files: 1, ..PatchLimits::default() }, ..policy() };
        let input = format!("{}{}", modify(".github/workflows/ci.yml"), modify(".git/config"));
        assert_eq!(
            check(&tight, &input),
            Err(PolicyViolations(vec![
                PolicyViolation::WorkflowPath { path: path(".github/workflows/ci.yml") },
                PolicyViolation::GitDir { path: path(".git/config") },
                PolicyViolation::TooManyFiles { files: 2, limit: 1 },
            ]))
        );
    }

    #[test]
    fn exclusion_classification() {
        let p = policy();
        let changed = |paths: &[&str]| paths.iter().map(|s| path(s)).collect::<Vec<_>>();
        let cases: &[(&str, &[&str], Option<ExcludeReason>)] = &[
            ("src/app.ts", &[], None),
            (".mage/env.toml", &[], Some(ExcludeReason::MageDir)),
            (".mage/repro/c1/repro.spec.ts", &[], Some(ExcludeReason::MageDir)),
            ("web/dist/app.js", &[], Some(ExcludeReason::Generated)),
            ("dist/index.html", &[], Some(ExcludeReason::Generated)),
            ("target/debug/x", &[], Some(ExcludeReason::Generated)),
            ("node_modules/a/index.js", &[], Some(ExcludeReason::Generated)),
            ("app/.next/cache/x", &[], Some(ExcludeReason::Generated)),
            ("public/jquery.min.js", &[], Some(ExcludeReason::Generated)),
            ("src/__generated__/types.ts", &[], Some(ExcludeReason::Generated)),
            ("src/gen/api.ts", &[], Some(ExcludeReason::Generated)),
            ("src/generator.ts", &[], None),
            // Lockfile with its sibling manifest: kept.
            ("package-lock.json", &["package.json"], None),
            ("api/Cargo.lock", &["api/Cargo.toml"], None),
            // Workspace: root lockfile with a member manifest below it: kept.
            ("Cargo.lock", &["crates/core/Cargo.toml"], None),
            ("pnpm-lock.yaml", &["packages/ui/package.json"], None),
            // No matching manifest: excluded.
            ("package-lock.json", &[], Some(ExcludeReason::Lockfile)),
            ("package-lock.json", &["Cargo.toml"], Some(ExcludeReason::Lockfile)),
            // Manifest in another directory (not below the lockfile): excluded.
            ("api/go.sum", &["web/go.mod"], Some(ExcludeReason::Lockfile)),
            ("api/go.sum", &["go.mod"], Some(ExcludeReason::Lockfile)),
            ("poetry.lock", &["pyproject.toml"], None),
            // `.mage/` wins over every other class; generated wins over lockfile.
            (".mage/package-lock.json", &[".mage/package.json"], Some(ExcludeReason::MageDir)),
            ("node_modules/x/yarn.lock", &["node_modules/x/package.json"], Some(ExcludeReason::Generated)),
        ];
        for &(p_str, others, want) in cases {
            let mut all = changed(others);
            all.push(path(p_str));
            assert_eq!(p.exclusion(&path(p_str), &all), want, "{p_str} with {others:?}");
        }
    }

    #[test]
    fn lockfile_policy_variants() {
        let changed = vec![path("yarn.lock"), path("src/a.ts")];
        let with = |lockfiles| PatchPolicy { lockfiles, ..policy() };
        assert_eq!(
            with(LockfilePolicy::WithManifest).exclusions(&changed),
            vec![Exclusion { path: path("yarn.lock"), reason: ExcludeReason::Lockfile }]
        );
        assert_eq!(
            with(LockfilePolicy::Exclude).exclusions(&changed),
            vec![Exclusion { path: path("yarn.lock"), reason: ExcludeReason::Lockfile }]
        );
        assert_eq!(with(LockfilePolicy::Include).exclusions(&changed), vec![]);
        // With the manifest changed only `Exclude` drops it.
        let changed = vec![path("yarn.lock"), path("package.json")];
        assert_eq!(with(LockfilePolicy::WithManifest).exclusions(&changed), vec![]);
        assert_eq!(with(LockfilePolicy::Exclude).exclusions(&changed).len(), 1);
    }

    #[test]
    fn exclude_globs_match_the_classification() {
        let p = PatchPolicy { lockfiles: LockfilePolicy::Exclude, ..policy() };
        let globs = p.exclude_globs();
        for g in &globs {
            assert!(Glob::new(*g).is_ok(), "{g}");
        }
        let matched = |s: &str| globs.iter().any(|g| glob_match(g, s));
        for s in [".mage/env.toml", "a/dist/x.js", "src/gen/x.ts", "sub/yarn.lock", "Cargo.lock"] {
            assert!(matched(s), "{s}");
        }
        assert!(!matched("src/app.ts"));
        // `WithManifest` lockfiles depend on the change set, so they are not static globs.
        assert!(!policy().exclude_globs().iter().any(|g| glob_match(g, "yarn.lock")));
    }
}
