# Build flow (`mage_build::app`)

Use cases from spec to PR. Assumes base extension points E0–E6 ([README.md](README.md)); base
docs referenced: runs.md, executor.md, harness.md, knowledge.md, fix.md, intake.md, server.md.

## Ports

Build reuses platform ports (after E4/E5): `RunStore`-backed run manager via the `Product`
registry (E0), `ArtifactStore`, `JobQueue`, `RepoStore`, `KnowledgeStore` (read), `PullRequests`,
`TicketTracker`, `ChatNotifier`, `Clock`, `IdGen`.

Build-owned ports (`mage_build::ports`, impl `adapters::driven::pg`):

```rust
#[async_trait]
pub trait SpecStore: Send + Sync {
    async fn create(&self, org: OrgId, spec: &Spec, jobs: &[NewJob]) -> Result<(), BuildStoreError>;
    async fn get(&self, org: OrgId, id: SpecId) -> Result<Spec, BuildStoreError>;
    async fn by_linear_issue(&self, org: OrgId, issue_id: &str) -> Result<Option<Spec>, BuildStoreError>;
    async fn update(&self, org: OrgId, spec: &Spec, expected_updated_at: OffsetDateTime, jobs: &[NewJob])
        -> Result<(), BuildStoreError>;          // optimistic concurrency
    async fn list(&self, org: OrgId, q: &SpecQuery) -> Result<Page<SpecSummary>, BuildStoreError>;
}

#[async_trait]
pub trait PlanStore: Send + Sync {
    async fn put_version(&self, org: OrgId, plan: &Plan, jobs: &[NewJob]) -> Result<(), BuildStoreError>;
    async fn latest(&self, org: OrgId, spec: SpecId) -> Result<Option<Plan>, BuildStoreError>;
    async fn version(&self, org: OrgId, spec: SpecId, version: u32) -> Result<Plan, BuildStoreError>;
    async fn set_status(&self, org: OrgId, id: PlanId, status: &PlanStatus, jobs: &[NewJob]) -> Result<(), BuildStoreError>;
    async fn comments(&self, org: OrgId, id: PlanId) -> Result<Vec<PlanComment>, BuildStoreError>;
    async fn add_comment(&self, org: OrgId, c: &PlanComment) -> Result<(), BuildStoreError>;
}

#[async_trait]
pub trait BuildStore: Send + Sync {
    async fn create(&self, org: OrgId, b: &Build, jobs: &[NewJob]) -> Result<(), BuildStoreError>;
    async fn get(&self, org: OrgId, id: BuildId) -> Result<Build, BuildStoreError>;
    async fn commit(&self, org: OrgId, b: &Build, expected_status: BuildStatus, jobs: &[NewJob])
        -> Result<(), BuildStoreError>;          // fenced on status
    async fn by_run(&self, org: OrgId, run: RunId) -> Result<Option<(Build, BuildRunRole)>, BuildStoreError>;
}
```

`BuildRunRole = Plan | Acceptance | Step(StepId) | RedGate | GreenGate | Relearn` links platform runs
to Build aggregates (stored on the run as `RunSubject::Product { product: "build", subject }`, E6).

Tables (`build` schema, same conventions as storage.md: `org_id` in every key, outbox jobs):
`specs`, `plans` (one row per version, JSON columns for steps/criteria/affected), `plan_comments`,
`builds`, `build_steps`, `acceptance_scripts`.

## Run kinds (registered by the Build product)

| Run kind | `RunProfile` (E1) | Workspace | Submit tool | Gate |
|---|---|---|---|---|
| `build.plan` | Knowledge MCP + `submit_plan` | none (`empty_env`, like Triage) | `submit_plan` | evidence (citations), DAG + criteria rules |
| `build.acceptance` | `read`, `write` (only under the scripts dir), `list`, `exec`, `playwright_run`, `http_request`, `logs`, `services` + `submit_acceptance` | fork of base at `base_sha` | `submit_acceptance` | red gate (`VerifySpec::Scripts { patch: None, expect: Fail }`) |
| `build.step` | Fix-like: workspace RW, `exec`, `playwright_run` (frozen scripts read-only), `http_request`, `logs`, `db_query` + `submit_step` | fork of base + cumulative patch of done steps | `submit_step` | none per step (tests optional, advisory) |
| `build.verify_red` / `build.verify_green` | no harness (Verify) | fresh forks | — | red / green gate |
| Learn (base) | base profile | base | `submit_recipe` | boot gate (when `plan.recipe_changes` is set) |

