import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Entry, Event } from "../src/aho";
import { CLOSE_REPLACED, ConnectionClosed, RpcClient, RpcError, type SocketLike, type Status } from "../src/lib/rpc";

/** An in-memory WebSocket: records what the client sends; the test plays the server. */
class FakeSocket implements SocketLike {
  static all: FakeSocket[] = [];
  readyState = 0;
  sent: string[] = [];
  closed: { code?: number; reason?: string } | null = null;
  onopen: ((ev: unknown) => void) | null = null;
  onmessage: ((ev: { data: unknown }) => void) | null = null;
  onclose: ((ev: { code: number; reason: string }) => void) | null = null;
  onerror: ((ev: unknown) => void) | null = null;

  constructor(readonly url: string) {
    FakeSocket.all.push(this);
  }
  send(data: string) {
    if (this.readyState !== 1) throw new Error("send on a socket that is not open");
    this.sent.push(data);
  }
  close(code?: number, reason?: string) {
    this.closed = { code, reason };
    this.readyState = 3;
  }
  // ---- server side
  open() {
    this.readyState = 1;
    this.onopen?.({});
  }
  frames(): Array<{ v: number; id: number; method: string; params: unknown }> {
    return this.sent.map((s) => JSON.parse(s));
  }
  last() {
    return this.frames().at(-1)!;
  }
  push(frame: unknown) {
    this.onmessage?.({ data: JSON.stringify(frame) });
  }
  reply(id: number, payload: unknown, event = "result") {
    this.push({ v: 1, reply_to: id, event, payload });
  }
  drop() {
    this.readyState = 3;
    this.onclose?.({ code: 1006, reason: "" });
  }
}

const HELLO_OK = { version: 1, capabilities: ["session.prompt"], session: "5f3a", head: 7 };

function entry(id: number, text = `note ${id}`): Entry {
  return { id, parent: id === 1 ? null : id - 1, prev_hash: "00", hash: `h${id}`, at: 1_700_000_000_000 + id, by: { kind: "operator" }, kind: { type: "note", text } };
}

/** Runs pending microtasks (and zero-delay timers) under fake timers. */
const flush = () => vi.advanceTimersByTimeAsync(0);

/** Connects a client and completes hello + the initial subscribe; returns the live socket. */
async function connected(client: RpcClient): Promise<FakeSocket> {
  const done = client.connect();
  await flush();
  const s = FakeSocket.all.at(-1)!;
  s.open();
  s.reply(0, HELLO_OK, "hello_ok");
  await flush();
  s.reply(s.last().id, { replayed: 0, head: null });
  await done;
  return s;
}

function client(extra: Partial<ConstructorParameters<typeof RpcClient>[0]> = {}) {
  return new RpcClient({ url: "wss://host/rpc/ws1?token=t1", clientInstance: "console-test", socket: (u) => new FakeSocket(u), backoff: { initialMs: 100, maxMs: 1000 }, ...extra });
}

beforeEach(() => {
  vi.useFakeTimers();
  FakeSocket.all = [];
});
afterEach(() => {
  vi.useRealTimers();
});

