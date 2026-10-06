// Display names and badge tones of the wire enums.
import type { ArtifactKind, CauseStatus, GateKind, GateStatus, HarnessKind, IntegrationKind, Layer, LayerState, MemberRole, RecipeStatus, RunKind, RunStatus, TicketStatus } from "../gen/mage";

export type Tone = "ok" | "warn" | "bad" | "info" | "muted";

export const TICKET_STATUSES: readonly TicketStatus[] = ["new", "triaging", "triaged", "triage_failed", "diagnosing", "diagnosed", "fixing", "fix_ready", "pr_open", "resolved", "closed"];

export const TICKET_STATUS_LABEL: Record<TicketStatus, string> = {
  new: "New",
  triaging: "Triaging",
  triaged: "Triaged",
  triage_failed: "Triage failed",
  diagnosing: "Diagnosing",
  diagnosed: "Diagnosed",
  fixing: "Fixing",
  fix_ready: "Fix ready",
  pr_open: "PR open",
  resolved: "Resolved",
  closed: "Closed",
};

export const TICKET_STATUS_TONE: Record<TicketStatus, Tone> = {
  new: "muted",
  triaging: "info",
  triaged: "ok",
  triage_failed: "bad",
  diagnosing: "info",
  diagnosed: "ok",
  fixing: "info",
  fix_ready: "ok",
  pr_open: "ok",
  resolved: "muted",
  closed: "muted",
};

export const RUN_STATUS_LABEL: Record<RunStatus, string> = {
  queued: "Queued",
  leased: "Leased",
  preparing: "Preparing",
  running: "Running",
  awaiting_gate: "Awaiting gate",
  succeeded: "Succeeded",
  failed: "Failed",
  timed_out: "Timed out",
  cancelled: "Cancelled",
  errored: "Errored",
};

export const RUN_STATUS_TONE: Record<RunStatus, Tone> = {
  queued: "muted",
  leased: "muted",
  preparing: "info",
  running: "info",
  awaiting_gate: "warn",
  succeeded: "ok",
  failed: "bad",
  timed_out: "bad",
  cancelled: "muted",
  errored: "bad",
};

/** Statuses a run never leaves. */
export const TERMINAL: Record<RunStatus, boolean> = {
  queued: false,
  leased: false,
  preparing: false,
  running: false,
  awaiting_gate: false,
  succeeded: true,
  failed: true,
  timed_out: true,
  cancelled: true,
  errored: true,
};

export const RUN_KIND_LABEL: Record<RunKind, string> = {
  learn: "Learn",
  triage: "Triage",
  diagnose: "Diagnose",
  verify: "Verify",
  fix: "Fix",
};

export const GATE_LABEL: Record<GateKind, string> = {
  evidence: "Evidence",
  boot: "Boot",
  repro: "Repro",
  fix: "Fix",
};

export const HARNESSES: readonly HarnessKind[] = ["omp", "claude_code"];

export const HARNESS_LABEL: Record<HarnessKind, string> = {
  omp: "omp",
  claude_code: "Claude Code",
};

export const ROLES: readonly MemberRole[] = ["member", "admin", "owner"];

export const ROLE_LABEL: Record<MemberRole, string> = {
  member: "Member",
  admin: "Admin",
  owner: "Owner",
};

const ROLE_RANK: Record<MemberRole, number> = { member: 0, admin: 1, owner: 2 };

/** `Owner > Admin > Member` (server.md "RBAC"). */
export function atLeast(role: MemberRole, min: MemberRole): boolean {
  return ROLE_RANK[role] >= ROLE_RANK[min];
}

export const INTEGRATION_LABEL: Record<IntegrationKind, string> = {
  github: "GitHub",
  github_fix: "GitHub fix app",
  linear: "Linear",
  slack: "Slack",
};

export const LAYERS: readonly Layer[] = ["files", "symbols", "refs", "error_sites", "app_map", "semantic", "history"];

export const LAYER_LABEL: Record<Layer, string> = {
  files: "Files",
  symbols: "Symbols",
  refs: "Refs",
  error_sites: "Error sites",
  app_map: "App map",
  semantic: "Semantic",
  history: "History",
};

export const LAYER_TONE: Record<LayerState["type"], Tone> = {
  pending: "muted",
  running: "info",
  ready: "ok",
  failed: "bad",
  skipped: "muted",
};

export const RECIPE_STATUS_LABEL: Record<RecipeStatus["type"], string> = {
  draft: "Draft",
  validated: "Validated",
  broken: "Broken",
};

export const RECIPE_STATUS_TONE: Record<RecipeStatus["type"], Tone> = {
  draft: "warn",
  validated: "ok",
  broken: "bad",
};

export const CAUSE_STATUS_LABEL: Record<CauseStatus["type"], string> = {
  untested: "Untested",
  running: "Diagnosing",
  reproduced: "Reproduced",
  not_reproduced: "Not reproduced",
  inconclusive: "Inconclusive",
};

export const CAUSE_STATUS_TONE: Record<CauseStatus["type"], Tone> = {
  untested: "muted",
  running: "info",
  reproduced: "ok",
  not_reproduced: "warn",
  inconclusive: "warn",
};

export const GATE_STATUS_TONE: Record<GateStatus["type"], Tone> = {
  pending: "info",
  passed: "ok",
  failed: "bad",
};

export const ARTIFACT_KIND_LABEL: Record<ArtifactKind, string> = {
  video: "Video",
  gif: "GIF",
  trace: "Trace",
  screenshot: "Screenshot",
  log: "Log",
  script: "Repro script",
  patch: "Patch",
  transcript: "Transcript",
  scip_index: "SCIP index",
  source_bundle: "Source bundle",
  fix_brief: "Fix brief",
};
