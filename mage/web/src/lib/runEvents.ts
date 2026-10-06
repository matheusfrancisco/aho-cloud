// A run's event log and what the run view derives from it. Events arrive from the paged history
// and then the SSE stream; the log keeps each `seq` once, in `seq` order, whatever the arrival order.
import type { TranscriptMessage } from "@aho/ui";
import type { GateKind, RunEvent, RunStatus, Usage } from "../gen/mage";
import { GATE_LABEL, RUN_STATUS_LABEL } from "./labels";

/** `known` plus the events of `incoming` whose `seq` is new, sorted by `seq`. Returns `known` itself when nothing is new. */
export function mergeEvents(known: readonly RunEvent[], incoming: readonly RunEvent[]): readonly RunEvent[] {
  const last = known.length === 0 ? 0 : known[known.length - 1]!.seq;
  let appendOnly = true;
  let prev = last;
  for (const e of incoming) {
    if (e.seq <= prev) {
      appendOnly = false;
      break;
    }
    prev = e.seq;
  }
  if (appendOnly) return incoming.length === 0 ? known : [...known, ...incoming];
  const bySeq = new Map<number, RunEvent>();
  for (const e of known) bySeq.set(e.seq, e);
  let added = false;
  for (const e of incoming) {
    if (bySeq.has(e.seq)) continue;
    bySeq.set(e.seq, e);
    added = true;
  }
  return added ? [...bySeq.values()].sort((a, b) => a.seq - b.seq) : known;
}

/** One entry of the status timeline. */
export interface StatusChange {
  seq: number;
  status: RunStatus;
  at: string;
}

export function statusTimeline(events: readonly RunEvent[]): StatusChange[] {
  const out: StatusChange[] = [];
  for (const e of events) if (e.kind.type === "status") out.push({ seq: e.seq, status: e.kind.data, at: e.at });
  return out;
}

/** The newest cumulative `Usage` event, if any (gateway-metered runs report usage only on the run). */
export function latestUsage(events: readonly RunEvent[]): Usage | null {
  for (let i = events.length - 1; i >= 0; i--) {
    const k = events[i]!.kind;
    if (k.type === "usage") return k.data;
  }
  return null;
}

function gateLabel(kind: GateKind): string {
  return `${GATE_LABEL[kind]} gate`;
}

/**
 * The transcript of a run: assistant text (coalesced deltas joined into one reply until another
 * event interrupts), tool calls and results (collapsed by `Transcript`), notices, gate progress,
 * status changes and errors. Usage and artifact events are shown elsewhere.
 */
export function toTranscript(events: readonly RunEvent[]): TranscriptMessage[] {
  const out: TranscriptMessage[] = [];
  for (const e of events) {
    const id = `ev-${e.seq}`;
    const k = e.kind;
    switch (k.type) {
      case "assistant_text": {
        const last = out[out.length - 1];
        const prior = last?.kind === "assistant" ? last.blocks[0] : undefined;
        if (last?.kind === "assistant" && prior?.type === "text") {
          out[out.length - 1] = { ...last, blocks: [{ type: "text", text: prior.text + k.data }] };
        } else {
          out.push({ kind: "assistant", id, blocks: [{ type: "text", text: k.data }] });
        }
        break;
      }
      case "tool_call":
        out.push({ kind: "tool_call", id, tool: k.data.name, input: k.data.args_preview });
        break;
      case "tool_result":
        out.push({ kind: "tool_result", id, outcome: k.data.ok ? "ok" : "err", summary: `${k.data.name} ${k.data.ok ? "ok" : "failed"}`, output: k.data.preview });
        break;
      case "notice":
        out.push({ kind: "notice", id, tone: "note", content: k.data });
        break;
      case "error":
        out.push({ kind: "notice", id, tone: "error", content: k.data });
        break;
      case "status":
        out.push({ kind: "notice", id, tone: "note", content: `Status: ${RUN_STATUS_LABEL[k.data]}` });
        break;
      case "gate_started":
        out.push({ kind: "notice", id, tone: "highlight", content: `${gateLabel(k.data)} started` });
        break;
      case "gate_result": {
        const [gate, status] = k.data;
        if (status.type === "passed") out.push({ kind: "notice", id, tone: "highlight", content: `${gateLabel(gate)} passed` });
        else if (status.type === "failed") out.push({ kind: "notice", id, tone: "error", content: `${gateLabel(gate)} failed: ${status.reason}` });
        else out.push({ kind: "notice", id, tone: "note", content: `${gateLabel(gate)} pending` });
        break;
      }
      case "usage":
      case "artifact":
        break;
    }
  }
  return out;
}
