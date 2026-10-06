// Number, money, duration and link formatting shared by the pages.
import type { Citation, Repo, SlackThreadRef } from "../gen/mage";

/** `cost_micros` (millionths of a US dollar) → `$1.23`; sub-cent amounts keep four decimals. */
export function formatCost(micros: number): string {
  const usd = micros / 1_000_000;
  return `$${usd.toFixed(micros !== 0 && micros < 10_000 ? 4 : 2)}`;
}

/** Milliseconds → `1h 5m`, `3m 20s`, `42s`, `850 ms`. */
export function formatDuration(ms: number): string {
  if (ms < 1000) return `${Math.round(ms)} ms`;
  const s = Math.round(ms / 1000);
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ${s % 60}s`;
  return `${Math.floor(m / 60)}h ${m % 60}m`;
}

/** `0.734` → `73%`. */
export function formatConfidence(c: number): string {
  return `${Math.round(c * 100)}%`;
}

/**
 * GitHub permalink of a citation: `https://github.com/{owner}/{name}/blob/{sha}/{path}#L{a}-L{b}`
 * with each path segment percent-encoded (intake.md "Notify"). `null` when the citation is in
 * another repo than `repo`.
 */
export function citationUrl(repo: Repo | null, c: Citation): string | null {
  if (repo === null || repo.id !== c.repo_id) return null;
  const path = c.path.split("/").map(encodeURIComponent).join("/");
  return `https://github.com/${encodeURIComponent(repo.owner)}/${encodeURIComponent(repo.name)}/blob/${encodeURIComponent(c.sha)}/${path}#L${c.lines.start}-L${c.lines.end}`;
}

/** The Slack web client's URL of a thread. */
export function slackThreadUrl(t: SlackThreadRef): string {
  return `https://app.slack.com/client/${encodeURIComponent(t.team)}/${encodeURIComponent(t.channel)}/thread/${encodeURIComponent(t.channel)}-${encodeURIComponent(t.ts)}`;
}
