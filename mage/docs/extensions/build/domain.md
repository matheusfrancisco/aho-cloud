# Build domain (`mage_build_core::domain`)

Pure, no I/O, same rules as the base domain ([../../internals/domain.md](../../internals/domain.md)).
Reuses base ids and types (`OrgId`, `RepoId`, `CommitSha`, `RunId`, `ArtifactId`, `Citation`,
`Budget`, `Usage`, `FailureSignature`, `Verdict` is not used). Every aggregate carries `org_id`.

## Identifiers

`SpecId`, `PlanId`, `BuildId`, `StepId`, `CriterionId`, `ScriptId` — `Uuid` v7 newtypes, display
prefixes `spec_`, `plan_`, `bld_`, `step_`, `crit_`, `scr_`.

## Spec

What the user wants, in their words.

```
Spec {
  id, org_id, repo_id,
  source: SpecSource::{ Ui { author: UserId } | Linear { issue_id, identifier, url } },
  title, body (markdown), content_hash,
  base: SpecBase { sha: CommitSha, branch: String },   // default branch head at creation; refreshable
  status: SpecStatus,
  created_at, updated_at,
}
SpecStatus = Draft | Planning | PlanReady | Approved | Building | Shipped | Abandoned
```

`Spec.body` is untrusted data (same stance as tickets): it never grants authority.

## Plan

The agent's proposal; versioned; the user edits or comments, the agent revises.

```
Plan {
  id, spec_id, org_id, version: u32,
  author: PlanAuthor::{ Agent { run: RunId } | User { user: UserId } },  // user edits create a new version
  summary: String,
  affected: Vec<AffectedArea { title, citations: Vec<Citation>, change: ChangeKind::{Add | Modify | Remove} }>,
  steps: Vec<PlanStep { id: StepId, title, intent, touches: Vec<RepoPathGlob>, depends_on: Vec<StepId> }>,
  criteria: Vec<AcceptanceCriterion>,
  out_of_scope: Vec<String>,
  risks: Vec<String>,
  recipe_changes: Option<String>,        // e.g. new service, migration — triggers a Learn run before implement
  status: PlanStatus,
  created_at,
}
AcceptanceCriterion {
  id: CriterionId,
  statement: String,                     // "Admin can export the users table as CSV from /users"
  check: CheckKind::{ Ui { flow: String } | Api { endpoint: String } | Job { name: String } },
  demo: bool,                            // record GIF/mp4 for the PR
}
PlanStatus = Proposed | Superseded | Approved { by: UserId, at } | Rejected { by, reason }
```

Rules:
- Every `AffectedArea` has ≥ 1 citation; the base **evidence gate** checks them at `Spec.base.sha`
  (`EvidenceChecked` token required by `Plan::new`).
- `steps` form a DAG (no cycles, `depends_on` ids exist); checked in `Plan::new`.
- 1 ≤ `criteria.len()` ≤ 15; each criterion is testable (`check` set).
- Only the latest version can be approved. Approving supersedes nothing; a new version supersedes
  the previous `Proposed` one.
- `Plan::approve(by, role)` returns `PlanApproved` (proof token) only for `Proposed` and an allowed
  role (org setting; default Admin+).

## Build

One execution of an approved plan.

```
Build {
  id, org_id, spec_id, plan_id, plan_version,
  base_sha: CommitSha,
  scripts: Vec<AcceptanceScript>,        // frozen after the red gate
  steps: Vec<BuildStep>,
  status: BuildStatus,
  budget: Budget, usage: Usage,          // across all runs of the build
  pr: Option<BuildPullRequest>,
  created_at, updated_at,
}
AcceptanceScript { id: ScriptId, criterion: CriterionId, artifact: ArtifactId, sha256: [u8; 32],
                   kind: ScriptKind (base: Playwright | Shell), path: RepoPath }
BuildStep { id: StepId, plan_step: StepId, status: StepStatus, run: Option<RunId>,
            patch: Option<ArtifactId>, cumulative_patch: Option<ArtifactId> }
StepStatus = Pending | Running | Done | Failed { reason }
```

`BuildStatus` transitions:

| From | Event | To |
|---|---|---|
| `Created` | acceptance run started | `Authoring` |
| `Authoring` | scripts submitted | `RedGate` |
| `RedGate` | every script failed on base | `ScriptsFrozen` |
| `RedGate` | a script passed on base (tests nothing new), attempts left | `Authoring` (feedback) |
| `RedGate` | attempts exhausted | `Failed` |
| `ScriptsFrozen` | recipe change needed | `Relearning` |
| `Relearning` | boot gate passed | `ScriptsFrozen` |
| `ScriptsFrozen` | first step started | `Implementing` |
| `Implementing` | all steps `Done` | `GreenGate` |
| `Implementing` | a step `Failed`, attempts exhausted | `Failed` |
| `GreenGate` | all scripts pass + tests pass | `Ready` |
| `GreenGate` | failure, attempts left | `Implementing` (feedback to the last step) |
| `GreenGate` | attempts exhausted | `Failed` |
| `Ready` | PR opened | `PrOpen` |
| `PrOpen` | PR merged | `Shipped` |
| `PrOpen` | PR closed unmerged | `Ready` |
| any non-terminal | user cancel | `Cancelled` |

Terminal: `Shipped`, `Failed`, `Cancelled`. A `Failed` build keeps its artifacts; the user can
start a new build from the same approved plan.

## Proof tokens

Same pattern as the base: private constructors, created only by pure gate functions, never
persisted (re-derived from stored results).

| Token | Created by | Consumed by |
|---|---|---|
| `PlanApproved` | `Plan::approve` | `Build::new` (no build without an approved plan) |
| `ScriptsRed` | `gates::red(&scripts, &VerifyResult)` — every script ran on base and **failed** on every repetition, none errored for infra reasons | `Build::freeze_scripts` |
| `AcceptanceGatePassed` | `gates::green(&build, &VerifyResult)` — cumulative patch applied in a clean fork, every frozen script **passed** on every repetition, recipe test command exit 0, scripts byte-identical (sha256) to the frozen set | `Build` → `Ready`; `CommitPlan::build` via base extension E3 (`Shippable`) |

`AcceptanceGatePassed` carries a change-set digest (base sha, cumulative patch sha256, script
hashes), so the PR commits exactly what was gated — same binding as `FixGatePassed`.

## Why "red then green"

| Risk | Control |
|---|---|
| Agent writes scripts that always pass | Red gate: each script must fail on the unchanged base |
| Agent edits scripts while implementing | Scripts frozen by sha256; excluded from implement patches; green gate checks hashes |
| Scripts test the wrong thing | User approved the criteria; each script is linked to one criterion; scripts are shown in plan review before implementing (optional second approval, org setting) |
| Script fails on base for an unrelated reason (env broken) | Red gate requires the failure to be an assertion/HTTP/expected-absence failure, not a boot or infra error (uses base `FailureSignature` kinds) |

## Errors

`BuildDomainError = PlanCycle | MissingEvidence | NotApprovable | ScriptsNotRed { which }
| ScriptsChanged { which } | InvalidTransition { from, event } | CrossTenant | Invalid(String)`
