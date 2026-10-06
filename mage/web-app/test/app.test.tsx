import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { EventSourceFactory, EventSourceLike, SessionMembership, SessionView } from "@mage/web";
import { MemoryRouter, useLocation } from "react-router";
import { describe, expect, it, vi } from "vitest";
import { App } from "../src/App";

interface Call {
  method: string;
  url: string;
  headers: Record<string, string>;
}

const err = (status: number, code: string) => new Response(JSON.stringify({ error: { code, message: code, request_id: "req-1" } }), { status });

function server(routes: Record<string, () => unknown>) {
  const calls: Call[] = [];
  const fetch = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    const method = init?.method ?? "GET";
    calls.push({ method, url, headers: (init?.headers ?? {}) as Record<string, string> });
    const route = routes[`${method} ${url.split("?")[0]}`];
    if (route === undefined) return err(404, "not_found");
    const body = route();
    if (body instanceof Response) return body;
    return body === undefined ? new Response(null, { status: 204 }) : new Response(JSON.stringify(body), { status: 200 });
  });
  return { fetch: fetch as unknown as typeof globalThis.fetch, calls };
}

const quietEventSource: EventSourceFactory = (): EventSourceLike => ({ readyState: 0, addEventListener: () => {}, close: () => {} });

function session(memberships: SessionMembership[]): SessionView {
  return { user: { id: "usr_ana", email: "ana@example.com", name: "Ana" }, memberships, csrf_token: "csrf-S", features: [] };
}

const acme: SessionMembership = { org: "acme", name: "Acme", role: "admin" };
const globex: SessionMembership = { org: "globex", name: "Globex", role: "member" };

function Where() {
  const l = useLocation();
  return <output data-testid="location">{l.pathname + l.search + l.hash}</output>;
}

function renderApp(path: string, routes: Record<string, () => unknown>) {
  const s = server(routes);
  render(
    <MemoryRouter initialEntries={[path]}>
      <App fetch={s.fetch} eventSource={quietEventSource} />
      <Where />
    </MemoryRouter>,
  );
  return s;
}

const location = () => screen.getByTestId("location").textContent;
const emptyLists = {
  "GET /api/v1/orgs/acme/tickets": () => ({ items: [], next_cursor: null }),
  "GET /api/v1/orgs/acme/repos": () => ({ repos: [] }),
  "GET /api/v1/orgs/acme/customers": () => ({ customers: [] }),
};

describe("login", () => {
  it("sends signed-out visitors to the login page with where they were going", async () => {
    renderApp("/o/acme/tickets?status=new", { "GET /api/v1/session": () => err(401, "unauthenticated") });
    await waitFor(() => expect(location()).toBe("/login?return_to=%2Fo%2Facme%2Ftickets%3Fstatus%3Dnew"));
    expect(screen.getByRole("link", { name: "Sign in with GitHub" })).toHaveAttribute("href", "/auth/github/login?return_to=%2Fo%2Facme%2Ftickets%3Fstatus%3Dnew");
    expect(screen.getByRole("link", { name: "Sign in with Google" })).toHaveAttribute("href", "/auth/google/login?return_to=%2Fo%2Facme%2Ftickets%3Fstatus%3Dnew");
    expect(screen.queryByRole("complementary")).toBeNull();
  });

  it("never returns to another site after login", async () => {
    for (const evil of ["//evil.example/x", "/\\evil.example", "https://evil.example"]) {
      const { unmount } = render(
        <MemoryRouter initialEntries={[`/login?return_to=${encodeURIComponent(evil)}`]}>
          <App fetch={server({ "GET /api/v1/session": () => err(401, "unauthenticated") }).fetch} eventSource={quietEventSource} />
        </MemoryRouter>,
      );
      expect(await screen.findByRole("link", { name: "Sign in with GitHub" })).toHaveAttribute("href", "/auth/github/login?return_to=%2F");
      unmount();
    }
  });

  it("shows why the last sign-in failed", async () => {
    renderApp("/login?error=state_mismatch", { "GET /api/v1/session": () => err(401, "unauthenticated") });
    expect(await screen.findByRole("alert")).toHaveTextContent("Sign-in failed (state_mismatch)");
  });
});

