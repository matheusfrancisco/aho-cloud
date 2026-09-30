// What the console remembers across visits, per signed-in user: the org last worked in (the
// sidebar's context and the home redirect), and the preview URLs workspaces reported.
import { useSyncExternalStore } from "react";
import { currentSession } from "./session";

const listeners = new Set<() => void>();

function key(what: string): string | null {
  const s = currentSession();
  return s === null ? null : `aho-cloud.${what}.${s.user.id}`;
}

/** Org slug the user last worked in, or `null`. */
export function currentOrg(): string | null {
  const k = key("org");
  return k === null ? null : localStorage.getItem(k);
}

export function setCurrentOrg(org: string): void {
  const k = key("org");
  if (k === null || localStorage.getItem(k) === org) return;
  localStorage.setItem(k, org);
  for (const l of listeners) l();
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

export function useCurrentOrg(): string | null {
  return useSyncExternalStore(subscribe, currentOrg, currentOrg);
}

interface Previews {
  /** Preview URL with `{project}` for the slug, learned from a workspace start. */
  template: string | null;
  urls: Record<string, string>;
}

function previews(): Previews {
  const k = key("previews");
  const raw = k === null ? null : localStorage.getItem(k);
  if (raw !== null) {
    try {
      const v = JSON.parse(raw) as Partial<Previews>;
      return { template: typeof v.template === "string" ? v.template : null, urls: typeof v.urls === "object" && v.urls !== null ? v.urls : {} };
    } catch {
      // A corrupt entry is replaced on the next write.
    }
  }
  return { template: null, urls: {} };
}

/**
 * Records the preview URL a workspace start reported. When its host starts with the project
 * slug (the session host's `{project}.` template), the template is kept for other projects too.
 */
export function rememberPreview(project: string, url: string): void {
  const k = key("previews");
  if (k === null) return;
  const p = previews();
  p.urls[project] = url;
  const marker = `//${project}.`;
  if (url.includes(marker)) p.template = url.replace(marker, "//{project}.");
  localStorage.setItem(k, JSON.stringify(p));
}

/** The project's preview URL: as reported for it, else from the learned template, else `null`. */
export function previewUrl(project: string): string | null {
  const p = previews();
  return p.urls[project] ?? (p.template === null ? null : p.template.replace("{project}", project));
}
