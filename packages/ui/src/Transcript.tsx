// An agent conversation: prompts, replies, tool calls and results (collapsed), and notices. Each
// product maps its own protocol (aho session entries, Mage run events) to `TranscriptMessage`.
import type { ReactNode } from "react";

/** A piece of an assistant reply. */
export type TranscriptBlock = { type: "text"; text: string } | { type: "thinking"; text: string } | { type: "image"; mediaType: string };

/**
 * One transcript row. `id` keys the row and must be unique within the transcript.
 * - `user`: a prompt.
 * - `assistant`: a reply; `thinking` blocks are collapsed.
 * - `tool_call`: the tool's name, an optional class label, and its input, collapsed.
 * - `tool_result`: `summary` follows "result: "; `output` is collapsed; `err` is marked.
 * - `notice`: `note` is a quiet aside, `error` a failure, `highlight` something to act on.
 */
export type TranscriptMessage =
  | { kind: "user"; id: string; text: string }
  | { kind: "assistant"; id: string; blocks: TranscriptBlock[] }
  | { kind: "tool_call"; id: string; tool: string; label?: string; input: string }
  | { kind: "tool_result"; id: string; outcome: "ok" | "err" | "cancelled"; summary: string; output: string }
  | { kind: "notice"; id: string; tone: "note" | "error" | "highlight"; content: ReactNode };

const NOTICE_CLASS: Record<"note" | "error" | "highlight", string> = {
  note: "msg note",
  error: "msg note error",
  highlight: "msg highlight",
};

function Blocks({ blocks }: { blocks: TranscriptBlock[] }) {
  return (
    <>
      {blocks.map((b, i) => {
        switch (b.type) {
          case "text":
            return (
              <p key={i} className="text">
                {b.text}
              </p>
            );
          case "thinking":
            return (
              <details key={i} className="thinking">
                <summary>thinking</summary>
                <p>{b.text}</p>
              </details>
            );
          case "image":
            return (
              <p key={i} className="muted">
                [image {b.mediaType}]
              </p>
            );
        }
      })}
    </>
  );
}

function Row({ message: m, userLabel, assistantLabel }: { message: TranscriptMessage; userLabel: string; assistantLabel: string }) {
  switch (m.kind) {
    case "user":
      return (
        <li className="msg user">
          <span className="who">{userLabel}</span>
          <p className="text">{m.text}</p>
        </li>
      );
    case "assistant":
      return (
        <li className="msg assistant">
          <span className="who">{assistantLabel}</span>
          <Blocks blocks={m.blocks} />
        </li>
      );
    case "tool_call":
      return (
        <li className="msg tool">
          <details>
            <summary>
              <code>{m.tool}</code>
              {m.label !== undefined && (
                <>
                  {" "}
                  <span className="muted">{m.label}</span>
                </>
              )}
            </summary>
            <pre>{m.input}</pre>
          </details>
        </li>
      );
    case "tool_result":
      return (
        <li className={`msg tool-result ${m.outcome}`}>
          <details>
            <summary>result: {m.summary}</summary>
            <pre>{m.output}</pre>
          </details>
        </li>
      );
    case "notice":
      return <li className={NOTICE_CLASS[m.tone]}>{m.content}</li>;
  }
}

/** `streaming`: the reply being generated, shown after the messages until it lands as one. */
export function Transcript({ messages, streaming = "", userLabel = "you", assistantLabel, label = "transcript" }: { messages: readonly TranscriptMessage[]; streaming?: string; userLabel?: string; assistantLabel: string; label?: string }) {
  return (
    <ol className="transcript" aria-label={label}>
      {messages.map((m) => (
        <Row key={m.id} message={m} userLabel={userLabel} assistantLabel={assistantLabel} />
      ))}
      {streaming !== "" && (
        <li className="msg assistant streaming">
          <span className="who">{assistantLabel}</span>
          <p className="text">{streaming}</p>
        </li>
      )}
    </ol>
  );
}