describe("RpcClient handshake", () => {
  it("sends hello first with the fixed params, then subscribes from the start", async () => {
    const c = client();
    const done = c.connect();
    await flush();
    const s = FakeSocket.all[0]!;
    expect(s.url).toBe("wss://host/rpc/ws1?token=t1");
    expect(s.sent).toEqual([]);
    s.open();
    expect(s.frames()).toEqual([
      { v: 1, id: 0, method: "hello", params: { min: 1, max: 1, client_instance: "console-test", has_local_history: false, allow_takeover: false } },
    ]);
    // Nothing else goes out before hello_ok.
    await expect(c.prompt("too early")).rejects.toBeInstanceOf(ConnectionClosed);
    expect(s.sent).toHaveLength(1);

    s.reply(0, HELLO_OK, "hello_ok");
    await flush();
    expect(s.last()).toMatchObject({ method: "session.subscribe", params: {} });
    s.reply(s.last().id, { replayed: 0, head: null });
    await expect(done).resolves.toEqual(HELLO_OK);
    expect(c.status).toBe("open");
    expect(c.hello?.session).toBe("5f3a");
  });

  it("fails connect when the server refuses hello, without reconnecting", async () => {
    const statuses: Status[] = [];
    const c = client({ onStatus: (s) => statuses.push(s) });
    const done = c.connect();
    await flush();
    const s = FakeSocket.all[0]!;
    s.open();
    s.push({ v: 1, reply_to: null, event: "error", payload: { message: "takeover refused: session owned by another client" } });
    await expect(done).rejects.toThrow(/takeover refused/);
    expect(s.closed).not.toBeNull();
    await vi.advanceTimersByTimeAsync(5000);
    expect(FakeSocket.all).toHaveLength(1);
    expect(statuses.at(-1)).toBe("closed");
  });
});

describe("RpcClient requests", () => {
  it("matches replies to requests by id, whatever the reply order", async () => {
    const c = client();
    const s = await connected(c);
    const prompt = c.prompt("deploy it", "key-1");
    const answer = c.answer({ id: 12, outcome: "allow_once" });
    const [p, a] = s.frames().slice(-2);
    expect(p).toMatchObject({ v: 1, method: "session.prompt", params: { text: "deploy it", client_key: "key-1" } });
    expect(a).toMatchObject({ v: 1, method: "approval.answer", params: { id: 12, outcome: "allow_once" } });
    expect(p!.id).not.toBe(a!.id);

    s.reply(a!.id, {});
    s.reply(p!.id, { transition: { transition: "completed" } });
    await expect(answer).resolves.toEqual({});
    await expect(prompt).resolves.toEqual({ transition: { transition: "completed" } });
  });

  it("rejects with RpcError on an error reply and keeps other requests alive", async () => {
    const c = client();
    const s = await connected(c);
    const abort = c.abort();
    const prompt = c.prompt("x");
    const [ab, pr] = s.frames().slice(-2);
    expect(ab).toMatchObject({ method: "session.abort", params: {} });
    s.reply(ab!.id, { message: "no turn is running" }, "error");
    const err = await abort.catch((e: unknown) => e);
    expect(err).toBeInstanceOf(RpcError);
    expect((err as RpcError).message).toBe("`session.abort` failed: no turn is running");
    s.reply(pr!.id, { transition: { transition: "completed" } });
    await expect(prompt).resolves.toBeDefined();
  });
});

describe("RpcClient events", () => {
  it("dispatches entry, stream, idle, approval and error events in wire order", async () => {
    const c = client();
    const s = await connected(c);
    const seen: Event[] = [];
    c.on((e) => seen.push(e));
    const card = { id: 12, tool: "bash", cap: "aho-lab", class: "host_write", args: { cmd: "rm -rf /tmp/x" }, reason: "Rule", why: "bash wants host_write", world: "vm", enforced_by: "policy", suggested_cut: null, preset_cut: null, epoch_next: 2 };
    s.push({ v: 1, reply_to: null, event: "entry", payload: entry(1) });
    s.push({ v: 1, reply_to: null, event: "stream", payload: { kind: "text", text: "Build" } });
    s.push({ v: 1, reply_to: null, event: "approval", payload: card });
    s.push({ v: 1, reply_to: null, event: "idle", payload: { turn: 1, transition: { transition: "completed" } } });
    s.push({ v: 1, reply_to: null, event: "error", payload: { message: "client lagging" } });
    s.onmessage?.({ data: "not json" });
    expect(seen.map((e) => e.event)).toEqual(["entry", "stream", "approval", "idle", "error", "error"]);
    expect(seen[2]!.payload).toEqual(card);
    expect(c.lastEntryId).toBe(1);
  });

  it("drops entries it already has, so a replay overlap never duplicates the transcript", async () => {
    const c = client();
    const s = await connected(c);
    const ids: number[] = [];
    c.on((e) => {
      if (e.event === "entry") ids.push(e.payload.id);
    });
    for (const id of [1, 2, 2, 1, 3]) s.push({ v: 1, reply_to: null, event: "entry", payload: entry(id) });
    expect(ids).toEqual([1, 2, 3]);
    expect(c.lastEntryId).toBe(3);
  });
});

