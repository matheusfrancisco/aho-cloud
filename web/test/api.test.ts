import { describe, expect, it, vi } from "vitest";
import { Api, ApiError, decodeError } from "../src/lib/api";
import { currentSession, signIn } from "../src/lib/session";
import { manifest } from "./fixtures";

interface Call {
  url: string;
  method: string;
  headers: Record<string, string>;
  body: string | undefined;
}

function fakeFetch(respond: (call: Call) => Response | Promise<Response>) {
  const calls: Call[] = [];
  const fetch = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const call: Call = { url: String(input), method: init?.method ?? "GET", headers: (init?.headers ?? {}) as Record<string, string>, body: init?.body as string | undefined };
    calls.push(call);
    return respond(call);
  });
  return { fetch: fetch as unknown as typeof globalThis.fetch, calls };
}

/** The ApiError a call rejects with; fails the test if it resolves or rejects with anything else. */
async function failure(call: Promise<unknown>): Promise<ApiError> {
  const e = await call.then(
    () => new Error("expected the call to fail"),
    (err: unknown) => err,
  );
  if (!(e instanceof ApiError)) throw e;
  return e;
}

const json = (status: number, body: unknown) => new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });

describe("Api routes", () => {
  it("fills route templates with encoded values and sends method, bearer and JSON body", async () => {
    const { fetch, calls } = fakeFetch(() => json(200, { id: "r/1" }));
    const api = new Api({ baseUrl: "https://cp.example/", fetch, token: () => "tok-1" });
    await api.approve("shop", "r/1 ü", { stage: "live", proof: { method: "device_key", key_id: "k", signature: "s" } });
    expect(calls).toHaveLength(1);
    const c = calls[0]!;
    expect(c.url).toBe("https://cp.example/v1/projects/shop/releases/r%2F1%20%C3%BC/approve");
    expect(c.method).toBe("POST");
    expect(c.headers.authorization).toBe("Bearer tok-1");
    expect(c.headers["content-type"]).toBe("application/json");
    expect(JSON.parse(c.body!)).toEqual({ stage: "live", proof: { method: "device_key", key_id: "k", signature: "s" } });
  });

  it("encodes hostnames in the domain-verify route and sends no body", async () => {
    const { fetch, calls } = fakeFetch(() => json(200, { hostname: "a.example", mode: "cname", state: "verified", records: [] }));
    const api = new Api({ fetch, token: () => null });
    const view = await api.verifyDomain("shop", "a.example/../x");
    expect(view.state).toBe("verified");
    expect(calls[0]!.url).toBe("/v1/projects/shop/domains/a.example%2F..%2Fx/verify");
    expect(calls[0]!.headers.authorization).toBeUndefined();
    expect(calls[0]!.body).toBeUndefined();
  });

  it("uploads a session log raw, with its session id in the query", async () => {
    const { fetch, calls } = fakeFetch(() => json(200, { session_id: "ab", head_hash: "h", entries: 2 }));
    const api = new Api({ fetch, token: () => "t" });
    await api.uploadSession("shop", "a b", '{"id":1}\n{"id":2}\n');
    expect(calls[0]!.url).toBe("/v1/projects/shop/sessions?session_id=a%20b");
    expect(calls[0]!.headers["content-type"]).toBe("application/x-ndjson");
    expect(calls[0]!.body).toBe('{"id":1}\n{"id":2}\n');
  });

  it("resolves undefined for an empty 204 reply", async () => {
    const { fetch } = fakeFetch(() => new Response(null, { status: 204 }));
    const api = new Api({ fetch, token: () => "t" });
    await expect(api.approveDevice({ user_code: "WDJB-MJHT" })).resolves.toBeUndefined();
  });

  it("deploys the workspace with an empty body so its deploy.json is used, or with the confirmed manifest", async () => {
    const { fetch, calls } = fakeFetch(() => json(200, { status: "needs_manifest", draft: { fields: {} } }));
    const api = new Api({ fetch, token: () => "t" });
    const first = await api.workspaceDeploy("shop");
    expect(first.status).toBe("needs_manifest");
    expect(calls[0]!.url).toBe("/v1/projects/shop/workspace/deploy");
    expect(calls[0]!.method).toBe("POST");
    expect(JSON.parse(calls[0]!.body!)).toEqual({});
    await api.workspaceDeploy("shop", manifest);
    expect(JSON.parse(calls[1]!.body!)).toEqual({ manifest });
  });

  it("reads a workspace file by its encoded path", async () => {
    const { fetch, calls } = fakeFetch(() => json(200, { path: "src/a b.ts", size: 1, text: "x", truncated: false }));
    const api = new Api({ fetch, token: () => "t" });
    await api.workspaceFile("shop", "src/a b.ts?&#");
    expect(calls[0]!.url).toBe("/v1/projects/shop/workspace/file?path=src%2Fa%20b.ts%3F%26%23");
    expect(calls[0]!.method).toBe("GET");
  });
});

describe("Api errors", () => {
  it("decodes the server's ApiError body, refusals included", async () => {
    const refusal = { code: "public_without_port", field: "/processes/0/port", message: "public process has no port", fix: "set port" };
    const { fetch } = fakeFetch(() => json(422, { code: "refused", message: "manifest refused", refusals: [refusal] }));
    const api = new Api({ fetch, token: () => "t" });
    const e = await failure(api.propose("shop", {} as never));
    expect([e.status, e.code, e.message]).toEqual([422, "refused", "manifest refused"]);
    expect(e.refusals).toEqual([refusal]);
  });

  it("keeps quorum errors distinct from transport errors", async () => {
    const quorum = await failure(new Api({ fetch: fakeFetch(() => json(409, { code: "quorum_not_met", message: "1 of 2" })).fetch, token: () => "t" }).promote("shop", "r1", { stage: "live" }));
    expect([quorum.code, quorum.refusals]).toEqual(["quorum_not_met", []]);

    const down = await failure(new Api({ fetch: (() => Promise.reject(new TypeError("Failed to fetch"))) as typeof fetch, token: () => "t" }).me());
    expect([down.status, down.code]).toEqual([0, "network"]);
    expect(down.message).toContain("Failed to fetch");
  });

  it("describes a non-ApiError body by status instead of inventing a code", () => {
    const e = decodeError(502, "Bad Gateway", "<html>upstream down</html>");
    expect([e.status, e.code]).toEqual([502, "http_502"]);
    expect(e.message).toContain("upstream down");
    expect(decodeError(500, "", "").message).toBe("500");
    // JSON that is not an ApiError (wrong field types) is not trusted as one.
    expect(decodeError(400, "Bad Request", '{"code":7,"message":"x"}').code).toBe("http_400");
  });

  it("signs out on 401 so the router sends the user to login", async () => {
    signIn({ token: "stale", user: { id: "u1", email: "a@example.com", name: "A" } });
    const { fetch, calls } = fakeFetch(() => json(401, { code: "unauthorized", message: "session expired" }));
    const api = new Api({ fetch });
    const err = await failure(api.me());
    expect(calls[0]!.headers.authorization).toBe("Bearer stale");
    expect(err.code).toBe("unauthorized");
    expect(currentSession()).toBeNull();
  });
});
