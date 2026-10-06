import { act, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { ArtifactView, RunEvent, RunEventPage } from "../src/gen/mage";
import { event, runDetail, text } from "./fixtures";
import { fakeEventSources, fakeServer, renderMage } from "./helpers";

const RUN = "/api/v1/orgs/acme/runs/run_1";

function artifact(over: Partial<ArtifactView>): ArtifactView {
  return { id: "art_1", run_id: "run_1", kind: "log", name: "runner.log", content_type: "text/plain", bytes: 12, sha256: "00", complete: true, created_at: "2026-10-01T12:00:00Z", ...over };
}

function setup(pages: RunEventPage[], over: Parameters<typeof runDetail>[0] = {}, artifacts: ArtifactView[] = []) {
  const events = fakeEventSources();
  let page = 0;
  const server = fakeServer({
    [`GET ${RUN}`]: () => runDetail(over),
    [`GET ${RUN}/events`]: () => pages[page++] ?? { events: [], more: false },
    [`GET ${RUN}/artifacts`]: () => ({ artifacts }),
    [`POST ${RUN}/steer`]: () => ({ seq: 99 }),
    [`POST ${RUN}/cancel`]: () => ({ status: "cancelled" }),
    "GET /api/v1/resolve/cau_1": () => ({ org: "acme", kind: "cause", ticket: "tkt_1" }),
    "POST /api/v1/orgs/acme/artifacts/art_trace/share": () => ({ url: "https://trace.mage.example/trace/index.html?trace=x", expires_at: "2026-10-01T12:10:00Z" }),
  });
  renderMage("/o/acme/runs/run_1", { fetch: server.fetch, eventSource: events.open });
  return { server, events };
}

const transcriptTexts = () =>
  within(screen.getByRole("list", { name: "Run events" }))
    .getAllByRole("listitem")
    .map((li) => li.textContent);

async function stream(events: ReturnType<typeof fakeEventSources>) {
  await waitFor(() => expect(events.latest("/runs/run_1")).toBeDefined());
  return events.latest("/runs/run_1")!;
}

const emit = (src: { emit: (type: string, data: unknown, id?: string) => void }, e: RunEvent) => act(() => src.emit("run", e, String(e.seq)));

describe("run view", () => {
  it("loads every history page, then streams from the last seq and applies each event once, in order", async () => {
    const { server, events } = setup([
      { events: [text(1, "Reading the ticket. "), event(2, { type: "tool_call", data: { name: "knowledge.search_code", args_preview: "{}" } })], more: true },
      { events: [event(3, { type: "tool_result", data: { name: "knowledge.search_code", ok: true, preview: "3 hits" } })], more: false },
    ]);
    const src = await stream(events);
    expect(server.find("GET", `${RUN}/events`).map((c) => c.query.get("after"))).toEqual(["0", "2"]);
    expect(src.url).toBe("/events/orgs/acme/runs/run_1?after=3");
    expect(transcriptTexts()).toHaveLength(3);

    emit(src, event(3, { type: "tool_result", data: { name: "knowledge.search_code", ok: true, preview: "duplicate" } }));
    emit(src, text(4, "Found the handler"));
    emit(src, text(5, " in submit.ts."));
    emit(src, text(4, "Found the handler"));
    emit(src, event(6, { type: "gate_started", data: "repro" }));
    await waitFor(() => expect(transcriptTexts()).toHaveLength(5));
    const texts = transcriptTexts();
    expect(texts[0]).toContain("Reading the ticket.");
    expect(texts[2]).toContain("3 hits");
    expect(texts[2]).not.toContain("duplicate");
    expect(texts[3]).toContain("Found the handler in submit.ts.");
    expect(texts[4]).toBe("Repro gate started");
  });

  it("refetches the run when a status event arrives and shows the status timeline", async () => {
    const { server, events } = setup([{ events: [event(1, { type: "status", data: "preparing" })], more: false }]);
    const src = await stream(events);
    const before = server.find("GET", RUN).length;
    emit(src, event(2, { type: "status", data: "running" }));
    await waitFor(() => expect(server.find("GET", RUN).length).toBeGreaterThan(before));
    const timeline = screen.getByRole("list", { name: "Status timeline" });
    expect(within(timeline).getAllByRole("listitem").map((li) => li.querySelector("span")?.textContent)).toEqual(["Preparing", "Running"]);
  });

  it("shows usage against the budget", async () => {
    setup([]);
    const meter = await screen.findByRole("group", { name: "Usage against budget" });
    expect(meter).toHaveTextContent("$0.25 of $15.00");
    expect(meter).toHaveTextContent("3 of 150");
    expect(meter).toHaveTextContent("1m 0s of 45m 0s");
  });

  it("steers a running run and cancels it after confirmation", async () => {
    const user = userEvent.setup();
    const { server } = setup([]);
    await user.type(await screen.findByRole("textbox", { name: "Steer the agent" }), "Check the cart service first");
    await user.click(screen.getByRole("button", { name: "Send" }));
    await waitFor(() => expect(server.find("POST", `${RUN}/steer`)).toHaveLength(1));
    expect(server.find("POST", `${RUN}/steer`)[0]!.body).toEqual({ text: "Check the cart service first" });
    expect(screen.getByRole("textbox", { name: "Steer the agent" })).toHaveValue("");

    await user.click(screen.getByRole("button", { name: "Cancel run" }));
    expect(server.find("POST", `${RUN}/cancel`)).toHaveLength(0);
    await user.click(screen.getByRole("button", { name: "Confirm cancel" }));
    await waitFor(() => expect(server.find("POST", `${RUN}/cancel`)).toHaveLength(1));
  });

  it("disables steering while a gate runs", async () => {
    setup([], { status: "awaiting_gate" });
    expect(await screen.findByRole("textbox", { name: "Steer the agent" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Send" })).toBeDisabled();
  });

  it("hides steer and cancel once the run is terminal", async () => {
    setup([], { status: "succeeded", finished_at: "2026-10-01T12:30:00Z" });
    expect(await screen.findByRole("heading", { name: "Diagnose run" })).toBeInTheDocument();
    expect(screen.queryByRole("textbox", { name: "Steer the agent" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Cancel run" })).toBeNull();
  });

  it("shows video, GIF and trace artifacts, and opens the trace through a share link", async () => {
    const user = userEvent.setup();
    const open = vi.spyOn(window, "open").mockImplementation(() => null);
    const { server } = setup([], { status: "succeeded", finished_at: "2026-10-01T12:30:00Z" }, [
      artifact({ id: "art_video", kind: "video", name: "repro.video.mp4" }),
      artifact({ id: "art_gif", kind: "gif", name: "repro.gif" }),
      artifact({ id: "art_trace", kind: "trace", name: "repro.trace.zip" }),
      artifact({ id: "art_src", kind: "source_bundle", name: "source.tar.zst" }),
      artifact({ id: "art_part", kind: "log", name: "partial.log", complete: false }),
    ]);
    const list = await screen.findByRole("list", { name: "Artifacts" });
    expect(list.querySelector("video")).toHaveAttribute("src", "/api/v1/orgs/acme/artifacts/art_video/content");
    expect(within(list).getByRole("img", { name: "repro.gif" })).toHaveAttribute("src", "/api/v1/orgs/acme/artifacts/art_gif/content");
    expect(within(list).queryByText("source.tar.zst")).toBeNull();
    expect(within(list).queryByText("partial.log")).toBeNull();
    expect(screen.queryByRole("button", { name: "Cancel run" })).toBeNull();
    await user.click(within(list).getByRole("button", { name: "Open trace" }));
    await waitFor(() => expect(open).toHaveBeenCalledWith("https://trace.mage.example/trace/index.html?trace=x", "_blank", "noopener,noreferrer"));
    expect(server.find("POST", "/api/v1/orgs/acme/artifacts/art_trace/share")).toHaveLength(1);
  });

  it("links a cause run to the cause on its ticket", async () => {
    setup([]);
    expect(await screen.findByRole("link", { name: "Cause on its ticket" })).toHaveAttribute("href", "/o/acme/tickets/tkt_1?cause=cau_1");
  });
});
