// An aho session as a conversation: prompts, replies, tool calls and results, approvals, and the
// `Deploy/*` entries the cloud capability appends, mapped to `@aho/ui`'s transcript model.
// Bookkeeping entries are not shown.
import { type TranscriptBlock, type TranscriptMessage, Transcript as TranscriptView } from "@aho/ui";
import { Link } from "react-router";
import type { ContentBlock, Entry, EntryKindCustom } from "../aho";
import type { Draft } from "../gen/contract";

/** Body of a `Deploy/Drafted` entry. */
export interface DraftedBody {
  session_id: string;
  head_hash: string;
  draft: Draft;
  unconfirmed: string[];
}

/** Body of a `Deploy/Proposed` entry. */
export interface ProposedBody {
  project: string;
  release: string;
  manifest_hash: string;
}

export function draftedBody(kind: EntryKindCustom): DraftedBody | null {
  if (kind.ns !== "Deploy" || kind.name !== "Drafted") return null;
  const b = kind.body as Partial<DraftedBody> | null;
  if (b === null || typeof b !== "object" || typeof b.draft !== "object" || b.draft === null || typeof b.draft.fields !== "object") return null;
  return b as DraftedBody;
}

function proposedBody(kind: EntryKindCustom): ProposedBody | null {
  if (kind.ns !== "Deploy" || kind.name !== "Proposed") return null;
  const b = kind.body as Partial<ProposedBody> | null;
  return b !== null && typeof b === "object" && typeof b.release === "string" && typeof b.project === "string" ? (b as ProposedBody) : null;
}

function blocksOf(blocks: ContentBlock[]): TranscriptBlock[] {
  return blocks.flatMap((b): TranscriptBlock[] => {
    switch (b.type) {
      case "text":
        return [{ type: "text", text: b.text }];
      case "thinking":
        return [{ type: "thinking", text: b.text }];
      case "tool_use":
        return []; // the matching `tool_call` entry renders it
      case "tool_result":
        return blocksOf(b.content);
      case "image":
        return [{ type: "image", mediaType: b.media_type }];
      case "provider_metadata":
        return [];
    }
  });
}

function blockText(blocks: ContentBlock[]): string {
  return blocks
    .map((b) => (b.type === "text" ? b.text : b.type === "tool_result" ? blockText(b.content) : ""))
    .filter((t) => t !== "")
    .join("\n");
}

function message(entry: Entry, onReviewDraft: ((body: DraftedBody) => void) | undefined): TranscriptMessage | null {
  const id = String(entry.id);
  const k = entry.kind;
  switch (k.type) {
    case "user_message":
      return k.form === "prompt" ? { kind: "user", id, text: k.text } : null;
    case "assistant_message":
      return { kind: "assistant", id, blocks: blocksOf(k.content) };
    case "assistant_attempt":
      return { kind: "notice", id, tone: "error", content: `model request failed: ${k.error}` };
    case "tool_call":
      return { kind: "tool_call", id, tool: k.tool, label: k.class.replaceAll("_", " "), input: JSON.stringify(k.args, null, 2) };
    case "tool_result": {
      const text = blockText(k.content);
      const o = k.outcome;
      return {
        kind: "tool_result",
        id,
        outcome: o.outcome,
        summary: o.outcome === "ok" ? "ok" : o.outcome === "err" ? `error ${o.code}: ${o.reason}` : `cancelled (${o.reason})`,
        output: text === "" ? JSON.stringify(k.value, null, 2) : text,
      };
    }
    case "approval_requested":
      return {
        kind: "notice",
        id,
        tone: "note",
        content: (
          <>
            approval requested for <code>{k.for_cap}</code>: {k.reason}
          </>
        ),
      };
    case "approval_decided":
      return { kind: "notice", id, tone: "note", content: `approval ${k.outcome.replaceAll("_", " ")} (${k.scope.replaceAll("_", " ")})` };
    case "step_end":
      return k.transition.transition === "aborted" ? { kind: "notice", id, tone: "note", content: `aborted: ${k.transition.reason}` } : null;
    case "note":
      return { kind: "notice", id, tone: "note", content: k.text };
    case "custom": {
      const drafted = draftedBody(k);
      if (drafted !== null) {
        return {
          kind: "notice",
          id,
          tone: "highlight",
          content: (
            <>
              <strong>Manifest draft ready</strong> — {Object.keys(drafted.draft.fields).length} fields, {drafted.unconfirmed.length} to confirm.{" "}
              {onReviewDraft !== undefined && (
                <button type="button" onClick={() => onReviewDraft(drafted)}>
                  Review draft
                </button>
              )}
            </>
          ),
        };
      }
      const proposed = proposedBody(k);
      if (proposed !== null) {
        return {
          kind: "notice",
          id,
          tone: "highlight",
          content: (
            <>
              <strong>Release proposed</strong>:{" "}
              <Link to={`/projects/${encodeURIComponent(proposed.project)}/releases/${encodeURIComponent(proposed.release)}`}>
                <code>{proposed.release}</code>
              </Link>{" "}
              — review and approve it.
            </>
          ),
        };
      }
      return null;
    }
    default:
      return null;
  }
}

/** `onReviewDraft`: offers a "Review draft" button on manifest drafts; without it they are only shown. */
export function Transcript({ entries, streaming, onReviewDraft }: { entries: Entry[]; streaming: string; onReviewDraft?: (body: DraftedBody) => void }) {
  const messages = entries.flatMap((e) => message(e, onReviewDraft) ?? []);
  return <TranscriptView messages={messages} streaming={streaming} assistantLabel="aho" />;
}
