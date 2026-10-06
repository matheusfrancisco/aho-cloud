// Recipe (`.mage/env.toml`) viewer and editor. The viewer is `CodeView` over the current recipe and
// its history; the editor saves Drafts (server: parse, normalize, hash, secret scan) and shows the
// validation report, plus the diff against the Learn run's proposal when there is one.
import { CodeView, type FileContent, type FileSource, useAction } from "@aho/ui";
import { useMemo, useState } from "react";
import type { OrgApi } from "../api/client";
import type { Recipe, RecipeIssue, RecipeReport, RepoId, SavedRecipe } from "../gen/mage";
import { lineDiff } from "../lib/lineDiff";
import { MageError } from "./common";

export const RECIPE_PATH = ".mage/env.toml";

/** `CodeView` source: the current recipe at `.mage/env.toml`, earlier ones under `history/`. */
export function recipeSource(repo: RepoId, current: Recipe | null, history: readonly Recipe[]): FileSource {
  const files = new Map<string, string>();
  if (current !== null) files.set(RECIPE_PATH, current.body);
  history.forEach((r, i) => files.set(`history/${String(history.length - i).padStart(3, "0")}-${r.hash.slice(0, 12)}.toml`, r.body));
  const bytes = (text: string) => new TextEncoder().encode(text).length;
  return {
    id: `recipes:${repo}:${current?.hash ?? ""}:${history.map((r) => r.hash).join(",")}`,
    list: async () => ({ files: [...files].map(([path, text]) => ({ path, kind: "file" as const, size: bytes(text) })), truncated: false }),
    read: async (path): Promise<FileContent> => {
      const text = files.get(path);
      if (text === undefined) throw new Error(`${path} is not a recipe`);
      return { path, size: bytes(text), text, truncated: false };
    },
  };
}

export function RecipeViewer({ repo, current, history }: { repo: RepoId; current: Recipe | null; history: readonly Recipe[] }) {
  const source = useMemo(() => recipeSource(repo, current, history), [repo, current, history]);
  return <CodeView source={source} intervalMs={3_600_000} empty="No recipe yet." />;
}

function Issues({ title, tone, issues }: { title: string; tone: "error" | "note"; issues: readonly RecipeIssue[] }) {
  if (issues.length === 0) return null;
  return (
    <div className={tone} role={tone === "error" ? "alert" : undefined}>
      <strong>{title}</strong>
      <ul>
        {issues.map((i, n) => (
          <li key={n}>
            {i.path !== "" && <code>{i.path}</code>} {i.message} <span className="muted small">({i.code})</span>
          </li>
        ))}
      </ul>
    </div>
  );
}

export function RecipeReportView({ report }: { report: RecipeReport }) {
  return (
    <>
      <Issues title="Errors (block the Boot gate)" tone="error" issues={report.errors} />
      <Issues title="Warnings" tone="note" issues={report.warnings} />
    </>
  );
}

/** Removed and added lines from `before` to `after`. */
export function RecipeDiff({ before, after }: { before: string; after: string }) {
  const lines = useMemo(() => lineDiff(before, after), [before, after]);
  return (
    <pre className="diff" aria-label="Changes from the proposal">
      {lines.map((l, i) => (
        <div key={i} className={l.op === "+" ? "add" : l.op === "-" ? "del" : ""}>
          {l.op}
          {l.text}
        </div>
      ))}
    </pre>
  );
}

/** Edits a recipe; `onSaved` gets the stored Draft when the report has no errors. */
export function RecipeEditor({ api, repo, initial, proposal = null, saveLabel = "Save draft", onSaved }: { api: OrgApi; repo: RepoId; initial: string; proposal?: string | null; saveLabel?: string; onSaved: (saved: SavedRecipe) => void }) {
  const [text, setText] = useState(initial);
  const [report, setReport] = useState<RecipeReport | null>(null);
  const save = useAction(async () => {
    const saved = await api.saveRecipe(repo, { toml: text });
    setReport(saved.report);
    if (saved.report.errors.length === 0) onSaved(saved);
  });
  return (
    <form
      className="stack"
      onSubmit={(e) => {
        e.preventDefault();
        void save.run();
      }}
    >
      <label>
        {RECIPE_PATH}
        <textarea
          className="mage-mono"
          rows={20}
          spellCheck={false}
          value={text}
          onChange={(e) => {
            setText(e.target.value);
            setReport(null);
          }}
        />
      </label>
      {proposal !== null && text !== proposal && (
        <details open>
          <summary>Changes from the proposal</summary>
          <RecipeDiff before={proposal} after={text} />
        </details>
      )}
      {report !== null && <RecipeReportView report={report} />}
      <div className="row">
        <button type="submit" disabled={save.busy || text.trim() === ""}>
          {save.busy ? "Saving…" : saveLabel}
        </button>
      </div>
      <MageError error={save.error} />
    </form>
  );
}
