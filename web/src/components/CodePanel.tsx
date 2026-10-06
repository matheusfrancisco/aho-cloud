// The Code / Files tab: a project workspace's files (what a deploy would ship) or a sandbox's
// `/workspace`, in `@aho/ui`'s `CodeView`. Refreshes on demand, shortly after aho touches
// something, and every 10 s while shown.
import { CodeView, type FileSource } from "@aho/ui";
import { type AhoTarget, api } from "../lib/api";

function workspaceFiles(target: AhoTarget): FileSource {
  return target.kind === "project"
    ? { id: `project:${target.project}`, list: () => api.workspaceFiles(target.project), read: (path) => api.workspaceFile(target.project, path) }
    : { id: `sandbox:${target.sandbox}`, list: () => api.sandboxFiles(target.sandbox), read: (path) => api.sandboxFile(target.sandbox, path) };
}

export function CodePanel({ target, refreshKey, intervalMs = 10_000 }: { target: AhoTarget; refreshKey: number; intervalMs?: number }) {
  return <CodeView source={workspaceFiles(target)} refreshKey={refreshKey} intervalMs={intervalMs} empty="No files yet. Ask aho to build something." />;
}
