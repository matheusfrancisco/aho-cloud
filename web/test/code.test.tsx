import { act, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { CodePanel } from "../src/components/CodePanel";
import type { FileContent, FileEntry } from "../src/gen/api";
import { api } from "../src/lib/api";

const file = (path: string, size = 10): FileEntry => ({ path, kind: "file", size });

describe("code panel", () => {
  const contents: Record<string, FileContent> = {
    "src/main.rs": { path: "src/main.rs", size: 12, text: "fn main() {}", truncated: false },
    "assets/logo.png": { path: "assets/logo.png", size: 900, text: null, truncated: false },
  };

  it("lists the workspace tree with the truncated notice, expands directories and opens files", async () => {
    const user = userEvent.setup();
    const files = vi.spyOn(api, "workspaceFiles").mockResolvedValue({ files: [file("assets/logo.png"), file("src/main.rs"), file("README.md")], truncated: true });
    const read = vi.spyOn(api, "workspaceFile").mockImplementation(async (_p, path) => contents[path]!);
    render(<CodePanel target={{ kind: "project", project: "shop" }} refreshKey={0} />);

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
    const { rerender } = render(<CodePanel target={{ kind: "project", project: "shop" }} refreshKey={0} />);
    await screen.findByRole("button", { name: "a.txt" });
    expect(files).toHaveBeenCalledTimes(1);
    vi.useFakeTimers();
    try {
      for (let k = 1; k <= 3; k++) {
        rerender(<CodePanel target={{ kind: "project", project: "shop" }} refreshKey={k} />);
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
    render(<CodePanel target={{ kind: "project", project: "shop" }} refreshKey={0} />);
    expect(await screen.findByText("No files yet. Ask aho to build something.")).toBeInTheDocument();
  });
});