Harness choice: same policy as support (org setting, omp default). Budgets: per run kind defaults
plus a per-build cap; `RecordUsage` sums into `Build.usage`.

## UC1 IngestSpec

- **From UI**: `POST /api/v1/build/specs { repo_id, title, body }` → `Spec { status: Draft }`.
  `base` = default branch head (via `RepoStore::default_head`). User clicks **Plan** → UC2.
- **From Linear**: issues with the configured label (e.g. `mage:build`) on configured teams go to
  Build instead of support (routing in the platform intake by label; support ignores them).
  Reuses intake's webhook verification, dedupe and debounce. Creates/updates the `Spec`;
  auto-plan is an org setting (default off).
- Indexing: if `base.sha` has no `RepoVersion`, enqueue base `index.version` and wait (same as
  triage's layer policy: Files required; Symbols, ErrorSites, AppMap waited on).

## UC2 PlanSpec (`build.plan` run)

1. Preconditions: spec `Draft | PlanReady` (re-plan), no plan run active, layers ready.
2. Create run `build.plan` with prompt: spec title/body (nonce-delimited untrusted block), repo
   overview, and on re-plan the previous plan + user comments + user edits.
3. Agent explores with Knowledge MCP and calls `submit_plan`:

```json
{
  "summary": "string ≤ 2000",
  "affected": [{ "title": "string", "change": "add|modify|remove",
                 "citations": [{ "path": "string", "lines": "12-40", "quote": "string" }] }],
  "steps": [{ "key": "s1", "title": "string", "intent": "string ≤ 1000",
              "touches": ["glob"], "depends_on": ["s0"] }],
  "criteria": [{ "statement": "string ≤ 300", "check": { "kind": "ui|api|job", "target": "string" },
                 "demo": true }],
  "out_of_scope": ["string"], "risks": ["string"],
  "recipe_changes": "string | null"
}
```

4. Synchronous validation in the submit handler (E0 `on_submit`): base evidence gate on every
   citation, DAG check, criteria count, `touches` globs valid. Reject → tool error with reasons,
   run stays Running (same 422 contract as triage); after max attempts, fail the run.
5. Accept → `Plan v(n) Proposed`, previous `Proposed` → `Superseded`, spec → `PlanReady`,
   `notify` (Linear comment with plan summary + link, if Linear-sourced).

## UC3 ReviewPlan

UI shows the plan with citations rendered as code links, steps as a graph, criteria as a checklist,
and a diff against the previous version.

| Action | Effect |
|---|---|
| Comment (on plan, step, or criterion) | `PlanComment`; **Revise** runs UC2 with comments |
| Edit directly (user changes text, removes/adds steps or criteria) | New version `author: User`; evidence gate re-runs on any citation the user added |
| Approve | `Plan::approve` → `PlanApproved`; spec → `Approved`; UC4 starts (or waits for **Build** click, org setting) |
| Reject | spec back to `Draft` |

Approval is recorded with user, version, and timestamp (audit log).

## UC4 StartBuild and AuthorAcceptance (`build.acceptance` run)

1. `Build::new(&PlanApproved, base_sha)`; if `plan.recipe_changes` is set, the build will need a
   Learn run after scripts are frozen (B5).
2. Run `build.acceptance` on a fork of the base (base executor prepare/fork). The agent writes one
   script per criterion under `.mage/acceptance/<criterion-key>.{spec.ts|sh}` (Playwright for `ui`,
   shell/HTTP for `api|job`), may run them, and calls `submit_acceptance`:

```json
{ "scripts": [{ "criterion": "crit key", "path": ".mage/acceptance/export-csv.spec.ts",
                "expected_failure": { "kind": "assertion_text|http_status|error_string",
                                      "value": "string", "matcher": "contains" } }] }
```

3. Runner uploads each script as an artifact (sha256 recorded); server validates one script per
   criterion and paths under `.mage/acceptance/`.
4. **Red gate**: Verify run `VerifySpec::Scripts { patch: None, scripts, expect: Fail, tests: false,
   repetitions: 2 }` on fresh forks of the base. Each script must fail with its expected failure
   kind (not a boot/infra error). `gates::red` → `ScriptsRed` → `Build::freeze_scripts`.
5. Red gate failure (a script passes on base, or fails for the wrong reason) → `GateFeedback` to the
   acceptance session with the offending scripts; attempts per budget.

Optional org setting `review_scripts`: after freezing, the user reviews scripts before
implementing (shown next to their criteria).

## UC5 Implement (`build.step` runs)

1. Steps run in topological order of the plan DAG; v1 runs them sequentially (parallel later).
2. Each step forks the base and applies the **cumulative patch** of all done steps
   (`Env::apply_patch` + `Env::rebuild(Changed)`), so step N sees steps 1..N-1.
3. Prompt: approved plan, this step's intent and `touches`, the frozen scripts (read-only), notes
   from previous steps.
4. Agent edits code with built-in tools (v1 workspace policy, as in Fix) and Run MCP; may run the
   frozen scripts and tests to self-check. `submit_step { summary, notes_for_next }`.
5. Runner computes the step patch with `Env::diff_against_base` against the step's starting tree,
   excluding `.mage/acceptance/**` (frozen scripts can't change) and base `PatchPolicy` exclusions;
   rejects workflow files, `.git`, symlinks, binaries (same rules as fix.md).
6. Server re-checks the patch (policy, size, `SecretScanner`) and stores step patch + new
   cumulative patch. Touching paths outside the step's `touches` is allowed but flagged in the PR.
7. Step failure (no submit, policy violation after attempts) → build `Failed` or user may **Retry
   step** / **Edit plan** (new plan version → new build).

## UC6 GreenGate

Verify run `VerifySpec::Scripts { patch: Some(cumulative), scripts: frozen, expect: Pass,
tests: true, repetitions: 2 }` on fresh forks:

- cumulative patch applies at `base_sha`; rebuild; migrate;
- every frozen script passes on every repetition (scripts verified by sha256 before running);
- recipe test command exits 0;
- recordings: every criterion with `demo: true` is recorded (Playwright video; API criteria get a
  request-timeline rendering).

`gates::green` → `AcceptanceGatePassed` → build `Ready`. Failure with attempts left → feedback
steered into a new `build.step` run for the last step (or a dedicated "repair" step) with the
failing scripts' output.

## UC7 ShipBuild (PR)

Reuses fix.md's F3 machinery through E3/E4:

1. If the default branch moved past `base_sha`: rebase the cumulative patch in a fork
   (`AllowOffset`), re-run the green gate; conflict → user decides (re-plan or manual).
2. `CommitPlan::build(&AcceptanceGatePassed, …)` with the cumulative patch **plus** the frozen
   acceptance scripts (they ship as tests; location per org setting: keep `.mage/acceptance/` or
   move to the repo's test dir).
3. Media: runner-produced mp4 + GIF per demo criterion (same conversion rules as fix.md "Media in
   the PR and in Linear"); `PullRequests::upload_media`; fallback to Mage artifact links.
4. Branch `mage/build-<spec-short>-<digest7>`, draft PR by default, labels, reviewers.
5. Linear (if spec is Linear-sourced): comment with PR link and the mp4 via `TicketTracker::upload_file`.

PR body template:

````markdown
Implements {spec.title} ({linear link if any}) — planned and verified by Mage.

## Overview
{plan.summary}

### What changed
| Area | Change | Files |
|---|---|---|
| {affected.title} | {add/modify/remove} | {n files} |

### Steps
1. {step.title} — {step summary from submit_step}

## Acceptance (approved by {approver} on plan v{n})
| Criterion | Before (base) | After (this PR) | Demo |
|---|---|---|---|
| {criterion.statement} | fails: `{expected_failure}` | passes ({repetitions}×) | ![gif]({gif_url}) |

{mp4 embeds for demo criteria}

## Verification (clean environment)
| Check | Result |
|---|---|
| Patch applies at `{base_short}` | pass |
| Acceptance scripts ({k}) | pass on {repetitions} fresh forks |
| `{recipe.test}` | exit 0 |

Out of scope: {plan.out_of_scope}. Risks: {plan.risks}.
Changes outside planned areas: {flagged paths or "none"}.

<!-- mage:build={build_id} plan={plan_id}@v{n} change-set={digest_hex} -->
````

The overview is assembled from stored, already-validated data (plan, step summaries, gate
results); no extra model call. Agent-written text is quoted and escaped like fix.md's PR body.

## UC8 PR lifecycle

`fix.pr_event`-style handling for `mage/build-*` branches: merged → `Shipped` + Linear state;
closed unmerged → `Ready` (user can re-open or start a new build).

## Jobs

| Kind | Payload | Dedupe |
|---|---|---|
| `build.plan` | `{org_id, spec_id, reason}` | `build.plan:{spec_id}:{spec.content_hash}` |
| `build.start` | `{org_id, build_id}` | `build.start:{build_id}` |
| `build.next_step` | `{org_id, build_id}` | `build.next_step:{build_id}:{done_count}` |
| `build.ship` | `{org_id, build_id}` | `build.ship:{build_id}` |
| `build.pr_event` | `{org_id, repo_id, number, delivery_id}` | `build.pr_event:{repo_id}:{number}:{delivery_id}` |
| `notify.*` (base) | Build notices: `PlanReady`, `BuildReady`, `BuildFailed`, `PrOpened` | base rules |

## Failure modes

| Failure | Handling |
|---|---|
| Plan cites code that doesn't exist | Evidence gate rejects; agent revises |
| Criteria too vague to script | Acceptance agent may return `unscriptable: [criterion]` → build pauses, user edits plan |
| Script passes on base | Red gate feedback; after attempts → user edits criterion |
| Feature needs new service / migration | `plan.recipe_changes` → Learn run after freeze; boot gate before steps |
| Step drifts outside `touches` | Allowed, flagged in UI and PR |
| Green gate flaky | Repetitions must all pass; flake → fail with both outputs, user can **Re-run gate** |
| Default branch moved | Rebase + green gate again (UC7.1) |
| Budget exceeded | Run `TimedOut`; build `Failed` with partial artifacts (plan, scripts, patches) downloadable |

## Security

- Same trust boundary as the base: spec, comments, repo text are untrusted; authority comes only
  from the approved plan, run profiles, and server checks.
- Frozen scripts are the acceptance contract: excluded from step patches, hash-checked in the gate,
  shipped as reviewed tests.
- v1 host runner: trusted repos only (base rule). Build writes much more code than Fix, so the
  patch checks (workflow files, secrets, size) matter more; consider lower size limits per step.
- PR always requires human review; Mage never merges.

## Testing

| Layer | Tests |
|---|---|
| Domain | `Plan::new` (DAG cycles, missing evidence, criteria bounds), `Plan::approve` roles/versions, `BuildStatus` full transition table, `gates::red`/`gates::green` tables (pass on base, infra error vs assertion, script hash mismatch, one repetition failing) |
| Use cases (fakes) | re-plan with comments; approve stale version refused; red-gate feedback loop; step chaining applies cumulative patch; green-gate failure routes feedback; ship refuses without `AcceptanceGatePassed` |
| Contract | `SpecStore`/`PlanStore`/`BuildStore` suites against fakes and pg |
| End to end | fixture repo + feature spec ("add CSV export"): plan → approve → red → implement → green → PR on a throwaway GitHub repo (opt-in) |
| Eval | replay historical merged features: spec from PR description, compare plan `affected` vs actual changed files (recall), green-gate pass rate, cost |
