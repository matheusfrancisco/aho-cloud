# Domain (`mage_core::domain`)

Pure Rust: no I/O, no async. The web UI's TypeScript types are generated from the wire DTOs
(ts-rs). Depends only on `serde`, `thiserror`, `time`, `uuid`, `sha2`. Every crate speaks these types.

## Identifiers

| Type | Inner | Notes |
|---|---|---|
| `OrgId`, `UserId`, `RepoId`, `TicketId`, `CauseId`, `RunId`, `GateId`, `ArtifactId`, `JobId`, `RunnerId`, `CustomerId` | `Uuid` (v7, time-sortable) | Newtypes; `Display` as `prefix_<base32>` (e.g. `run_01j…`) for logs and URLs |
| `CommitSha` | `[u8; 20]` | Hex `Display`/`FromStr`; validated length |
| `BlobOid`, `TreeOid` | `[u8; 20]` | Git object ids; content-addressed cache keys |
| `RepoVersionKey` | `(RepoId, CommitSha)` | Identity of an indexed commit |
| `RecipeHash` | `[u8; 32]` | SHA-256 of the normalized recipe TOML |

Ids are created through the `IdGen` port so tests are deterministic.

## Tenancy

Every aggregate carries `org_id: OrgId`. Constructors take it; there is no setter. Any operation
that links two aggregates (run → ticket, cause → citation repo) checks equal `org_id` and returns
`DomainError::CrossTenant` otherwise.

## Aggregates

### Org, User, Membership, Integration

- `Org { id, slug, name, created_at }`
- `User { id, email, name }`, `Membership { org_id, user_id, role: Owner | Admin | Member }`
- `Integration { org_id, kind: GitHub | Linear | Slack, external_id, status: Active | Revoked }`
  (`external_id` = GitHub installation id / Linear workspace id / Slack team id)

### Repo and RepoVersion

- `Repo { id, org_id, owner, name, default_branch, installation_id, created_at }`
- `RepoVersion { repo_id, sha, reasons: BTreeSet<VersionReason>, layers: LayerMap, created_at }`
  - `VersionReason = DefaultHead | Release(tag) | Deployed(env) | OnDemand(TicketId)`
  - `LayerMap = BTreeMap<Layer, LayerState>`
  - `Layer = Files | Symbols | Refs | ErrorSites | AppMap | Semantic | History`
  - `LayerState = Pending | Running | Ready { at } | Failed { reason } | Skipped { reason }`
- Rule: a query may use a layer only when it is `Ready`. Tools report which layer answered.

### Recipe

- `Recipe { repo_id, hash: RecipeHash, body: String (TOML), validated_at_sha: Option<CommitSha>, status }`
- `RecipeStatus = Draft | Validated { at } | Broken { reason, at }`
- `Recipe::validate(gate: BootGatePassed)` is the only way to reach `Validated`.

### Deployment and Customer

- `Customer { id, org_id, name, external_refs: Vec<ExternalRef>, environment: Option<String> }`
- `Deployment { repo_id, environment, sha, observed_at, source: GitHubDeployments | Manual | Tag }`
- Resolution order for a ticket's sha: ticket override → customer environment deployment →
  repo default branch head (marked `assumed` on the ticket).

### Ticket

```
Ticket {
  id, org_id,
  source: TicketSource::Linear { issue_id, identifier, url },
  title, body, content_hash,
  slack_thread: Option<SlackThreadRef { team, channel, ts }>,
  customer: Option<CustomerId>,
  target: Option<TicketTarget { repo_id, sha, assumed: bool }>,
  error_strings: Vec<String>,
  status: TicketStatus,
  created_at, updated_at,
}
```

`TicketStatus` transitions:

| From | Event | To |
|---|---|---|
| `New` | triage started | `Triaging` |
| `Triaging` | triage succeeded | `Triaged` |
| `Triaging` | triage failed | `TriageFailed` |
| `TriageFailed`, `Triaged` | content changed / manual retriage | `Triaging` |
| `Triaged` | diagnose run started | `Diagnosing` |
| `Diagnosing` | all diagnose runs terminal, ≥1 cause `Reproduced` | `Diagnosed` |
| `Diagnosing` | all diagnose runs terminal, none reproduced | `Triaged` |
| `Diagnosed` | fix run started | `Fixing` |
| `Fixing` | fix gate passed | `FixReady` |
| `Fixing` | fix failed | `Diagnosed` |
| `FixReady` | PR opened | `PrOpen` |
| any non-terminal | resolved by user | `Resolved` |
| any | closed upstream (Linear) | `Closed` |

`Resolved` and `Closed` are terminal. Retriage is refused while `Diagnosing` or `Fixing`.

### Cause

```
Cause {
  id, ticket_id, org_id, rank: u16,
  title, explanation,
  evidence: Vec<Citation>,          // ≥ 1, checked by the evidence gate
  repro_steps: Vec<String>,
  confirm_if: String, rule_out_if: String,
  confidence: Confidence (0.0..=1.0),
  method: ReproMethod (Ui | Api | Job | Unknown),
  status: CauseStatus,
}
Citation { repo_id, sha, path, lines: LineRange, snippet_hash: Option<[u8; 32]> }
```

`CauseStatus = Untested | Running(RunId) | Reproduced { run, gate } | NotReproduced { run } | Inconclusive { run, reason }`

`Cause::mark_reproduced(proof: ReproGatePassed)` is the only path to `Reproduced`.

### Run

```
Run {
  id, org_id, kind: RunKind, subject: RunSubject,
  harness: Option<HarnessKind>,       // None for Verify
  status: RunStatus, attempt: u16,
  budget: Budget, usage: Usage,
  parent: Option<RunId>,              // Verify runs point at the run they check
  runner: Option<RunnerLease { runner_id, until }>,
  result: Option<RunResult>, error: Option<RunError>,
  created_at, started_at, finished_at,
}
```

