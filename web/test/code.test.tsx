import { act, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { CodePanel, FileViewer } from "../src/components/CodePanel";
import type { FileContent, FileEntry } from "../src/gen/api";
import { api } from "../src/lib/api";
import { buildTree, type TreeNode } from "../src/lib/filetree";

const file = (path: string, size = 10): FileEntry => ({ path, kind: "file", size });
const dir = (path: string): FileEntry => ({ path, kind: "dir", size: 0 });

/** `name/` for directories, children indented: the tree as text. */
function outline(nodes: TreeNode[], depth = 0): string[] {
  return nodes.flatMap((n) => [`${"  ".repeat(depth)}${n.name}${n.kind === "dir" ? "/" : ""}`, ...outline(n.children, depth + 1)]);
}

describe("buildTree", () => {
  it("nests paths, creating the directories they imply, directories first then by name ignoring case", () => {
    const tree = buildTree([file("src/main.rs"), file("README.md"), file("src/lib/util.rs"), file("Cargo.toml"), dir("src"), file("assets/logo.png"), dir("empty"), file("src/Api.rs"), file("a.txt")]);
    expect(outline(tree)).toEqual(["assets/", "  logo.png", "empty/", "src/", "  lib/", "    util.rs", "  Api.rs", "  main.rs", "a.txt", "Cargo.toml", "README.md"]);
    const src = tree.find((n) => n.name === "src")!;
    expect(src.path).toBe("src");
    expect(src.children.find((n) => n.name === "lib")!.children[0]).toEqual({ name: "util.rs", path: "src/lib/util.rs", kind: "file", size: 10, children: [] });
  });

  it("keeps one node per path and ignores empty and `.` segments", () => {
    const tree = buildTree([file("a/b.txt", 1), file("a/b.txt", 2), file("./a//c.txt"), file(""), { path: "link", kind: "symlink", size: 4 }]);
    expect(outline(tree)).toEqual(["a/", "  b.txt", "  c.txt", "link"]);
    expect(tree[0]!.children[0]!.size).toBe(1);
    expect(tree[0]!.children[1]!.path).toBe("a/c.txt");
    expect(tree[1]!.kind).toBe("symlink");
  });
});

describe("file viewer", () => {
  it("numbers every line and does not count the final newline as a line", () => {
    render(<FileViewer file={{ path: "src/main.rs", size: 30, text: "fn main() {\n    run();\n}\n", truncated: false }} />);
    const view = screen.getByRole("region", { name: "contents of src/main.rs" });
    const rows = within(view).getAllByRole("row").map((r) => [...r.querySelectorAll("td")].map((td) => td.textContent));
    expect(rows).toEqual([
      ["1", "fn main() {"],
      ["2", "    run();"],
      ["3", "}"],
    ]);
    expect(within(view).queryByText(/Truncated/)).toBeNull();
    expect(within(view).queryByText(/Binary/)).toBeNull();
  });

  it("says a binary file is not shown and a truncated one shows only its beginning", () => {
    const { rerender } = render(<FileViewer file={{ path: "logo.png", size: 2048, text: null, truncated: false }} />);
    expect(screen.getByText("Binary file — not shown.")).toBeInTheDocument();
    expect(screen.queryAllByRole("row")).toHaveLength(0);
    rerender(<FileViewer file={{ path: "big.log", size: 5 * 1024 * 1024, text: "first\nsecond", truncated: true }} />);
    expect(screen.getByText("Truncated: this file is 5.0 MB; only its beginning is shown.")).toBeInTheDocument();
    expect(screen.getAllByRole("row")).toHaveLength(2);
  });
});

describe("code panel", () => {
  const contents: Record<string, FileContent> = {
    "src/main.rs": { path: "src/main.rs", size: 12, text: "fn main() {}", truncated: false },
    "assets/logo.png": { path: "assets/logo.png", size: 900, text: null, truncated: false },
  };

  it("lists the workspace tree with the truncated notice, expands directories and opens files", async () => {
    const user = userEvent.setup();
    const files = vi.spyOn(api, "workspaceFiles").mockResolvedValue({ files: [file("assets/logo.png"), file("src/main.rs"), file("README.md")], truncated: true });
    const read = vi.spyOn(api, "workspaceFile").mockImplementation(async (_p, path) => contents[path]!);
    render(<CodePanel project="shop" refreshKey={0} />);

    expect(await screen.findByText("More files exist than are listed here.")).toBeInTheDocument();
    expect(files).toHaveBeenCalledWith("shop");
    const nav = screen.getByRole("navigation", { name: "Files" });
    // Collapsed at first: only the top level shows.
    expect(within(nav).getAllByRole("button").map((b) => b.textContent)).toEqual(["assets", "src", "README.md"]);
    const src = within(nav).getByRole("button", { name: "src" });
    expect(src).toHaveAttribute("aria-expanded", "false");
    await user.click(src);
    expect(src).toHaveAttribute("aria-expanded", "true");
    await user.click(within(nav).getByRole("button", { name: "main.rs" }));
    expect(read).toHaveBeenCalledWith("shop", "src/main.rs");
    const view = await screen.findByRole("region", { name: "contents of src/main.rs" });
    expect(within(view).getByText("fn main() {}")).toBeInTheDocument();
    expect(within(nav).getByRole("button", { name: "main.rs" })).toHaveAttribute("aria-current", "true");

    await user.click(within(nav).getByRole("button", { name: "assets" }));
    await user.click(within(nav).getByRole("button", { name: "logo.png" }));
    expect(await screen.findByText("Binary file — not shown.")).toBeInTheDocument();

    // Collapsing hides the children again.
    await user.click(src);
    expect(within(nav).queryByRole("button", { name: "main.rs" })).toBeNull();
  });

  it("refreshes once, shortly after a burst of agent activity", async () => {
    const files = vi.spyOn(api, "workspaceFiles").mockResolvedValue({ files: [file("a.txt")], truncated: false });
    const { rerender } = render(<CodePanel project="shop" refreshKey={0} />);
    await screen.findByRole("button", { name: "a.txt" });
    expect(files).toHaveBeenCalledTimes(1);
    vi.useFakeTimers();
    try {
      for (let k = 1; k <= 3; k++) {
        rerender(<CodePanel project="shop" refreshKey={k} />);
        await act(() => vi.advanceTimersByTimeAsync(300));
      }
      expect(files).toHaveBeenCalledTimes(1);
      await act(() => vi.advanceTimersByTimeAsync(800));
      expect(files).toHaveBeenCalledTimes(2);
    } finally {
      vi.useRealTimers();
    }
  });

  it("says when the workspace has no files yet", async () => {
    vi.spyOn(api, "workspaceFiles").mockResolvedValue({ files: [], truncated: false });
    render(<CodePanel project="shop" refreshKey={0} />);
    expect(await screen.findByText("No files yet. Ask aho to build something.")).toBeInTheDocument();
  });
});
