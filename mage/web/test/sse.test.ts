import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { FeedStream, RunStream } from "../src/api/sse";
import type { RunEvent } from "../src/gen/mage";
import { text } from "./fixtures";
import { fakeEventSources } from "./helpers";

beforeEach(() => {
  vi.useFakeTimers();
});
afterEach(() => {
  vi.useRealTimers();
});

function runStream(after: number) {
  const es = fakeEventSources();
  const got: RunEvent[] = [];
  const onEnd = vi.fn();
  const stream = new RunStream({ url: (seq) => `/events/orgs/acme/runs/run_1?after=${seq}`, after, open: es.open, withCredentials: false, onEvent: (e) => got.push(e), onEnd, retryMs: 1000 }).start();
  return { es, got, onEnd, stream };
}

describe("run stream", () => {
  it("starts after the history's last seq and delivers each new seq once", () => {
    const { es, got, stream } = runStream(3);
    expect(es.sources.map((s) => s.url)).toEqual(["/events/orgs/acme/runs/run_1?after=3"]);
    const src = es.sources[0]!;
    src.open();
    src.emit("run", text(3, "already in the history"), "3");
    src.emit("run", text(4, "a"), "4");
    src.emit("run", text(4, "a again"), "4");
    src.emit("run", text(2, "older"), "2");
    src.emit("run", text(5, "b"), "5");
    src.emit("run", "{not json");
    expect(got.map((e) => e.seq)).toEqual([4, 5]);
    expect(stream.lastSeq).toBe(5);
  });

  it("leaves reconnects the browser handles (Last-Event-ID) alone, including after `reconnect`", () => {
    const { es } = runStream(0);
    const src = es.sources[0]!;
    src.emit("run", text(1, "a"), "1");
    src.emit("reconnect");
    src.fail(true);
    vi.advanceTimersByTime(60_000);
    expect(es.sources).toHaveLength(1);
  });

  it("reopens from the last delivered seq when the browser gives up, backing off between tries", () => {
    const { es, got } = runStream(2);
    es.sources[0]!.emit("run", text(7, "x"), "7");
    es.sources[0]!.fail(false);
    vi.advanceTimersByTime(999);
    expect(es.sources).toHaveLength(1);
    vi.advanceTimersByTime(1);
    expect(es.sources.map((s) => s.url)).toEqual(["/events/orgs/acme/runs/run_1?after=2", "/events/orgs/acme/runs/run_1?after=7"]);

    // The second source fails before opening: the next try waits twice as long.
    es.sources[1]!.fail(false);
    vi.advanceTimersByTime(1999);
    expect(es.sources).toHaveLength(2);
    vi.advanceTimersByTime(1);
    expect(es.sources).toHaveLength(3);

    // A source that opened resets the delay; events of a replaced source are ignored.
    const third = es.sources[2]!;
    third.open();
    es.sources[0]!.emit("run", text(8, "stale source"), "8");
    third.emit("run", text(8, "y"), "8");
    third.fail(false);
    vi.advanceTimersByTime(1000);
    expect(es.sources[3]!.url).toBe("/events/orgs/acme/runs/run_1?after=8");
    expect(got.map((e) => e.kind.type === "assistant_text" && e.kind.data)).toEqual(["x", "y"]);
  });

  it("closes on `end` and never reopens afterwards", () => {
    const { es, onEnd } = runStream(0);
    const src = es.sources[0]!;
    src.emit("end");
    expect(onEnd).toHaveBeenCalledTimes(1);
    expect(src.closed).toBe(true);
    src.fail(false);
    vi.advanceTimersByTime(60_000);
    expect(es.sources).toHaveLength(1);
  });

  it("close() cancels a pending reopen", () => {
    const { es, stream } = runStream(0);
    es.sources[0]!.fail(false);
    stream.close();
    vi.advanceTimersByTime(60_000);
    expect(es.sources).toHaveLength(1);
  });
});

describe("org feed", () => {
  it("delivers feed events and asks for a resync after reopening a source the browser gave up on", () => {
    const es = fakeEventSources();
    const events: unknown[] = [];
    const onResync = vi.fn();
    new FeedStream({ url: "/events/orgs/acme/feed", open: es.open, withCredentials: false, onEvent: (e) => events.push(e), onResync, retryMs: 500 }).start();
    const src = es.sources[0]!;
    src.emit("feed", { kind: "ticket", subject: "tkt_1" }, "41");
    src.emit("feed", { kind: 3 }, "42");
    src.fail(true);
    expect(onResync).not.toHaveBeenCalled();
    src.fail(false);
    vi.advanceTimersByTime(500);
    expect(es.sources).toHaveLength(2);
    expect(onResync).toHaveBeenCalledTimes(1);
    expect(events).toEqual([{ kind: "ticket", subject: "tkt_1" }]);
  });
});