| `RunKind` | `RunSubject` | Harness | Result |
|---|---|---|---|
| `Learn` | `RepoVersion(key)` | yes | `RecipeProposal { toml, notes }` |
| `Triage` | `Ticket(id)` | yes, Knowledge MCP only | `TriageResult { causes: Vec<CauseDraft> }` |
| `Diagnose` | `Cause(id)` | yes | `DiagnoseResult { verdict, script: ArtifactId, signature: FailureSignature, summary }` |
| `Verify` | `Run(parent)` | no | `VerifyResult { outcome, observed: Option<FailureSignature>, attempts, artifacts }` |
| `Fix` | `Cause(id)` | yes | `FixResult { patch: ArtifactId, summary }` |

`HarnessKind = Omp | ClaudeCode` (later `Aho`).

`RunStatus` transitions:

| From | Event | To |
|---|---|---|
| `Queued` | runner leased it | `Leased` |
| `Leased` | lease expired before start | `Queued` (attempt + 1) |
| `Leased` | env preparing | `Preparing` |
| `Preparing` | harness started / verify started | `Running` |
| `Running` | result submitted, kind needs a gate | `AwaitingGate` |
| `Running` | result submitted, no gate (Triage after evidence gate inline) | `Succeeded` |
| `AwaitingGate` | gate passed | `Succeeded` |
| `AwaitingGate` | gate failed, attempts left | `Running` (feedback steered into the session) |
| `AwaitingGate` | gate failed, no attempts left | `Failed` |
| `Running`, `Preparing` | budget exceeded | `TimedOut` |
| any non-terminal | user cancel | `Cancelled` |
| any non-terminal | infrastructure error | `Errored` |
| `Leased`, `Preparing`, `Running` | runner lease lost | `Queued` if attempt < max, else `Errored` |

Terminal: `Succeeded`, `Failed`, `TimedOut`, `Cancelled`, `Errored`.

### Budget and Usage

- `Budget { max_wall: Duration, max_turns: u32, max_cost_micros: u64, max_gate_attempts: u8 }`
- `Usage { input_tokens, output_tokens, cost_micros, turns, wall }`
- `Usage::exceeds(&Budget) -> Option<BudgetLimit>` — pure; the run manager calls it on every
  usage event.

### Gate

```
Gate { id, run_id, kind: GateKind, attempt: u8, status: GateStatus, verify_run: Option<RunId> }
GateKind = Evidence | Boot | Repro | Fix
GateStatus = Pending | Passed { at } | Failed { reason, at }
```

Proof tokens — zero-sized-ish types with private constructors, created only by gate evaluation
functions in this crate:

| Token | Created by | Consumed by |
|---|---|---|
| `EvidenceChecked` | `evidence::check(citations, &ManifestView)` | `Cause::new` (no cause without it) |
| `BootGatePassed` | `gates::boot(&VerifyResult)` | `Recipe::validate` |
| `ReproGatePassed` | `gates::repro(&DiagnoseResult, &VerifyResult)` | `Cause::mark_reproduced` |
| `FixGatePassed` | `gates::fix(&FixResult, &VerifyResult)` | `Ticket` → `FixReady`, `PullRequests` use case |

This makes "reproduced without a passing gate" a compile error, not a code-review item.

### FailureSignature

How a repro failure is identified so a clean re-run can be compared:

```
FailureSignature {
  kind: AssertionText | HttpStatus | ErrorString | ExitCode,
  value: String,
  matcher: Exact | Contains | Regex,
}
```

`FailureSignature::matches(&observed) -> bool` is pure. Repro gate: the verify run must observe a
matching signature on every attempt. Fix gate: the verify run must observe **no** matching
signature and the test command must exit 0.

### Verdict

`Verdict = Reproduced | NotReproduced | Inconclusive { reason }`. A harness may *claim*
`Reproduced`; the domain stores the claim on `DiagnoseResult` and only the repro gate moves the
cause to `Reproduced`.

### Artifact

```
Artifact { id, org_id, run_id, kind: ArtifactKind, key: String, bytes: u64, sha256: [u8; 32], created_at }
ArtifactKind = Video | Gif | Trace | Screenshot | Log | Script | Patch | Transcript | ScipIndex | SourceBundle | FixBrief
```

`key` follows `org/{org}/runs/{run}/{kind}/{name}` (see [storage.md](storage.md)).

### RunEvent

```
RunEvent { run_id, seq: u64, at, kind: RunEventKind }
RunEventKind = Status(RunStatus) | AssistantText(String) | ToolCall { name, args_preview }
             | ToolResult { name, ok, preview } | Usage(Usage) | Notice(String)
             | GateStarted(GateKind) | GateResult(GateKind, GateStatus) | Artifact(ArtifactId)
             | Error(String)
```

`seq` is assigned by the server, strictly increasing per run. Previews are truncated to 4 KiB.

### FixBrief and Resolution

- `FixBrief { cause_id, markdown, created_at }` — created only for a `Reproduced` cause.
- `Resolution { ticket_id, confirmed_cause: Option<CauseId>, root_cause, fix_pr_url: Option<Url>, repro_script: Option<ArtifactId>, resolved_by: UserId, at }`

## Errors

```
DomainError = CrossTenant | InvalidTransition { from, event } | MissingEvidence
            | BudgetExceeded(BudgetLimit) | Invalid(String)
```

Transition functions are `fn apply(self, event) -> Result<Self, DomainError>` on each status enum,
tested with a full `(state, event)` table.
