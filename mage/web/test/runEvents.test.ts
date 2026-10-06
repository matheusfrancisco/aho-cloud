import { describe, expect, it } from "vitest";
import { mergeEvents, statusTimeline, toTranscript } from "../src/lib/runEvents";
import { event, text } from "./fixtures";

describe("event log", () => {
  it("keeps each seq once, in seq order, whatever the arrival order", () => {
    const log = mergeEvents([text(1, "a"), text(2, "b")], [text(5, "e"), text(2, "dup"), text(3, "c"), text(5, "dup"), text(4, "d")]);
    expect(log.map((e) => e.seq)).toEqual([1, 2, 3, 4, 5]);
    // The first copy of a seq wins.
    expect(log[1]!.kind).toEqual({ type: "assistant_text", data: "b" });
  });

  it("returns the same log when nothing is new, so renders can skip", () => {
    const known = [text(1, "a"), text(2, "b")];
    expect(mergeEvents(known, [])).toBe(known);
    expect(mergeEvents(known, [text(2, "b"), text(1, "a")])).toBe(known);
  });
});

describe("transcript", () => {
  it("joins coalesced assistant text until another event interrupts", () => {
    const messages = toTranscript([
      text(1, "Looking at "),
      text(2, "the cart."),
      event(3, { type: "tool_call", data: { name: "knowledge.search_code", args_preview: '{"q":"submit"}' } }),
      event(4, { type: "tool_result", data: { name: "knowledge.search_code", ok: false, preview: "timeout" } }),
      text(5, "Retrying."),
    ]);
    expect(messages).toEqual([
      { kind: "assistant", id: "ev-1", blocks: [{ type: "text", text: "Looking at the cart." }] },
      { kind: "tool_call", id: "ev-3", tool: "knowledge.search_code", input: '{"q":"submit"}' },
      { kind: "tool_result", id: "ev-4", outcome: "err", summary: "knowledge.search_code failed", output: "timeout" },
      { kind: "assistant", id: "ev-5", blocks: [{ type: "text", text: "Retrying." }] },
    ]);
  });

  it("shows gates, errors and status changes as notices and leaves usage and artifacts to their panels", () => {
    const messages = toTranscript([
      event(1, { type: "status", data: "running" }),
      event(2, { type: "usage", data: { input_tokens: 1, output_tokens: 1, cost_micros: 1, turns: 1, wall: 1 } }),
      event(3, { type: "gate_started", data: "repro" }),
      event(4, { type: "gate_result", data: ["repro", { type: "failed", reason: "signature not observed", at: "2026-10-01T00:00:00Z" }] }),
      event(5, { type: "artifact", data: "art_1" }),
      event(6, { type: "error", data: "harness exited" }),
    ]);
    expect(messages.map((m) => (m.kind === "notice" ? [m.tone, m.content] : m.kind))).toEqual([
      ["note", "Status: Running"],
      ["highlight", "Repro gate started"],
      ["error", "Repro gate failed: signature not observed"],
      ["error", "harness exited"],
    ]);
  });

  it("lists status changes for the timeline", () => {
    const timeline = statusTimeline([event(1, { type: "status", data: "preparing" }), text(2, "x"), event(3, { type: "status", data: "running" })]);
    expect(timeline.map((s) => [s.seq, s.status])).toEqual([
      [1, "preparing"],
      [3, "running"],
    ]);
  });
});
