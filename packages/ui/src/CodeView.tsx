// Files as a collapsible tree and a read-only, line-numbered viewer, over any `FileSource`.
// Refreshes on demand, shortly after `refreshKey` changes, and every `intervalMs` while the page
// is visible.
import { type ReactNode, useEffect, useMemo, useState } from "react";
import { ErrorBox, formatBytes, Loading } from "./common";
import { buildTree, type FileEntry, type TreeNode } from "./filetree";
import { useLoad } from "./hooks";
import { ChevronDown, ChevronRight, FileIcon, FolderIcon, RefreshIcon } from "./icons";

/** A listing of the source's files. */
export interface FileListing {
  /** In path order. */
  files: FileEntry[];
  /** More entries exist than were listed. */
  truncated: boolean;
}

/** One file's contents. */
export interface FileContent {
  path: string;
  size: number;
  /** UTF-8 text, or `null` for binary files. */
  text: string | null;
  /** Only the first part of a large file is in `text`. */
  truncated: boolean;
}

/** Where `CodeView` reads files from. */
export interface FileSource {
  /** Identifies what is listed; a change reloads the tree and the open file. */
  readonly id: string;
  list(): Promise<FileListing>;
  read(path: string): Promise<FileContent>;
}

/** A text file split into lines; the newline ending the last line does not start another. */
function lines(text: string): string[] {
  const all = text.split("\n");
  if (all.length > 1 && all[all.length - 1] === "") all.pop();
  return all;
}

export function FileViewer({ file }: { file: FileContent }) {
  return (
    <div className="viewer" role="region" aria-label={`contents of ${file.path}`}>
      <div className="viewer-head">
        <code>{file.path}</code>
        <span className="muted small">{formatBytes(file.size)}</span>
      </div>
      {file.text === null ? (
        <p className="note">Binary file — not shown.</p>
      ) : (
        <>
          {file.truncated && <p className="note">Truncated: this file is {formatBytes(file.size)}; only its beginning is shown.</p>}
          <div className="code-scroll">
            <table className="code-lines">
              <tbody>
                {lines(file.text).map((line, i) => (
                  <tr key={i}>
                    <td className="ln">{i + 1}</td>
                    <td className="src">{line}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </>
      )}
    </div>
  );
}

function Tree({ nodes, depth, open, selected, onToggle, onSelect }: { nodes: TreeNode[]; depth: number; open: ReadonlySet<string>; selected: string | null; onToggle: (path: string) => void; onSelect: (path: string) => void }) {
  return (
    <ul className="tree">
      {nodes.map((n) =>
        n.kind === "dir" ? (
          <li key={n.path}>
            <button type="button" className="tree-row" aria-expanded={open.has(n.path)} style={{ paddingLeft: `${0.5 + depth * 0.9}rem` }} onClick={() => onToggle(n.path)}>
              {open.has(n.path) ? <ChevronDown /> : <ChevronRight />}
              <FolderIcon />
              <span>{n.name}</span>
            </button>
            {open.has(n.path) && <Tree nodes={n.children} depth={depth + 1} open={open} selected={selected} onToggle={onToggle} onSelect={onSelect} />}
          </li>
        ) : (
          <li key={n.path}>
            <button type="button" className={`tree-row file${selected === n.path ? " selected" : ""}`} aria-current={selected === n.path ? "true" : undefined} style={{ paddingLeft: `${0.5 + depth * 0.9 + 0.9}rem` }} onClick={() => onSelect(n.path)}>
              <FileIcon />
              <span>{n.name}</span>
              {n.kind === "symlink" && <span className="muted small">symlink</span>}
            </button>
          </li>
        ),
      )}
    </ul>
  );
}

/**
 * `source` is read through its `id`: a new object with the same `id` does not reload.
 * `refreshKey`: bump it when the files probably changed (e.g. an agent touched them); one refresh
 * follows 800 ms after the last bump. `empty`: shown when the source lists no files.
 */
export function CodeView({ source, refreshKey = 0, intervalMs = 10_000, empty = "No files." }: { source: FileSource; refreshKey?: number; intervalMs?: number; empty?: ReactNode }) {
  const tree = useLoad(() => source.list(), [source.id]);
  const [open, setOpen] = useState<ReadonlySet<string>>(new Set());
  const [selected, setSelected] = useState<string | null>(null);
  const nodes = useMemo(() => (tree.data === undefined ? [] : buildTree(tree.data.files)), [tree.data]);
  const exists = selected !== null && tree.data !== undefined && tree.data.files.some((f) => f.path === selected);
  // Re-read the open file whenever the listing refreshes: it may have just been rewritten.
  const file = useLoad(() => (selected === null || !exists ? Promise.resolve(null) : source.read(selected)), [source.id, selected, exists, tree.data]);
  const { reload } = tree;

  // Activity arrives in bursts; one refresh once it settles.
  useEffect(() => {
    if (refreshKey === 0) return;
    const handle = setTimeout(reload, 800);
    return () => clearTimeout(handle);
  }, [refreshKey, reload]);

  useEffect(() => {
    const handle = setInterval(() => {
      if (document.visibilityState === "visible") reload();
    }, intervalMs);
    return () => clearInterval(handle);
  }, [intervalMs, reload]);

  const toggle = (path: string) =>
    setOpen((o) => {
      const next = new Set(o);
      if (!next.delete(path)) next.add(path);
      return next;
    });

  return (
    <div className="code-panel">
      <div className="code-toolbar">
        <span className="muted small grow">{tree.data === undefined ? "" : `${tree.data.files.filter((f) => f.kind !== "dir").length} files`}</span>
        <button type="button" className="secondary small" onClick={reload} disabled={tree.loading} aria-label="Refresh files">
          <RefreshIcon /> {tree.loading ? "Refreshing…" : "Refresh"}
        </button>
      </div>
      <ErrorBox error={tree.error} />
      {tree.data === undefined ? (
        tree.loading && <Loading what="files" />
      ) : (
        <div className="code-split">
          <nav className="tree-pane" aria-label="Files">
            {nodes.length === 0 ? <p className="muted pad">{empty}</p> : <Tree nodes={nodes} depth={0} open={open} selected={selected} onToggle={toggle} onSelect={setSelected} />}
            {tree.data.truncated && <p className="note">More files exist than are listed here.</p>}
          </nav>
          <div className="viewer-pane">
            {selected === null ? (
              <p className="muted pad">Select a file to view it.</p>
            ) : !exists ? (
              <p className="muted pad">
                <code>{selected}</code> is no longer in the workspace.
              </p>
            ) : (
              <>
                <ErrorBox error={file.error} />
                {/* The previous file stays loaded while the next one is fetched; show it only when it is the selected one. */}
                {file.data !== undefined && file.data !== null && (file.data.path === selected || !file.loading) ? <FileViewer file={file.data} /> : file.loading && <Loading what={selected} />}
              </>
            )}
          </div>
        </div>
      )}
    </div>
  );
}
