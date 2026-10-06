import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { setTimeout as sleep } from "node:timers/promises";
import { describe, expect, it } from "vitest";
import { repo, SHA, ticketSummary } from "./fixtures";
import { type Call, fakeEventSources, fakeServer, renderMage } from "./helpers";

function setup(path = "/o/acme/tickets") {
  const events = fakeEventSources();
  const server = fakeServer({
    "GET /api/v1/orgs/acme/tickets": (c: Call) =>
      c.query.get("cursor") === "page-2"
        ? { items: [ticketSummary({ id: "tkt_3", identifier: "ENG-3", title: "Third" })], next_cursor: null }
        : { items: [ticketSummary(), ticketSummary({ id: "tkt_2", identifier: "ENG-2", title: "Second", status: "new", top_cause: null, target: null, assumed: false, customer: "cus_1" })], next_cursor: c.query.has("q") ? null : "page-2" },
    "GET /api/v1/orgs/acme/repos": () => ({ repos: [{ repo: repo(), head: null, recipe: null, last_learn: null }] }),
    "GET /api/v1/orgs/acme/customers": () => ({ customers: [{ id: "cus_1", org_id: "org_acme", name: "Globex", external_refs: [], environment: "production" }] }),
  });
  renderMage(path, { fetch: server.fetch, eventSource: events.open });
  const listCalls = () => server.find("GET", "/api/v1/orgs/acme/tickets").filter((c) => !c.query.has("cursor"));
  return { server, events, listCalls };
}

const rows = () => within(screen.getByRole("table", { name: "Tickets" })).getAllByRole("row").slice(1);
const location = () => screen.getByTestId("location").textContent;

describe("tickets list", () => {
  it("shows identifier, status, top cause with confidence, target with the assumed badge, customer", async () => {
    setup();
    expect(await screen.findByRole("link", { name: "ENG-412" })).toHaveAttribute("href", "/o/acme/tickets/tkt_1");
    const [first, second] = rows();
    expect(first).toHaveTextContent("Triaged");
    expect(first).toHaveTextContent("Missing await in submit handler 82%");
    expect(within(first!).getByTitle(SHA)).toHaveTextContent(SHA.slice(0, 7));
    expect(within(first!).getByText("assumed")).toBeInTheDocument();
    expect(first).toHaveTextContent("acme/web");
    expect(second).toHaveTextContent("none");
    expect(await within(second!).findByText("Globex")).toBeInTheDocument();
  });

  it("sends each filter to the server and keeps it in the URL", async () => {
    const user = userEvent.setup();
    const { listCalls } = setup();
    await screen.findByRole("link", { name: "ENG-412" });
    expect(listCalls().at(-1)!.url).toBe("/api/v1/orgs/acme/tickets?limit=50");

    await user.selectOptions(screen.getByRole("combobox", { name: "Status" }), "diagnosed");
    await waitFor(() => expect(listCalls().at(-1)!.url).toBe("/api/v1/orgs/acme/tickets?status=diagnosed&limit=50"));
    expect(location()).toBe("/o/acme/tickets?status=diagnosed");

    await user.selectOptions(screen.getByRole("combobox", { name: "Repo" }), "acme/web");
    await waitFor(() => expect(listCalls().at(-1)!.query.get("repo")).toBe("rep_web"));

    await user.selectOptions(await screen.findByRole("combobox", { name: "Customer" }), "Globex");
    await user.type(screen.getByRole("searchbox", { name: "Search tickets" }), "checkout{Enter}");
    await waitFor(() => expect(listCalls().at(-1)!.url).toBe("/api/v1/orgs/acme/tickets?status=diagnosed&repo=rep_web&customer=cus_1&q=checkout&limit=50"));
    expect(location()).toBe("/o/acme/tickets?status=diagnosed&repo=rep_web&customer=cus_1&q=checkout");

    await user.click(screen.getByRole("button", { name: "Clear filters" }));
    await waitFor(() => expect(listCalls().at(-1)!.url).toBe("/api/v1/orgs/acme/tickets?limit=50"));
    expect(screen.getByRole("searchbox", { name: "Search tickets" })).toHaveValue("");
  });

  it("reads filters from the URL on load and pages with the cursor under the same filters", async () => {
    const user = userEvent.setup();
    const { server } = setup("/o/acme/tickets?status=triaged");
    await screen.findByRole("link", { name: "ENG-412" });
    expect(screen.getByRole("combobox", { name: "Status" })).toHaveValue("triaged");
    await user.click(screen.getByRole("button", { name: "Load more" }));
    expect(await screen.findByRole("link", { name: "ENG-3" })).toBeInTheDocument();
    expect(server.calls.find((c) => c.query.get("cursor") === "page-2")!.url).toBe("/api/v1/orgs/acme/tickets?status=triaged&limit=50&cursor=page-2");
    expect(rows()).toHaveLength(3);
    expect(screen.queryByRole("button", { name: "Load more" })).toBeNull();
  });

  it("refetches when the org feed reports a ticket or cause change, not for other kinds", async () => {
    const { events, listCalls } = setup();
    await screen.findByRole("link", { name: "ENG-412" });
    const feed = events.latest("/feed")!;
    expect(feed.url).toBe("/events/orgs/acme/feed");
    const before = listCalls().length;
    feed.emit("feed", { kind: "member", subject: "usr_x" }, "1");
    // Past the feed's coalescing delay: nothing was scheduled.
    await sleep(400);
    expect(listCalls()).toHaveLength(before);
    feed.emit("feed", { kind: "ticket", subject: "tkt_9" }, "2");
    feed.emit("feed", { kind: "cause", subject: "cau_9" }, "3");
    await waitFor(() => expect(listCalls()).toHaveLength(before + 1));
  });
});
