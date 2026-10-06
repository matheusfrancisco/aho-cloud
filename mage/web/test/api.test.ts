import { CodedError } from "@aho/ui";
import { describe, expect, it, vi } from "vitest";
import { decodeError, MageApi, MageApiError, withQuery } from "../src/api/client";
import { apiError, fakeServer, json } from "./helpers";

/** The MageApiError a call rejects with; fails the test if it resolves or rejects with anything else. */
async function failure(call: Promise<unknown>): Promise<MageApiError> {
  const e = await call.then(
    () => new Error("expected the call to fail"),
    (err: unknown) => err,
  );
  if (!(e instanceof MageApiError)) throw e;
  return e;
}

describe("CSRF", () => {
  it("sends X-CSRF-Token on every mutation and never on reads", async () => {
    const server = fakeServer({
      "GET /api/v1/orgs/acme/tickets/tkt_1": () => ({}),
      "POST /api/v1/orgs/acme/tickets/tkt_1/retriage": () => ({ job_id: "job_1" }),
      "PUT /api/v1/orgs/acme/settings/model/key": () => ({ fingerprint: "sk-ant-…a1b2", set_at: "2026-10-01T00:00:00Z", set_by: { type: "system" } }),
      "DELETE /api/v1/orgs/acme/members/usr_bo": () => undefined,
    });
    const org = new MageApi({ fetch: server.fetch, csrfToken: () => "tok-9" }).org("acme");
    await org.ticket("tkt_1");
    await org.retriage("tkt_1");
    await org.setModelKey({ api_key: "sk-ant-secret" });
    await org.removeMember("usr_bo");
    expect(server.calls.map((c) => [c.method, c.headers["x-csrf-token"]])).toEqual([
      ["GET", undefined],
      ["POST", "tok-9"],
      ["PUT", "tok-9"],
      ["DELETE", "tok-9"],
    ]);
  });

  it("reads the token per request, and sends none when the host has none (aho cloud edge)", async () => {
    const server = fakeServer({ "POST /api/v1/orgs/acme/runs/run_1/cancel": () => ({ status: "cancelled" }) });
    let token: string | null = "first";
    const org = new MageApi({ fetch: server.fetch, csrfToken: () => token }).org("acme");
    await org.cancel("run_1");
    token = "rotated";
    await org.cancel("run_1");
    token = null;
    await org.cancel("run_1");
    expect(server.calls.map((c) => c.headers["x-csrf-token"])).toEqual(["first", "rotated", undefined]);
  });

  it("sets a JSON content type only when there is a body", async () => {
    const server = fakeServer({
      "POST /api/v1/orgs/acme/runs/run_1/steer": () => ({ seq: 4 }),
      "POST /api/v1/orgs/acme/runs/run_1/cancel": () => ({ status: "cancelled" }),
    });
    const org = new MageApi({ fetch: server.fetch, csrfToken: () => "t" }).org("acme");
    await org.steer("run_1", { text: "look at the cart" });
    await org.cancel("run_1");
    expect(server.calls[0]!.headers["content-type"]).toBe("application/json");
    expect(server.calls[0]!.body).toEqual({ text: "look at the cart" });
    expect(server.calls[1]!.headers["content-type"]).toBeUndefined();
  });
});

