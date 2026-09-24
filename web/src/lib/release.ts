// Approval-card logic: how a surface diff reads to a human, and what the next step needs.
import type { ApprovalView, Release } from "../gen/api";
import type { Change, Direction, Stage, SurfaceDiff } from "../gen/contract";

export const STAGES: readonly Stage[] = ["preview", "staging", "live"];

/** Changes by direction, in diff order. Widenings are what the approvers sign for. */
export function groupChanges(diff: SurfaceDiff): Record<Direction, Change[]> {
  const groups: Record<Direction, Change[]> = { widen: [], narrow: [], neutral: [] };
  for (const c of diff.changes) groups[c.direction].push(c);
  return groups;
}

/** The stage a release goes to next: preview first, then staging, then live; `null` once live. */
export function nextStage(current: Stage | null): Stage | null {
  if (current === null) return "preview";
  return STAGES[STAGES.indexOf(current) + 1] ?? null;
}

/** Progress of a release's approvals at `stage` against its requirement. Preview needs none; each approver counts once. */
export function approvalProgress(release: Release, stage: Stage): { approvals: ApprovalView[]; count: number; needed: number; ownerRequired: boolean; ownerPresent: boolean; met: boolean } {
  const approvals = release.approvals.filter((a) => a.stage === stage);
  const count = new Set(approvals.map((a) => a.user.id)).size;
  const needed = stage === "preview" ? 0 : release.requirement.approvals;
  const ownerRequired = stage !== "preview" && release.requirement.owner_required;
  const ownerPresent = approvals.some((a) => a.owner);
  return { approvals, count, needed, ownerRequired, ownerPresent, met: count >= needed && (!ownerRequired || ownerPresent) };
}
