import { fireEvent, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router";
import { describe, expect, it, vi } from "vitest";
import { App } from "../src/App";
import type { LedgerEntry, Project, Release, Sandbox } from "../src/gen/api";
import { api } from "../src/lib/api";
import { signIn } from "../src/lib/session";
import { filterRows, projectRow, projectStatus, recentActivity, statusCounts } from "../src/lib/status";
import { release } from "./fixtures";

const ana = { id: "u-ana", email: "ana@example.com", name: "Ana" };

function project(slug: string, over: Partial<Project> = {}): Project {
  return { slug, org: "acme", active: [], domains: [], ...over };
}

const shop = project("shop", {
  active: [
    { stage: "preview", release: "r-3" },
    { stage: "live", release: "r-2" },
  ],
  domains: [{ hostname: "shop.example.org", mode: "cname", state: "verified", records: [] }],
});
const blog = project("blog", { active: [{ stage: "preview", release: "r-b1" }] });
const staged = project("staged", { active: [{ stage: "staging", release: "r-s1" }] });
const apiProject = project("api");
const docs = project("docs");
const empty = project("empty");

const releasesOf: Record<string, Release[]> = {
  shop: [release({ id: "r-3", status: "building", stage: null }), release({ id: "r-2", status: "active", stage: "live" })],
  blog: [release({ id: "r-b1", status: "active", stage: "preview" })],
  staged: [release({ id: "r-s1", status: "active", stage: "staging" })],
  api: [release({ id: "r-a2", status: "building", stage: null }), release({ id: "r-a1", status: "build_failed", stage: null })],
  docs: [release({ id: "r-d1", status: "build_failed", stage: null })],
  empty: [],
};

function entry(id: number, at: number, r: string): LedgerEntry {
  return { id, hash: `h${id}`, at, body: { kind: "image_built", release: r, image_digest: `sha256:${id}` } };
}

describe("project status", () => {
  it("is Live when anything is live, even while a newer release builds", () => {
    expect(projectStatus(shop, releasesOf.shop![0]!)).toBe("live");
  });

  it("is Preview for a release at preview or staging only", () => {
    expect(projectStatus(blog, releasesOf.blog![0]!)).toBe("preview");
    expect(projectStatus(staged, releasesOf.staged![0]!)).toBe("preview");
  });

  it("is Building only when nothing runs and the newest release is building", () => {
    expect(projectRow(apiProject, releasesOf.api!).status).toBe("building");
    // The newest release decides: an older building one does not count.
    expect(projectRow(docs, [release({ status: "build_failed", stage: null }), release({ status: "building", stage: null })]).status).toBe("idle");
  });

  it("is Idle without releases or after a failed build", () => {
    expect(projectRow(empty, []).status).toBe("idle");
    expect(projectRow(docs, releasesOf.docs!).status).toBe("idle");
  });

  it("counts one status per project for the chips", () => {
    const rows = [shop, blog, staged, apiProject, docs, empty].map((p) => projectRow(p, releasesOf[p.slug]!));
    expect(statusCounts(rows)).toEqual({ live: 1, preview: 2, building: 1, idle: 2 });
    expect(statusCounts([])).toEqual({ live: 0, preview: 0, building: 0, idle: 0 });
  });

  it("filters by slug or domain, ignoring case and surrounding space, within the chosen status", () => {
    const rows = [shop, blog, apiProject, docs].map((p) => projectRow(p, releasesOf[p.slug]!));
    const slugs = (q: string, s: Parameters<typeof filterRows>[2] = null) => filterRows(rows, q, s).map((r) => r.project.slug);
    expect(slugs("")).toEqual(["shop", "blog", "api", "docs"]);
    expect(slugs("  BL ")).toEqual(["blog"]);
    expect(slugs("example.org")).toEqual(["shop"]);
    expect(slugs("", "idle")).toEqual(["docs"]);
    expect(slugs("o", "preview")).toEqual(["blog"]);
    expect(slugs("zzz")).toEqual([]);
  });

  it("keeps the newest ten ledger entries across projects", () => {
    const shopLedger = Array.from({ length: 8 }, (_, i) => entry(i + 1, 1000 + i * 10, "r-2"));
    const blogLedger = Array.from({ length: 5 }, (_, i) => entry(i + 1, 1005 + i * 10, "r-b1"));
    const feed = recentActivity(
      [
        { project: "shop", entries: shopLedger },
        { project: "blog", entries: blogLedger },
      ],
      10,
    );
    expect(feed).toHaveLength(10);
    expect(feed.map((a) => a.entry.at)).toEqual([1070, 1060, 1050, 1045, 1040, 1035, 1030, 1025, 1020, 1015]);
    expect(feed[0]).toEqual({ project: "shop", entry: shopLedger[7] });
    expect(feed[3]!.project).toBe("blog");
  });
});

describe("org dashboard", () => {
  function sandbox(slug: string, state: Sandbox["state"]): Sandbox {
    const settings = { egress: [], port: null, auto_stop_hours: 8 };
    return { slug, org: "acme", state, created_by: ana, created_at: 0, image: "aho.sandbox@sha256:aa", settings, url: null, stops_at: null, cpus: 2, mem_mib: 2048, disk_mib: 10240, error: null };
  }

  function mockOrg(projects: Project[], sandboxes: Sandbox[] = []) {
    signIn({ token: "t", user: ana });
    vi.spyOn(api, "me").mockResolvedValue({ user: ana, orgs: [{ org: "acme", owner: true }] });
    vi.spyOn(api, "projects").mockResolvedValue(projects);
    vi.spyOn(api, "sandboxes").mockResolvedValue(sandboxes);
    vi.spyOn(api, "releases").mockImplementation(async (p) => releasesOf[p] ?? []);
    vi.spyOn(api, "ledger").mockImplementation(async (p) => (p === "shop" ? [entry(1, Date.now() - 60_000, "r-2")] : []));
    return render(
      <MemoryRouter initialEntries={["/orgs/acme"]}>
        <App />
      </MemoryRouter>,
    );
  }

  const chip = (name: RegExp) => within(screen.getByRole("group", { name: "Filter by status" })).getByRole("button", { name });
  const rowSlugs = () => within(screen.getByRole("region", { name: "Projects" })).queryAllByRole("listitem").map((li) => li.querySelector(".project-name")?.textContent);

  it("shows status chips with counts, filters by search and chip, and focuses search on /", async () => {
    const user = userEvent.setup();
    mockOrg([shop, blog, apiProject, docs, empty], [sandbox("scratch", "running"), sandbox("old", "stopped")]);
    expect(await screen.findByRole("link", { name: "blog" })).toBeInTheDocument();
    expect(chip(/^Live/)).toHaveTextContent("Live 1");
    expect(chip(/^Preview/)).toHaveTextContent("Preview 1");
    expect(chip(/^Building/)).toHaveTextContent("Building 1");
    expect(chip(/^Idle/)).toHaveTextContent("Idle 2");
    expect(rowSlugs()).toEqual(["shop", "blog", "api", "docs", "empty"]);

    const search = screen.getByRole("searchbox", { name: "Search projects" });
    fireEvent.keyDown(document.body, { key: "/" });
    expect(search).toHaveFocus();
    await user.type(search, "bl");
    expect(rowSlugs()).toEqual(["blog"]);
    // Typing `/` inside the search box types it; it does not steal focus or clear anything.
    await user.type(search, "/");
    expect(search).toHaveValue("bl/");
    await user.clear(search);

    await user.click(chip(/^Idle/));
    expect(chip(/^Idle/)).toHaveAttribute("aria-pressed", "true");
    expect(rowSlugs()).toEqual(["docs", "empty"]);
    await user.click(chip(/^Idle/));
    expect(rowSlugs()).toHaveLength(5);

    const activity = screen.getByRole("region", { name: "Recent activity" });
    expect(within(activity).getByText(/built/)).toBeInTheDocument();
    const terminal = screen.getByRole("region", { name: "Deploy from your terminal" });
    expect(terminal).toHaveTextContent("ahocloud -p shop deploy");

    // Sandboxes sit beside the projects: a count, each with its state, and the full list.
    const sandboxes = screen.getByRole("region", { name: "Sandboxes" });
    expect(within(sandboxes).getByText("2")).toBeInTheDocument();
    expect(within(sandboxes).getAllByRole("listitem").map((li) => li.textContent)).toEqual(["scratchrunning", "oldstopped"]);
    expect(within(sandboxes).getByRole("link", { name: "scratch" })).toHaveAttribute("href", "/sandboxes/scratch");
    expect(within(sandboxes).getByRole("link", { name: "View all" })).toHaveAttribute("href", "/orgs/acme/sandboxes");
  });

  it("shows the empty state with a create button when the org has no projects", async () => {
    const user = userEvent.setup();
    mockOrg([]);
    expect(await screen.findByText("You don't have any projects yet")).toBeInTheDocument();
    expect(screen.getByRole("region", { name: "Deploy from your terminal" })).toHaveTextContent("ahocloud -p <project> deploy");
    expect(await within(screen.getByRole("region", { name: "Sandboxes" })).findByText(/A sandbox is a VM with aho/)).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: /Create a project/ }));
    expect(screen.getByRole("dialog", { name: "New project" })).toBeInTheDocument();
  });
});
