import { render } from "@testing-library/react";
import { MemoryRouter, Route, Routes, useLocation } from "react-router";
import { vi } from "vitest";
import type { EventSourceFactory, EventSourceLike } from "../src/api/sse";
import { type MageHost, MageProvider } from "../src/context";
import type { MemberRole } from "../src/gen/mage";
import { MageRoutes } from "../src/routes";

export interface Call {
  method: string;
  /** Path and query, without the API base. */
  url: string;
  path: string;
  query: URLSearchParams;
  headers: Record<string, string>;
  body: unknown;
}

export const json = (status: number, body: unknown) => new Response(body === undefined ? null : JSON.stringify(body), { status, headers: { "content-type": "application/json" } });

export const apiError = (status: number, code: string, message: string, request_id = "req-1") => json(status, { error: { code, message, request_id } });

type Handler = (call: Call) => unknown;

/**
 * A fake Mage server: `"METHOD /path"` → handler result as JSON 200 (a `Response` is returned as
 * is, `undefined` as 204). Unknown routes answer 404 with an error body. Every call is recorded.
 */
export function fakeServer(handlers: Record<string, Handler>) {
  const calls: Call[] = [];
  const fetch = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = new URL(String(input), "http://mage.test");
    const raw = init?.body;
    const call: Call = {
      method: init?.method ?? "GET",
      url: url.pathname + url.search,
      path: url.pathname,
      query: url.searchParams,
      headers: (init?.headers ?? {}) as Record<string, string>,
      body: typeof raw === "string" ? JSON.parse(raw) : undefined,
    };
    calls.push(call);
    const handler = handlers[`${call.method} ${call.path}`];
    if (handler === undefined) return apiError(404, "not_found", `no fake for ${call.method} ${call.path}`);
    const result = handler(call);
    if (result instanceof Response) return result;
    return result === undefined ? new Response(null, { status: 204 }) : json(200, result);
  });
  const find = (method: string, path: string) => calls.filter((c) => c.method === method && c.path === path);
  return { fetch: fetch as unknown as typeof globalThis.fetch, calls, find };
}

/** An EventSource the test drives. */
export class FakeEventSource implements EventSourceLike {
  readyState = 0;
  closed = false;
  readonly url: string;
  readonly #listeners = new Map<string, Array<(e: MessageEvent<string>) => void>>();

  constructor(url: string) {
    this.url = url;
  }

  addEventListener(type: string, listener: (e: MessageEvent<string>) => void): void {
    const list = this.#listeners.get(type) ?? [];
    list.push(listener);
    this.#listeners.set(type, list);
  }

  close(): void {
    this.closed = true;
    this.readyState = 2;
  }

  emit(type: string, data: unknown = "", id?: string): void {
    const event = new MessageEvent<string>(type, { data: typeof data === "string" ? data : JSON.stringify(data), lastEventId: id ?? "" });
    for (const l of this.#listeners.get(type) ?? []) l(event);
  }

  open(): void {
    this.readyState = 1;
    this.emit("open");
  }

  /** A failed connection: `retrying` = the browser reconnects itself (CONNECTING), else it gave up (CLOSED). */
  fail(retrying: boolean): void {
    this.readyState = retrying ? 0 : 2;
    this.emit("error");
  }
}

export function fakeEventSources() {
  const sources: FakeEventSource[] = [];
  const open: EventSourceFactory = (url) => {
    const es = new FakeEventSource(url);
    sources.push(es);
    return es;
  };
  /** The newest source whose URL contains `part`. */
  const latest = (part: string) => [...sources].reverse().find((s) => s.url.includes(part));
  return { open, sources, latest };
}

export const ana = { id: "usr_ana", email: "ana@example.com", name: "Ana" };

/** Shows the router's location so tests can assert navigation. */
function Where() {
  const location = useLocation();
  return (
    <output data-testid="location">
      {location.pathname}
      {location.search}
    </output>
  );
}

/** Mounts the Mage routes at `/o/acme/*` like mage/web-app, at `path`. */
export function renderMage(path: string, options: { fetch: typeof fetch; role?: MemberRole; csrfToken?: string; eventSource?: EventSourceFactory }) {
  const host: MageHost = {
    apiBase: "",
    user: ana,
    org: { org: "acme", name: "Acme", role: options.role ?? "admin" },
    fetch: options.fetch,
    basePath: "/o/acme",
    csrfToken: options.csrfToken ?? "csrf-1",
    eventSource: options.eventSource ?? fakeEventSources().open,
  };
  return render(
    <MemoryRouter initialEntries={[path]}>
      <Routes>
        <Route
          path="/o/:org/*"
          element={
            <MageProvider host={host}>
              <MageRoutes />
            </MageProvider>
          }
        />
      </Routes>
      <Where />
    </MemoryRouter>,
  );
}
