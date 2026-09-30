import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter, Route, Routes } from "react-router";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Project, Release } from "../src/gen/api";
import type { Draft } from "../src/gen/contract";
import { api, ApiError } from "../src/lib/api";
import { DeployFailed, previewState, waitForPreview } from "../src/lib/deploy";
import { API_VERSION, draftToValue } from "../src/lib/draft";
import { signIn } from "../src/lib/session";
import { ProjectPage } from "../src/pages/ProjectPage";
import { release } from "./fixtures";

/** A socket that never opens: the chat stays "connecting", which these tests do not exercise. */
class SilentSocket {
  readyState = 0;
  onopen = null;
  onmessage = null;
  onclose = null;
  onerror = null;
  send() {}
  close() {}
}

const PREVIEW_URL = "http://shop.preview.localhost:8443";
const draft: Draft = {
  fields: {
    "/project": { value: "shop", strength: "declared", evidence: [], confirmed: true },
    "/source": { value: { kind: "archive", hash: "blake3:ws" }, strength: "chained", evidence: [], confirmed: false },
    "/build": { value: { kind: "dockerfile", path: "Dockerfile", context: "." }, strength: "inferred", evidence: [{ uri: "file:Dockerfile:1", note: "FROM node" }], confirmed: false },
    "/processes/0/name": { value: "web", strength: "declared", evidence: [], confirmed: true },
    "/processes/0/port": { value: 8080, strength: "inferred", evidence: [{ uri: "file:Dockerfile:4", note: "EXPOSE 8080" }], confirmed: false },
    "/processes/0/expose": { value: "public", strength: "declared", evidence: [], confirmed: true },
  },
};

let project: Project;
let releases: Release[];

beforeEach(() => {
  signIn({ token: "t", user: { id: "u-ana", email: "ana@example.com", name: "Ana" } });
  vi.stubGlobal("WebSocket", SilentSocket);
  project = { slug: "shop", org: "acme", active: [], domains: [] };
  vi.spyOn(api, "project").mockImplementation(async () => project);
  releases = [];
  vi.spyOn(api, "releases").mockImplementation(async () => releases);
  vi.spyOn(api, "ledger").mockResolvedValue([]);
  vi.spyOn(api, "secrets").mockResolvedValue([]);
  vi.spyOn(api, "startWorkspace").mockResolvedValue({ rpc_url: "ws://sessions.test/rpc/w1", token: "tok", session_id: null, preview_url: PREVIEW_URL });
  vi.spyOn(api, "workspaceFiles").mockResolvedValue({ files: [], truncated: false });
});
afterEach(() => {
  vi.unstubAllGlobals();
});

function renderPage() {
  render(
    <MemoryRouter initialEntries={["/projects/shop"]}>
      <Routes>
        <Route path="/projects/:p" element={<ProjectPage pollMs={5} />} />
      </Routes>
    </MemoryRouter>,
  );
}

