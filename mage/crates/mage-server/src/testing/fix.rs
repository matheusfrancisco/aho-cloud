//! Fix-path test helpers: unified diffs in the strict `diff_against_base` subset, and stored
//! results that pass `gates::fix`, so tests obtain a real `FixGatePassed` (tokens have no public
//! constructor) and from it a real `CommitPlan`.

use mage_core::domain::fix::{ApplyMode, BaseFile, ChangeSet, CommitPlan, Patch};
use mage_core::domain::gates::{self, FixGate};
use mage_core::domain::{
    AttemptKind, CauseId, CommitSha, FailureSignature, FixChecks, FixGatePassed, FixResult, FixStats, GateId, Matcher, RepoPath, RunId,
    SignatureKind, VerifyAttempt, VerifyOutcome, VerifyResult, VerifySpec,
};

const OLD_OID: &str = "1111111111111111111111111111111111111111";
const NEW_OID: &str = "2222222222222222222222222222222222222222";
const ZERO_OID: &str = "0000000000000000000000000000000000000000";

/// `(path, before, after)` of [`replace_patch`]; `None` = absent.
pub type FileChange<'a> = (&'a str, Option<&'a [u8]>, Option<&'a [u8]>);

/// A diff that replaces each file wholesale: `(path, before, after)`, `None` = absent. Contents
/// must be empty or end with `\n`.
pub fn replace_patch(changes: &[FileChange<'_>]) -> Vec<u8> {
    let mut out = String::new();
    for (path, before, after) in changes {
        let lines = |b: Option<&[u8]>| -> Vec<String> {
            b.map(|b| String::from_utf8_lossy(b).split_inclusive('\n').map(str::to_owned).collect()).unwrap_or_default()
        };
        let (old, new) = (lines(*before), lines(*after));
        out.push_str(&format!("diff --git a/{path} b/{path}\n"));
        match (before, after) {
            (None, Some(_)) => out.push_str(&format!("new file mode 100644\nindex {ZERO_OID}..{NEW_OID}\n--- /dev/null\n+++ b/{path}\n")),
            (Some(_), None) => {
                out.push_str(&format!("deleted file mode 100644\nindex {OLD_OID}..{ZERO_OID}\n--- a/{path}\n+++ /dev/null\n"))
            }
            _ => out.push_str(&format!("index {OLD_OID}..{NEW_OID} 100644\n--- a/{path}\n+++ b/{path}\n")),
        }
        let range = |start_if_empty: usize, n: usize| if n == 0 { format!("{start_if_empty},0") } else { format!("1,{n}") };
        out.push_str(&format!("@@ -{} +{} @@\n", range(0, old.len()), range(0, new.len())));
        for l in &old {
            out.push('-');
            out.push_str(l);
        }
        for l in &new {
            out.push('+');
            out.push_str(l);
        }
    }
    out.into_bytes()
}

/// The repro signature the helpers' gates use.
pub fn signature() -> FailureSignature {
    FailureSignature { kind: SignatureKind::ErrorString, value: "TypeError: total is undefined".into(), matcher: Matcher::Contains }
}

/// Stored results of a Fix gate that passes for `change_set` (`Exact` apply, one repetition).
pub fn passing_fix_results(change_set: &ChangeSet, script: mage_core::domain::ArtifactId) -> (VerifySpec, FixResult, VerifyResult) {
    let spec = VerifySpec::Fix {
        patch: change_set.patch,
        script,
        signature: signature(),
        repetitions: 1,
        regression: change_set.regression.clone(),
        mode: ApplyMode::Exact,
        upstream_check: false,
    };
    let fix = FixResult { patch: change_set.patch, summary: "Fixed it.".into(), excluded: Vec::new(), stats: FixStats::default() };
    let verify = VerifyResult {
        outcome: VerifyOutcome::Completed,
        observed: None,
        attempts: vec![VerifyAttempt {
            index: 1,
            kind: AttemptKind::Script,
            script_exit: Some(0),
            script_passed: true,
            status: None,
            observed: Vec::new(),
            health_ok: true,
            duration_ms: 1200,
        }],
        artifacts: Vec::new(),
        fix_checks: Some(FixChecks {
            applied_patch_sha256: Some(change_set.patch_sha256),
            boot: true,
            test_exit: Some(0),
            lint_exit: None,
            source_applies: Some(true),
            upstream_already_fixed: None,
        }),
        rebased_patch: None,
    };
    (spec, fix, verify)
}

/// A real `FixGatePassed` for `change_set`.
pub fn mint_fix_gate(gate: GateId, fix_run: RunId, cause: CauseId, change_set: &ChangeSet) -> FixGatePassed {
    let (spec, fix, verify) = passing_fix_results(change_set, change_set.patch);
    gates::fix(&FixGate { gate, fix_run, cause, change_set, spec: &spec, lint_required: false }, &fix, &verify)
        .unwrap_or_else(|e| panic!("helper gate must pass: {e}"))
}

/// A gated `CommitPlan` on `parent` writing `writes` and deleting `deletes`, given the parent's
/// current contents of those paths (`base(path)`).
pub fn gated_plan(
    parent: CommitSha,
    base: &dyn Fn(&str) -> Option<Vec<u8>>,
    writes: &[(&str, &[u8])],
    deletes: &[&str],
    message: &str,
) -> CommitPlan {
    let befores: Vec<(String, Option<Vec<u8>>)> =
        writes.iter().map(|(p, _)| *p).chain(deletes.iter().copied()).map(|p| (p.to_owned(), base(p))).collect();
    let mut changes: Vec<FileChange<'_>> = Vec::new();
    for (i, (path, after)) in writes.iter().enumerate() {
        changes.push((path, befores[i].1.as_deref(), Some(after)));
    }
    for (j, path) in deletes.iter().enumerate() {
        changes.push((path, befores[writes.len() + j].1.as_deref(), None));
    }
    let raw = replace_patch(&changes);
    let patch = Patch::parse(&raw).unwrap_or_else(|e| panic!("helper patch: {e}"));
    let change_set = ChangeSet {
        base_sha: parent,
        patch: mage_core::domain::ArtifactId::from_uuid(uuid::Uuid::from_u128(0xfeed)),
        patch_sha256: patch.sha256(),
        regression: None,
    };
    let token = mint_fix_gate(
        GateId::from_uuid(uuid::Uuid::from_u128(1)),
        RunId::from_uuid(uuid::Uuid::from_u128(2)),
        CauseId::from_uuid(uuid::Uuid::from_u128(3)),
        &change_set,
    );
    let base_files: Vec<BaseFile> = befores
        .into_iter()
        .map(|(path, bytes)| {
            let path = RepoPath::new(path).unwrap_or_else(|e| panic!("{e}"));
            match bytes {
                Some(bytes) => BaseFile::Present { path, mode: mage_core::domain::fix::FileMode::Regular, bytes },
                None => BaseFile::Absent { path },
            }
        })
        .collect();
    CommitPlan::build(&token, base_files, &patch, None, message.to_owned()).unwrap_or_else(|e| panic!("helper plan: {e}"))
}

/// A connected repo `owner/name` (org 1, installation 7, default branch `main`).
pub fn test_repo(owner: &str, name: &str) -> mage_core::domain::Repo {
    mage_core::domain::Repo {
        id: mage_core::domain::RepoId::from_uuid(uuid::Uuid::from_u128(0x5e90)),
        org_id: mage_core::domain::OrgId::from_uuid(uuid::Uuid::from_u128(1)),
        external_id: 4242,
        owner: owner.into(),
        name: name.into(),
        default_branch: "main".into(),
        installation_id: mage_core::domain::InstallationId(7),
        access: mage_core::domain::RepoAccess::Granted,
        created_at: time::OffsetDateTime::UNIX_EPOCH,
    }
}

#[cfg(test)]
mod tests {
    use mage_core::domain::OrgId;

    use super::*;
    use crate::testing::pull_requests::{FakePullRequests, GitHubWorld, PullRequestsFixture, contract_pull_requests};

    #[test]
    fn replace_patch_round_trips_through_the_domain_parser() {
        let raw = replace_patch(&[("a.txt", Some(b"x\ny\n"), Some(b"z\n")), ("new.txt", None, Some(b"n\n")), ("gone.txt", Some(b"g\n"), None)]);
        let patch = Patch::parse(&raw).unwrap();
        let out: Vec<_> = patch
            .files()
            .iter()
            .map(|f| {
                let base: Option<&[u8]> = match f.path().as_str() {
                    "a.txt" => Some(b"x\ny\n"),
                    "gone.txt" => Some(b"g\n"),
                    _ => None,
                };
                f.apply(base, ApplyMode::Exact).unwrap()
            })
            .collect();
        assert_eq!(out, vec![Some(b"z\n".to_vec()), Some(b"n\n".to_vec()), None]);
    }

    #[tokio::test]
    async fn fake_pull_requests_pass_the_contract() {
        let world = GitHubWorld::new("acme", "shop");
        let prs = FakePullRequests::new(world.clone());
        let repo = crate::testing::fix::test_repo("acme", "shop");
        let w = world.clone();
        let plan = move |parent: CommitSha, writes: &[(&str, &[u8])], deletes: &[&str], message: &str| {
            let files = w.files_at(parent);
            gated_plan(parent, &|p| files.get(p).map(|f| f.1.clone()), writes, deletes, message)
        };
        contract_pull_requests(PullRequestsFixture { prs: &prs, world: &world, org: repo.org_id, repo: repo.clone(), plan: &plan }).await;
        let _ = OrgId::from_uuid(uuid::Uuid::nil());
    }
}
