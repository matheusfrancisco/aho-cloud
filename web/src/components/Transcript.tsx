// An aho session as a conversation: prompts, replies, tool calls and results, approvals, and the
// `Deploy/*` entries the cloud capability appends. Bookkeeping entries are not shown.
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

function Blocks({ blocks }: { blocks: ContentBlock[] }) {
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
          case "tool_use":
            return null; // the matching `tool_call` entry renders it
          case "tool_result":
            return <Blocks key={i} blocks={b.content} />;
          case "image":
            return (
              <p key={i} className="muted">
                [image {b.media_type}]
              </p>
            );
          case "provider_metadata":
            return null;
        }
      })}
    </>
  );
}

function blockText(blocks: ContentBlock[]): string {
  return blocks
    .map((b) => (b.type === "text" ? b.text : b.type === "tool_result" ? blockText(b.content) : ""))
    .filter((t) => t !== "")
    .join("\n");
}

function Row({ entry, onReviewDraft }: { entry: Entry; onReviewDraft?: (body: DraftedBody) => void }) {
  const k = entry.kind;
  switch (k.type) {
    case "user_message":
      return k.form === "prompt" ? (
        <li className="msg user">
          <span className="who">you</span>
          <p className="text">{k.text}</p>
        </li>
      ) : null;
    case "assistant_message":
      return (
        <li className="msg assistant">
          <span className="who">aho</span>
          <Blocks blocks={k.content} />
        </li>
      );
    case "assistant_attempt":
      return <li className="msg note error">model request failed: {k.error}</li>;
    case "tool_call":
      return (
        <li className="msg tool">
          <details>
            <summary>
              <code>{k.tool}</code> <span className="muted">{k.class.replaceAll("_", " ")}</span>
            </summary>
            <pre>{JSON.stringify(k.args, null, 2)}</pre>
          </details>
        </li>
      );
    case "tool_result": {
      const text = blockText(k.content);
      return (
        <li className={`msg tool-result ${k.outcome.outcome}`}>
          <details>
            <summary>
              result: {k.outcome.outcome === "ok" ? "ok" : k.outcome.outcome === "err" ? `error ${k.outcome.code}: ${k.outcome.reason}` : `cancelled (${k.outcome.reason})`}
            </summary>
            <pre>{text === "" ? JSON.stringify(k.value, null, 2) : text}</pre>
          </details>
        </li>
      );
    }
    case "approval_requested":
      return (
        <li className="msg note">
          approval requested for <code>{k.for_cap}</code>: {k.reason}
        </li>
      );
    case "approval_decided":
      return <li className="msg note">approval {k.outcome.replaceAll("_", " ")} ({k.scope.replaceAll("_", " ")})</li>;
    case "step_end":
      return k.transition.transition === "aborted" ? <li className="msg note">aborted: {k.transition.reason}</li> : null;
    case "note":
      return <li className="msg note">{k.text}</li>;
    case "custom": {
      const drafted = draftedBody(k);
      if (drafted !== null) {
        return (
          <li className="msg deploy">
            <strong>Manifest draft ready</strong> — {Object.keys(drafted.draft.fields).length} fields, {drafted.unconfirmed.length} to confirm.{" "}
            {onReviewDraft !== undefined && (
              <button type="button" onClick={() => onReviewDraft(drafted)}>
                Review draft
              </button>
            )}
          </li>
        );
      }
      const proposed = proposedBody(k);
      if (proposed !== null) {
        return (
          <li className="msg deploy">
            <strong>Release proposed</strong>:{" "}
            <Link to={`/projects/${encodeURIComponent(proposed.project)}/releases/${encodeURIComponent(proposed.release)}`}>
              <code>{proposed.release}</code>
            </Link>{" "}
            — review and approve it.
          </li>
        );
      }
      return null;
    }
    default:
      return null;
  }
}

/** `onReviewDraft`: offers a "Review draft" button on manifest drafts; without it they are only shown. */
export function Transcript({ entries, streaming, onReviewDraft }: { entries: Entry[]; streaming: string; onReviewDraft?: (body: DraftedBody) => void }) {
  return (
    <ol className="transcript" aria-label="transcript">
      {entries.map((e) => (
        <Row key={e.id} entry={e} onReviewDraft={onReviewDraft} />
      ))}
      {streaming !== "" && (
        <li className="msg assistant streaming">
          <span className="who">aho</span>
          <p className="text">{streaming}</p>
        </li>
      )}
    </ol>
  );
}