describe("org selection", () => {
  it("opens the only org", async () => {
    renderApp("/", { "GET /api/v1/session": () => session([acme]), ...emptyLists });
    await waitFor(() => expect(location()).toBe("/o/acme/tickets"));
  });

  it("opens the remembered org, else lets the user choose", async () => {
    localStorage.setItem("mage.org.usr_ana", "globex");
    const { unmount } = render(
      <MemoryRouter initialEntries={["/"]}>
        <App fetch={server({ "GET /api/v1/session": () => session([acme, globex]) }).fetch} eventSource={quietEventSource} />
        <Where />
      </MemoryRouter>,
    );
    await waitFor(() => expect(location()).toBe("/o/globex/tickets"));
    unmount();
    localStorage.clear();
    renderApp("/", { "GET /api/v1/session": () => session([acme, globex]) });
    expect(await screen.findByRole("heading", { name: "Choose an organization" })).toBeInTheDocument();
    expect(screen.getByRole("link", { name: "Globex" })).toHaveAttribute("href", "/o/globex/tickets");
  });

  it("tells a user without memberships how to get access", async () => {
    renderApp("/", { "GET /api/v1/session": () => session([]) });
    expect(await screen.findByRole("heading", { name: "No access yet" })).toBeInTheDocument();
  });

  it("treats an org the user is not a member of as missing", async () => {
    renderApp("/o/initech/tickets", { "GET /api/v1/session": () => session([acme]) });
    expect(await screen.findByText("Not found.")).toBeInTheDocument();
  });
});

describe("org shell", () => {
  it("mounts the Mage pages with the Mage nav and the org switcher, and remembers the org", async () => {
    const user = userEvent.setup();
    renderApp("/o/acme/tickets", { "GET /api/v1/session": () => session([acme, globex]), ...emptyLists });
    expect(await screen.findByRole("heading", { name: "Tickets" })).toBeInTheDocument();
    const nav = screen.getByRole("navigation", { name: "Mage pages" });
    expect(within(nav).getAllByRole("link").map((a) => [a.textContent, a.getAttribute("href")])).toEqual([
      ["Tickets", "/o/acme/tickets"],
      ["Repos", "/o/acme/repos"],
      ["Settings", "/o/acme/settings/general"],
    ]);
    expect(within(nav).getByRole("link", { name: "Tickets" })).toHaveAttribute("aria-current", "page");
    expect(localStorage.getItem("mage.org.usr_ana")).toBe("acme");

    await user.click(screen.getByRole("button", { name: "Organization" }));
    await user.click(screen.getByRole("menuitem", { name: /Globex/ }));
    expect(location()).toBe("/o/globex/tickets");
  });

  it("logs out with the session's CSRF token and returns to the login page", async () => {
    const user = userEvent.setup();
    const s = renderApp("/o/acme/tickets", { "GET /api/v1/session": () => session([acme]), "POST /auth/logout": () => undefined, ...emptyLists });
    await screen.findByRole("heading", { name: "Tickets" });
    await user.click(screen.getByRole("button", { name: "Account" }));
    await user.click(screen.getByRole("menuitem", { name: "Log out" }));
    await waitFor(() => expect(location()).toBe("/login"));
    const logout = s.calls.find((c) => c.url === "/auth/logout")!;
    expect([logout.method, logout.headers["x-csrf-token"]]).toEqual(["POST", "csrf-S"]);
  });

  it("goes to login when the session expires mid-use", async () => {
    let expired = false;
    renderApp("/o/acme/repos", {
      "GET /api/v1/session": () => (expired ? err(401, "unauthenticated") : session([acme])),
      "GET /api/v1/orgs/acme/repos": () => {
        expired = true;
        return err(401, "unauthenticated");
      },
    });
    await waitFor(() => expect(location()).toBe("/login?return_to=%2Fo%2Facme%2Frepos"));
  });
});

describe("short links", () => {
  it("resolves ticket, run, cause brief and artifact links to their org pages", async () => {
    const routes = {
      "GET /api/v1/session": () => session([acme]),
      "GET /api/v1/resolve/tkt_1": () => ({ org: "acme", kind: "ticket" }),
      "GET /api/v1/resolve/run_1": () => ({ org: "acme", kind: "run" }),
      "GET /api/v1/resolve/cau_1": () => ({ org: "acme", kind: "cause", ticket: "tkt_1" }),
      "GET /api/v1/resolve/art_1": () => ({ org: "acme", kind: "artifact" }),
      "GET /api/v1/orgs/acme/artifacts/art_1": () => ({ id: "art_1", run_id: "run_7", kind: "video" }),
    };
    const cases: Array<[string, string]> = [
      ["/tickets/tkt_1", "/o/acme/tickets/tkt_1"],
      ["/runs/run_1", "/o/acme/runs/run_1"],
      ["/causes/cau_1/brief", "/o/acme/tickets/tkt_1?cause=cau_1&tab=brief"],
      ["/artifacts/art_1", "/o/acme/runs/run_7#artifact-art_1"],
    ];
    for (const [from, to] of cases) {
      const s = server(routes);
      const { unmount } = render(
        <MemoryRouter initialEntries={[from]}>
          <App fetch={s.fetch} eventSource={quietEventSource} />
          <Where />
        </MemoryRouter>,
      );
      await waitFor(() => expect(location()).toBe(to));
      unmount();
    }
  });

  it("says so when the id is unknown or in another org", async () => {
    renderApp("/tickets/tkt_x", { "GET /api/v1/session": () => session([acme]) });
    expect(await screen.findByRole("alert")).toHaveTextContent("not_found");
  });
});
