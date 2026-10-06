// The workspace's flat `FileTree` listing as a nested tree for the Code tab.
import type { FileEntry, FileKind } from "../gen/api";

export interface TreeNode {
  name: string;
  /** Full `/`-separated path from the workspace root. */
  path: string;
  kind: FileKind;
  size: number;
  /** Directories only; sorted. */
  children: TreeNode[];
}

/** Directories first, then by name ignoring case (exact name breaks ties), like an editor's explorer. */
function byExplorerOrder(a: TreeNode, b: TreeNode): number {
  if ((a.kind === "dir") !== (b.kind === "dir")) return a.kind === "dir" ? -1 : 1;
  const x = a.name.toLowerCase();
  const y = b.name.toLowerCase();
  if (x !== y) return x < y ? -1 : 1;
  return a.name < b.name ? -1 : a.name > b.name ? 1 : 0;
}

function sortDeep(nodes: TreeNode[]): TreeNode[] {
  nodes.sort(byExplorerOrder);
  for (const n of nodes) sortDeep(n.children);
  return nodes;
}

/**
 * Nests entries by path. Directories a path implies but the listing omits are created; an entry
 * listed twice keeps its first occurrence; empty or `.` segments are ignored.
 */
export function buildTree(files: readonly FileEntry[]): TreeNode[] {
  const root: TreeNode[] = [];
  const dirs = new Map<string, TreeNode>();
  const seen = new Set<string>();

  const dir = (path: string, name: string, siblings: TreeNode[]): TreeNode => {
    let node = dirs.get(path);
    if (node === undefined) {
      node = { name, path, kind: "dir", size: 0, children: [] };
      dirs.set(path, node);
      seen.add(path);
      siblings.push(node);
    }
    return node;
  };

  for (const f of files) {
    const segments = f.path.split("/").filter((s) => s !== "" && s !== ".");
    if (segments.length === 0) continue;
    let siblings = root;
    for (let i = 0; i < segments.length - 1; i++) {
      siblings = dir(segments.slice(0, i + 1).join("/"), segments[i]!, siblings).children;
    }
    const path = segments.join("/");
    const name = segments[segments.length - 1]!;
    if (f.kind === "dir") dir(path, name, siblings);
    else if (!seen.has(path)) {
      seen.add(path);
      siblings.push({ name, path, kind: f.kind, size: f.size, children: [] });
    }
  }
  return sortDeep(root);
}