describe("deploy to preview", () => {
  it("confirms a drafted manifest in a modal, resends it, shows refusals, then follows the release to preview", async () => {
    const user = userEvent.setup();
    const building = release({ id: "r-new", status: "building", stage: null, image_digest: null });
    const refusal = { code: "public_without_port" as const, field: "/processes/0/port", message: "public process has no port", fix: "set processes[0].port" };
    const deploy = vi
      .spyOn(api, "workspaceDeploy")
      .mockResolvedValueOnce({ status: "needs_manifest", draft })
      .mockRejectedValueOnce(new ApiError(422, "refused", "manifest refused: 1 refusal(s)", [refusal]))
      .mockResolvedValueOnce({ status: "proposed", release: building });
    const polled = vi
      .spyOn(api, "release")
      .mockResolvedValueOnce(building)
      .mockResolvedValueOnce(release({ id: "r-new", status: "awaiting_approval", stage: null }))
      .mockImplementation(async () => {
        const active = release({ id: "r-new", status: "active", stage: "preview" });
        project = { ...project, active: [{ stage: "preview", release: "r-new" }] };
        releases = [active];
        return active;
      });
    renderPage();

    const header = (await screen.findByRole("heading", { name: "shop" })).closest("header")!;
    expect(within(header).getByRole("button", { name: /Go live/ })).toBeDisabled();
    await user.click(within(header).getByRole("button", { name: /Deploy to preview/ }));
    expect(deploy).toHaveBeenNthCalledWith(1, "shop", undefined);

    const dialog = await screen.findByRole("dialog", { name: "Deploy to preview" });
    const submit = within(dialog).getByRole("button", { name: "Deploy to preview" });
    expect(submit).toBeDisabled();
    await user.click(within(dialog).getByRole("button", { name: "Confirm /build" }));
    await user.click(within(dialog).getByRole("button", { name: "Confirm /processes/0/port" }));
    // Nothing in the code decides auth, health, owner or budget: the defaults are offered.
    const required = within(dialog).getByLabelText("required fields");
    expect(within(required).getByLabelText("Owner email (who gets called when it breaks)")).toHaveValue("ana@example.com");
    await user.click(submit);
    const sent = deploy.mock.calls[1]![1] as unknown as Record<string, unknown>;
    expect(sent).toMatchObject({
      ...draftToValue(draft),
      api_version: API_VERSION,
      auth: { mode: "magic_link", allowed_domains: [] },
      data_class: "none",
      owner: { name: "Ana", email: "ana@example.com", escalation: [] },
      budget: { ceiling_usd_month: 20, degrade: "sleep" },
      processes: [{ name: "web", port: 8080, expose: "public", health: { kind: "http", path: "/", port: null } }],
    });
    // Refused: the refusals show in the still-open modal, confirmations kept.
    expect(await within(dialog).findByText("public process has no port")).toBeInTheDocument();
    expect(within(dialog).getByText(/Fix: set processes\[0\]\.port/)).toBeInTheDocument();
    expect(submit).toBeEnabled();

    await user.click(submit);
    expect(deploy).toHaveBeenNthCalledWith(3, "shop", sent);
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());

    expect(screen.getByRole("tab", { name: "Releases" })).toHaveAttribute("aria-selected", "true");
    const progress = screen.getByRole("region", { name: "Deploy progress" });
    expect(progress).toHaveTextContent("Deploying r-new to preview");
    const link = await within(progress).findByRole("link", { name: new RegExp(PREVIEW_URL.replaceAll(".", "\\.")) });
    expect(link).toHaveAttribute("href", PREVIEW_URL);
    expect(within(progress).getByText("Active at preview")).toBeInTheDocument();
    expect(polled.mock.calls.every(([p, r]) => p === "shop" && r === "r-new")).toBe(true);
    expect(polled.mock.calls.length).toBeGreaterThanOrEqual(3);
    // The project reloads once the release is up, which enables Go live.
    await waitFor(() => expect(within(header).getByRole("button", { name: /Go live/ })).toBeEnabled());
  });

  it("ships deploy.json directly and reports a failed build", async () => {
    const user = userEvent.setup();
    const building = release({ id: "r-bad", status: "building", stage: null, image_digest: null });
    vi.spyOn(api, "workspaceDeploy").mockResolvedValueOnce({ status: "proposed", release: building });
    const built = Promise.withResolvers<Release>();
    vi.spyOn(api, "release").mockReturnValue(built.promise);
    renderPage();
    await user.click(await screen.findByRole("button", { name: /Deploy to preview/ }));
    expect(screen.queryByRole("dialog")).toBeNull();
    const progress = await screen.findByRole("region", { name: "Deploy progress" });
    expect(within(progress).getByText("Building…")).toBeInTheDocument();
    built.resolve(release({ id: "r-bad", status: "build_failed", stage: null, image_digest: null }));
    expect(await within(progress).findByText("Build failed")).toBeInTheDocument();
    expect(within(progress).getByText(/The build failed\. Open the release for details/)).toBeInTheDocument();
    expect(within(progress).queryByRole("link", { name: /preview\.localhost/ })).toBeNull();
  });
});

describe("waitForPreview", () => {
  it("polls until the release is active at preview, reporting each state", async () => {
    const seen: string[] = [];
    const states: Release[] = [release({ status: "building", stage: null }), release({ status: "awaiting_approval", stage: null }), release({ status: "active", stage: "preview" })];
    const client = { release: vi.fn(async () => states.shift()!) };
    const done = await waitForPreview(client, "shop", release({ status: "building", stage: null }), { intervalMs: 1, onUpdate: (r) => seen.push(previewState(r)) });
    expect(done.status).toBe("active");
    expect(seen).toEqual(["building", "activating", "active"]);
    expect(client.release).toHaveBeenCalledTimes(3);
  });

  it("gives up when a built release never activates, and on terminal states", async () => {
    const stuck = release({ status: "awaiting_approval", stage: null });
    await expect(waitForPreview({ release: async () => stuck }, "shop", stuck, { intervalMs: 1, activationTimeoutMs: 5 })).rejects.toThrow(/did not become active at preview/);
    const tainted = release({ status: "tainted", stage: null });
    const err = await waitForPreview({ release: async () => tainted }, "shop", tainted, { intervalMs: 1 }).catch((e: unknown) => e);
    expect(err).toBeInstanceOf(DeployFailed);
    expect((err as DeployFailed).release).toBe(tainted);
  });

  it("stops when aborted", async () => {
    const ctrl = new AbortController();
    const client = { release: vi.fn(async () => release({ status: "building", stage: null })) };
    const run = waitForPreview(client, "shop", release({ status: "building", stage: null }), { intervalMs: 50, signal: ctrl.signal });
    ctrl.abort(new Error("left the page"));
    await expect(run).rejects.toThrow("left the page");
    expect(client.release).not.toHaveBeenCalled();
  });
});
