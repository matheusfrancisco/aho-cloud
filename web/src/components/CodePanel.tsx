// The Code tab: the workspace files a deploy would ship, as a collapsible tree, and a read-only
// viewer. Refreshes on demand, shortly after aho touches something, and every 10 s while shown.
import { useEffect, useMemo, useState } from "react";
import type { FileContent } from "../gen/api";
import { api } from "../lib/api";
import { buildTree, type TreeNode } from "../lib/filetree";
import { ErrorBox, formatBytes, Loading } from "../ui/common";
import { useLoad } from "../ui/hooks";
import { ChevronDown, ChevronRight, FileIcon, FolderIcon, RefreshIcon } from "../ui/icons";

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

export function CodePanel({ project, refreshKey, intervalMs = 10_000 }: { project: string; refreshKey: number; intervalMs?: number }) {
  const tree = useLoad(() => api.workspaceFiles(project), [project]);
  const [open, setOpen] = useState<ReadonlySet<string>>(new Set());
  const [selected, setSelected] = useState<string | null>(null);
  const nodes = useMemo(() => (tree.data === undefined ? [] : buildTree(tree.data.files)), [tree.data]);
  const exists = selected !== null && tree.data !== undefined && tree.data.files.some((f) => f.path === selected);
  // Re-read the open file whenever the listing refreshes: aho may have just rewritten it.
  const file = useLoad(() => (selected !== null && exists ? api.workspaceFile(project, selected) : Promise.resolve(null)), [project, selected, exists, tree.data]);
  const { reload } = tree;

  // aho activity arrives in bursts; one refresh once it settles.
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
            {nodes.length === 0 ? (
              <p className="muted pad">No files yet. Ask aho to build something.</p>
            ) : (
              <Tree nodes={nodes} depth={0} open={open} selected={selected} onToggle={toggle} onSelect={setSelected} />
            )}
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
