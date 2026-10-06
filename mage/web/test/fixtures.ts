import type { Budget, Cause, OrgSettings, Repo, RepoDetail, Run, RunDetail, RunEvent, RunEventKind, RunSummary, TicketDetail, TicketSummary, Usage, VersionView } from "../src/gen/mage";

export const SHA = "0123456789abcdef0123456789abcdef01234567";
export const SHA2 = "fedcba9876543210fedcba9876543210fedcba98";

export const budget: Budget = { max_wall: 2_700_000, max_turns: 150, max_cost_micros: 15_000_000, max_gate_attempts: 2 };
export const noUsage: Usage = { input_tokens: 0, output_tokens: 0, cost_micros: 0, turns: 0, wall: 0 };

export function repo(over: Partial<Repo> = {}): Repo {
  return { id: "rep_web", org_id: "org_acme", external_id: 42, owner: "acme", name: "web", default_branch: "main", installation_id: 7, access: "granted", created_at: "2026-09-01T10:00:00Z", ...over };
}

export function ticketSummary(over: Partial<TicketSummary> = {}): TicketSummary {
  return {
    id: "tkt_1",
    identifier: "ENG-412",
    title: "Checkout button does nothing",
    status: "triaged",
    target: { repo_id: "rep_web", sha: SHA, origin: { type: "default_head" } },
    assumed: true,
    top_cause: { cause_id: "cau_1", title: "Missing await in submit handler", confidence: 0.82 },
    customer: null,
    updated_at: "2026-10-01T12:00:00Z",
    ...over,
  };
}

export function cause(over: Partial<Cause> = {}): Cause {
  return {
    id: "cau_1",
    ticket_id: "tkt_1",
    org_id: "org_acme",
    triage_run: "run_triage",
    rank: 1,
    title: "Missing await in submit handler",
    explanation: "The handler returns before **the promise** settles.",
    evidence: [{ repo_id: "rep_web", sha: SHA, path: "src/checkout/submit button.ts", lines: { start: 10, end: 14 }, snippet_hash: null, note: "returns early" }],
    repro_steps: ["Open /checkout", "Click Pay"],
    confirm_if: "the request never fires",
    rule_out_if: "the request fires",
    confidence: 0.82,
    method: "ui",
    status: { type: "untested" },
    superseded_at: null,
    ...over,
  };
}

export function runSummary(over: Partial<RunSummary> = {}): RunSummary {
  return {
    id: "run_1",
    kind: "diagnose",
    status: "running",
    subject: { type: "cause", data: "cau_1" },
    harness: "omp",
    attempt: 1,
    parent: null,
    usage: noUsage,
    budget,
    error: null,
    created_at: "2026-10-01T12:00:00Z",
    started_at: "2026-10-01T12:00:05Z",
    finished_at: null,
    ...over,
  };
}

export function ticketDetail(over: Partial<TicketDetail> = {}, causes: Cause[] = [cause()]): TicketDetail {
  return {
    ticket: {
      id: "tkt_1",
      org_id: "org_acme",
      source: { type: "linear", issue_id: "lin_1", identifier: "ENG-412", url: "https://linear.app/acme/issue/ENG-412" },
      title: "Checkout button does nothing",
      body: "Clicking **Pay** does nothing.\n\n<script>alert(1)</script>",
      content_hash: "h1",
      slack_thread: null,
      customer: null,
      target: { repo_id: "rep_web", sha: SHA, origin: { type: "default_head" } },
      error_strings: [],
      status: "triaged",
      triaged_hash: "h1",
      triage_run: "run_triage",
      created_at: "2026-10-01T10:00:00Z",
      updated_at: "2026-10-01T12:00:00Z",
    },
    assumed: true,
    repo: repo(),
    customer: null,
    causes,
    runs: [],
    pull_requests: [],
    resolution: null,
    ...over,
  };
}

export function run(over: Partial<Run> = {}): Run {
  return {
    id: "run_1",
    org_id: "org_acme",
    kind: "diagnose",
    subject: { type: "cause", data: "cau_1" },
    harness: "omp",
    status: "running",
    attempt: 1,
    budget,
    usage: { input_tokens: 1200, output_tokens: 300, cost_micros: 250_000, turns: 3, wall: 60_000 },
    parent: null,
    runner: null,
    result: null,
    error: null,
    created_at: "2026-10-01T12:00:00Z",
    started_at: "2026-10-01T12:00:05Z",
    finished_at: null,
    ...over,
  };
}

export function runDetail(over: Partial<Run> = {}, rest: Partial<Omit<RunDetail, "run">> = {}): RunDetail {
  return { run: run(over), gates: [], verify_runs: [], ...rest };
}

export function event(seq: number, kind: RunEventKind, runId = "run_1"): RunEvent {
  return { run_id: runId, seq, at: `2026-10-01T12:00:${String(seq).padStart(2, "0")}Z`, kind };
}

export const text = (seq: number, data: string) => event(seq, { type: "assistant_text", data });

export function settings(over: { run?: boolean; pull_requests?: boolean } = {}): OrgSettings {
  return {
    general: { harness: "omp", budgets: { learn: budget, diagnose: budget, fix: budget } },
    triage: { auto_triage: true, harness: "omp", model: null, budget, max_causes: 5, layer_wait_ms: 120_000, auto_retriage_per_day: 2, ticket_cost_cap_micros: 5_000_000 },
    fix: {
      brief: true,
      brief_in_linear: "summary",
      run: over.run ?? false,
      pull_requests: over.pull_requests ?? false,
      open_pr: "manual",
      pr: { branch_from: "default_head", draft: true, labels: [], reviewers: { users: [], teams: [] } },
      lockfiles: "exclude",
      allow_workflow_edits: false,
      limits: { max_patch_bytes: 262_144, max_files: 20, max_changed_lines: 400, max_file_bytes: 1_048_576 },
      repetitions: 2,
      regression: "mage_dir",
    },
  };
}

export function version(layers: VersionView["layers"], sha = SHA): VersionView {
  return { sha, reasons: [{ type: "default_head" }], layers, created_at: "2026-10-01T10:00:00Z" };
}

export const READY = { type: "ready", at: "2026-10-01T10:05:00Z" } as const;

export function repoDetail(over: Partial<RepoDetail> = {}): RepoDetail {
  return { repo: repo(), versions: [], recipe: null, deployments: [], last_learn: null, ...over };
}
