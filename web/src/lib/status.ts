// The org dashboard's view of its projects: one status per project, chip counts, the search
// filter, and the org-wide activity feed.
import type { LedgerEntry, Project, Release } from "../gen/api";

export type ProjectStatus = "live" | "preview" | "building" | "idle";

/** Chip order on the dashboard. */
export const PROJECT_STATUSES: readonly ProjectStatus[] = ["live", "preview", "building", "idle"];

export interface ProjectRow {
  project: Project;
  /** Newest release, if any. */
  latest: Release | null;
  status: ProjectStatus;
}

/**
 * Live when a release is active at live; Preview when one runs at preview or staging only;
 * Building when nothing runs yet and the newest release is building; Idle otherwise.
 */
export function projectStatus(project: Project, latest: Release | null): ProjectStatus {
  if (project.active.some((a) => a.stage === "live")) return "live";
  if (project.active.length > 0) return "preview";
  return latest?.status === "building" ? "building" : "idle";
}

/** `releases` newest first, as the API lists them. */
export function projectRow(project: Project, releases: readonly Release[]): ProjectRow {
  const latest = releases[0] ?? null;
  return { project, latest, status: projectStatus(project, latest) };
}

export function statusCounts(rows: readonly ProjectRow[]): Record<ProjectStatus, number> {
  const counts: Record<ProjectStatus, number> = { live: 0, preview: 0, building: 0, idle: 0 };
  for (const r of rows) counts[r.status]++;
  return counts;
}

/** Rows whose slug or a domain contains `query` (case-insensitive), limited to `status` when set. */
export function filterRows(rows: readonly ProjectRow[], query: string, status: ProjectStatus | null): ProjectRow[] {
  const q = query.trim().toLowerCase();
  return rows.filter(
    (r) => (status === null || r.status === status) && (q === "" || r.project.slug.includes(q) || r.project.domains.some((d) => d.hostname.includes(q))),
  );
}

export interface Activity {
  project: string;
  entry: LedgerEntry;
}

/** The newest `limit` ledger entries across projects, newest first (ties: higher ledger id first). */
export function recentActivity(ledgers: ReadonlyArray<{ project: string; entries: readonly LedgerEntry[] }>, limit: number): Activity[] {
  return ledgers
    .flatMap(({ project, entries }) => entries.map((entry) => ({ project, entry })))
    .sort((a, b) => b.entry.at - a.entry.at || b.entry.id - a.entry.id)
    .slice(0, limit);
}