describe("routes", () => {
  it("fills templates with encoded values under the API base", async () => {
    const urls: string[] = [];
    const fetch = vi.fn(async (input: RequestInfo | URL) => {
      urls.push(String(input));
      return json(200, { run_id: "run_9" });
    }) as unknown as typeof globalThis.fetch;
    const org = new MageApi({ apiBase: "https://mage.example/", fetch }).org("acme co");
    await org.diagnose("tkt/1", "cau 1", { harness: "claude_code" });
    expect(urls).toEqual(["https://mage.example/api/v1/orgs/acme%20co/tickets/tkt%2F1/causes/cau%201/diagnose"]);
  });

  it("uses the integration slug, not the serde name, in integration paths", async () => {
    const server = fakeServer({
      "POST /api/v1/orgs/acme/integrations/github-fix/connect": () => ({ url: "https://github.com/apps/mage-fix/installations/new?state=s" }),
      "DELETE /api/v1/orgs/acme/integrations/github-fix/991": () => undefined,
    });
    const org = new MageApi({ fetch: server.fetch }).org("acme");
    await expect(org.connectIntegration("github_fix")).resolves.toEqual({ url: "https://github.com/apps/mage-fix/installations/new?state=s" });
    await expect(org.revokeIntegration("github_fix", "991")).resolves.toBeUndefined();
  });

  it("builds list queries from the defined filters only", async () => {
    const server = fakeServer({ "GET /api/v1/orgs/acme/tickets": () => ({ items: [], next_cursor: null }) });
    const org = new MageApi({ fetch: server.fetch }).org("acme");
    await org.tickets({ status: "triaged", repo: undefined, q: "pay & ship", limit: 50 });
    expect(server.calls[0]!.url).toBe("/api/v1/orgs/acme/tickets?status=triaged&q=pay+%26+ship&limit=50");
    expect(withQuery("/x", { a: undefined, b: null, c: "", d: 0 })).toBe("/x?d=0");
  });

  it("asks for a byte range of large text artifacts and says the text is partial", async () => {
    const server = fakeServer({ "GET /api/v1/orgs/acme/artifacts/art_1/content": () => new Response("é".repeat(3), { status: 206 }) });
    const org = new MageApi({ fetch: server.fetch }).org("acme");
    const artifact = { id: "art_1", run_id: "run_1", kind: "log", name: "runner.log", content_type: "text/plain", bytes: 10_000, sha256: "00", complete: true, created_at: "2026-10-01T00:00:00Z" } as const;
    // 5 bytes of three 2-byte characters: the cut character is left out, not replaced.
    const partial = await org.artifactText(artifact, 5);
    expect(server.calls[0]!.headers.range).toBe("bytes=0-4");
    expect(partial).toEqual({ text: "éé", truncated: true });
    const whole = await org.artifactText({ ...artifact, bytes: 6 }, 5_000);
    expect(server.calls[1]!.headers.range).toBeUndefined();
    expect(whole).toEqual({ text: "ééé", truncated: false });
  });

  it("resolves undefined for an empty 204 reply", async () => {
    const server = fakeServer({ "DELETE /api/v1/orgs/acme/settings/model/key": () => undefined });
    await expect(new MageApi({ fetch: server.fetch }).org("acme").deleteModelKey()).resolves.toBeUndefined();
  });
});

describe("errors", () => {
  it("maps the server's error body: code, message, request id and status, as a CodedError", async () => {
    const server = fakeServer({ "POST /api/v1/orgs/acme/runs/run_1/steer": () => apiError(409, "conflict", "gate running", "req-77") });
    const e = await failure(new MageApi({ fetch: server.fetch }).org("acme").steer("run_1", { text: "x" }));
    expect(e).toBeInstanceOf(CodedError);
    expect([e.status, e.code, e.message, e.requestId]).toEqual([409, "conflict", "gate running", "req-77"]);
  });

  it("calls onUnauthorized on 401 and still rejects", async () => {
    const onUnauthorized = vi.fn();
    const server = fakeServer({ "GET /api/v1/session": () => apiError(401, "unauthenticated", "session expired") });
    const e = await failure(new MageApi({ fetch: server.fetch, onUnauthorized }).session());
    expect(e.code).toBe("unauthenticated");
    expect(onUnauthorized).toHaveBeenCalledTimes(1);
  });

  it("describes bodies that are not Mage errors by status instead of trusting them", () => {
    const proxy = decodeError(502, "Bad Gateway", "<html>upstream down</html>");
    expect([proxy.status, proxy.code, proxy.requestId]).toEqual([502, "http_502", null]);
    expect(proxy.message).toBe("502 Bad Gateway: <html>upstream down</html>");
    expect(decodeError(500, "", "").message).toBe("500");
    // Right shape, unknown code: not a Mage error.
    expect(decodeError(400, "Bad Request", JSON.stringify({ error: { code: "teapot", message: "x", request_id: "r" } })).code).toBe("http_400");
    // aho cloud's flat error shape is not Mage's either.
    expect(decodeError(403, "Forbidden", JSON.stringify({ code: "forbidden", message: "no" })).code).toBe("http_403");
  });

  it("reports transport failures and non-JSON success bodies with their own codes", async () => {
    const down = await failure(new MageApi({ fetch: (() => Promise.reject(new TypeError("Failed to fetch"))) as typeof fetch }).session());
    expect([down.status, down.code]).toEqual([0, "network"]);
    expect(down.message).toContain("Failed to fetch");
    const garbled = await failure(new MageApi({ fetch: (async () => new Response("<html/>", { status: 200 })) as typeof fetch }).session());
    expect([garbled.status, garbled.code]).toEqual([200, "invalid_response"]);
  });
});