describe("RpcClient reconnect", () => {
  it("fails in-flight requests on a drop, reconnects with backoff, and resubscribes from the last entry id", async () => {
    const urls = ["wss://host/rpc/ws1?token=t1", "wss://host/rpc/ws1?token=t2"];
    const statuses: Status[] = [];
    const c = client({ url: async () => urls.shift() ?? "wss://host/rpc/ws1?token=t3", onStatus: (s) => statuses.push(s) });
    const done = c.connect();
    await flush();
    const s1 = FakeSocket.all[0]!;
    s1.open();
    s1.reply(0, HELLO_OK, "hello_ok");
    await flush();
    s1.reply(s1.last().id, { replayed: 7, head: 7 });
    await done;
    for (let id = 1; id <= 7; id++) s1.push({ v: 1, reply_to: null, event: "entry", payload: entry(id) });

    const inflight = c.prompt("long turn");
    s1.drop();
    await expect(inflight).rejects.toBeInstanceOf(ConnectionClosed);
    expect(c.status).toBe("reconnecting");
    await vi.advanceTimersByTimeAsync(99);
    expect(FakeSocket.all).toHaveLength(1);
    await vi.advanceTimersByTimeAsync(1);
    const s2 = FakeSocket.all[1]!;
    expect(s2.url).toBe("wss://host/rpc/ws1?token=t2");

    s2.open();
    expect(s2.frames()[0]).toMatchObject({ id: 0, method: "hello" });
    s2.reply(0, HELLO_OK, "hello_ok");
    await flush();
    expect(s2.last()).toMatchObject({ method: "session.subscribe", params: { from: 7 } });
    s2.reply(s2.last().id, { replayed: 1, head: 8 });
    s2.push({ v: 1, reply_to: null, event: "entry", payload: entry(8) });
    await flush();
    expect(c.status).toBe("open");
    expect(c.lastEntryId).toBe(8);
    expect(statuses).toEqual(["connecting", "open", "reconnecting", "reconnecting", "open"]);
  });

  it("backs off exponentially while the server stays down, and close() stops retrying", async () => {
    const c = client();
    const s1 = await connected(c);
    s1.drop();
    await vi.advanceTimersByTimeAsync(100);
    expect(FakeSocket.all).toHaveLength(2);
    FakeSocket.all[1]!.drop(); // never opened
    await vi.advanceTimersByTimeAsync(199);
    expect(FakeSocket.all).toHaveLength(2);
    await vi.advanceTimersByTimeAsync(1);
    expect(FakeSocket.all).toHaveLength(3);
    FakeSocket.all[2]!.drop();
    c.close();
    await vi.advanceTimersByTimeAsync(10_000);
    expect(FakeSocket.all).toHaveLength(3);
    expect(c.status).toBe("closed");
  });

  it("does not reconnect when another window took the workspace over", async () => {
    const statuses: Array<[Status, string | undefined]> = [];
    const c = client({ onStatus: (s, d) => statuses.push([s, d]) });
    const s1 = await connected(c);
    const inflight = c.prompt("hi");
    s1.readyState = 3;
    s1.onclose?.({ code: CLOSE_REPLACED, reason: "replaced by a newer connection" });
    await expect(inflight).rejects.toBeInstanceOf(ConnectionClosed);
    await vi.advanceTimersByTimeAsync(10_000);
    expect(FakeSocket.all).toHaveLength(1);
    expect(c.status).toBe("closed");
    expect(statuses.at(-1)?.[1]).toContain("another window");
  });
});
