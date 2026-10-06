import { act, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { CodeView, type FileContent, FileViewer, type FileListing, type FileSource } from "../src/CodeView";
import { buildTree, type FileEntry, type TreeNode } from "../src/filetree";

const file = (path: string, size = 10): FileEntry => ({ path, kind: "file", size });
const dir = (path: string): FileEntry => ({ path, kind: "dir", size: 0 });

/** `name/` for directories, children indented: the tree as text. */
function outline(nodes: TreeNode[], depth = 0): string[] {
  return nodes.flatMap((n) => [`${"  ".repeat(depth)}${n.name}${n.kind === "dir" ? "/" : ""}`, ...outline(n.children, depth + 1)]);
}

/** A source over fixed files that records every call. */
function fakeSource(id: string, listing: FileListing, contents: Record<string, FileContent>) {
  const list = vi.fn(async () => listing);
  const read = vi.fn(async (path: string) => {
    const c = contents[path];
    if (c === undefined) throw new Error(`${path}: not found`);
    return c;
  });
  const source: FileSource = { id, list, read };
  return { source, list, read };
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

describe("code view", () => {
  const contents: Record<string, FileContent> = {
    "src/main.rs": { path: "src/main.rs", size: 25, text: "fn main() {\n    run();\n}\n", truncated: false },
    "assets/logo.png": { path: "assets/logo.png", size: 900, text: null, truncated: false },
    "big.log": { path: "big.log", size: 3 * 1024 * 1024, text: "first\nsecond\n", truncated: true },
  };
  const listing: FileListing = { files: [dir("assets"), file("assets/logo.png"), file("big.log"), dir("src"), file("src/main.rs")], truncated: false };

  it("loads a selected file through the source and marks binary and truncated files", async () => {
    const user = userEvent.setup();
    const { source, list, read } = fakeSource("repo", listing, contents);
    render(<CodeView source={source} />);
    const nav = await screen.findByRole("navigation", { name: "Files" });
    expect(list).toHaveBeenCalledTimes(1);
    expect(screen.getByText("3 files")).toBeInTheDocument();
    expect(screen.getByText("Select a file to view it.")).toBeInTheDocument();
    expect(read).not.toHaveBeenCalled();

    await user.click(within(nav).getByRole("button", { name: "src" }));
    await user.click(within(nav).getByRole("button", { name: "main.rs" }));
    expect(read).toHaveBeenLastCalledWith("src/main.rs");
    const view = await screen.findByRole("region", { name: "contents of src/main.rs" });
    expect(within(view).getAllByRole("row").map((r) => r.textContent)).toEqual(["1fn main() {", "2    run();", "3}"]);

    await user.click(within(nav).getByRole("button", { name: "big.log" }));
    expect(await screen.findByText("Truncated: this file is 3.0 MB; only its beginning is shown.")).toBeInTheDocument();
    expect(read).toHaveBeenLastCalledWith("big.log");

    await user.click(within(nav).getByRole("button", { name: "assets" }));
    await user.click(within(nav).getByRole("button", { name: "logo.png" }));
    expect(await screen.findByText("Binary file — not shown.")).toBeInTheDocument();
    expect(screen.queryAllByRole("row")).toHaveLength(0);
  });

  it("shows the source's error when a file cannot be read, and the empty text for an empty listing", async () => {
    const user = userEvent.setup();
    const broken = fakeSource("repo", { files: [file("gone.txt")], truncated: true }, {});
    const { unmount } = render(<CodeView source={broken.source} />);
    await user.click(await screen.findByRole("button", { name: "gone.txt" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("gone.txt: not found");
    expect(screen.getByText("More files exist than are listed here.")).toBeInTheDocument();
    unmount();

    const empty = fakeSource("blank", { files: [], truncated: false }, {});
    render(<CodeView source={empty.source} empty="Nothing indexed yet." />);
    expect(await screen.findByText("Nothing indexed yet.")).toBeInTheDocument();
  });

  it("reloads when the source id changes, not when an equal source is passed again", async () => {
    const a = fakeSource("a", { files: [file("a.txt")], truncated: false }, {});
    const { rerender } = render(<CodeView source={a.source} />);
    await screen.findByRole("button", { name: "a.txt" });
    rerender(<CodeView source={{ ...a.source }} />);
    expect(a.list).toHaveBeenCalledTimes(1);

    const b = fakeSource("b", { files: [file("b.txt")], truncated: false }, {});
    rerender(<CodeView source={b.source} />);
    expect(await screen.findByRole("button", { name: "b.txt" })).toBeInTheDocument();
    expect(b.list).toHaveBeenCalledTimes(1);
  });

  it("refreshes once, 800 ms after the last bump of refreshKey", async () => {
    const { source, list } = fakeSource("repo", { files: [file("a.txt")], truncated: false }, {});
    const { rerender } = render(<CodeView source={source} refreshKey={0} />);
    await screen.findByRole("button", { name: "a.txt" });
    vi.useFakeTimers();
    try {
      for (let k = 1; k <= 3; k++) {
        rerender(<CodeView source={source} refreshKey={k} />);
        await act(() => vi.advanceTimersByTimeAsync(300));
      }
      expect(list).toHaveBeenCalledTimes(1);
      await act(() => vi.advanceTimersByTimeAsync(800));
      expect(list).toHaveBeenCalledTimes(2);
    } finally {
      vi.useRealTimers();
    }
  });
});
