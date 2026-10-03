// The browser talks aho rpc to aho: in a project's workspace (Entry A) or inside a sandbox.
// Prompts, the live transcript, aho's tool-approval cards, and abort. In a project, when aho
// drafts a manifest, the human confirms it here and the console uploads the session log (the
// provenance) and proposes.
import { useEffect, useReducer, useRef, useState } from "react";
import type { AnswerOutcome, ApprovalCard as AhoCard, Entry, Event } from "../aho";
import type { CheckResult, Release, SandboxChat, Workspace } from "../gen/api";
import type { Draft, Manifest } from "../gen/contract";
import { type AhoTarget, api } from "../lib/api";
import { draftToValue } from "../lib/draft";
import { RpcClient, type Status } from "../lib/rpc";
import { Badge, ErrorBox } from "../ui/common";
import { useAction } from "../ui/hooks";
import { Modal } from "../ui/overlay";
import { DraftConfirm } from "./DraftConfirm";
import { type DraftedBody, Transcript } from "./Transcript";

interface View {
  entries: Entry[];
  /** Live model text since the last durable assistant message. */
  streaming: string;
  cards: AhoCard[];
  notices: string[];
}

type ViewAction = Event | { event: "answered"; id: number };

function reduce(view: View, a: ViewAction): View {
  switch (a.event) {
    case "entry": {
      const k = a.payload.kind;
      const cards = k.type === "approval_decided" ? view.cards.filter((c) => c.id !== k.req) : view.cards;
      return { ...view, entries: [...view.entries, a.payload], streaming: k.type === "assistant_message" ? "" : view.streaming, cards };
    }
    case "stream":
      return a.payload.kind === "text" ? { ...view, streaming: view.streaming + a.payload.text } : view;
    case "idle":
      return { ...view, streaming: "" };
    case "approval":
      return view.cards.some((c) => c.id === a.payload.id) ? view : { ...view, cards: [...view.cards, a.payload] };
    case "error":
      return { ...view, notices: [...view.notices.slice(-4), a.payload.message] };
    case "answered":
      return { ...view, cards: view.cards.filter((c) => c.id !== a.id) };
  }
}

const EMPTY: View = { entries: [], streaming: "", cards: [], notices: [] };

function socketUrl(w: Workspace | SandboxChat): string {
  return `${w.rpc_url}${w.rpc_url.includes("?") ? "&" : "?"}token=${encodeURIComponent(w.token)}`;
}

/** Stable per browser install: aho treats the client instance as the owner of a session. */
function clientInstance(): string {
  const key = "aho-cloud.client-instance";
  let id = localStorage.getItem(key);
  if (id === null) {
    id = `console-${crypto.randomUUID()}`;
    localStorage.setItem(key, id);
  }
  return id;
}

const STATUS_TONE: Record<Status, "ok" | "warn" | "bad" | "info"> = { connecting: "info", open: "ok", reconnecting: "warn", closed: "bad" };

function CardView({ card, onAnswer }: { card: AhoCard; onAnswer: (outcome: AnswerOutcome) => void }) {
  return (
    <div className="aho-approval" role="group" aria-label={`approval ${card.tool}`}>
      <div className="row">
        <strong>
          <code>{card.tool}</code>
        </strong>
        <Badge tone="warn">{card.class.replaceAll("_", " ")}</Badge>
        {card.cap !== null && <span className="muted">{card.cap}</span>}
      </div>
      <pre className="why">{card.why}</pre>
      <details>
        <summary>Arguments</summary>
        <pre>{JSON.stringify(card.args, null, 2)}</pre>
      </details>
      <div className="row">
        <button type="button" onClick={() => onAnswer("allow_once")}>
          Allow once
        </button>
        <button type="button" className="secondary" onClick={() => onAnswer("allow_session")}>
          Allow for this session
        </button>
        <button type="button" className="danger" onClick={() => onAnswer("reject")}>
          Reject
        </button>
      </div>
    </div>
  );
}

export interface ChatProps {
  /** Where aho runs; only a project's chat can confirm drafts and propose releases. */
  target: AhoTarget;
  /** The started workspace (its preview URL), once. */
  onWorkspace?: (w: Workspace) => void;
  /** Something may have changed the files: a tool result arrived, or a turn ended. */
  onActivity?: () => void;
  /** A release proposed from an aho-drafted manifest confirmed here. */
  onProposed?: (release: Release) => void;
}

export function Chat({ target, onWorkspace, onActivity, onProposed }: ChatProps) {
  const [started, setStarted] = useState<Workspace | SandboxChat | null>(null);
  const [status, setStatus] = useState<{ status: Status; detail?: string }>({ status: "connecting" });
  const [view, dispatch] = useReducer(reduce, EMPTY);
  const [error, setError] = useState<unknown>(null);
  const [prompt, setPrompt] = useState("");
  const [running, setRunning] = useState(false);
  const [review, setReview] = useState<DraftedBody | null>(null);
  const [check, setCheck] = useState<CheckResult | null>(null);
  const client = useRef<RpcClient | null>(null);
  const log = useRef<HTMLDivElement>(null);
  const callbacks = useRef({ onWorkspace, onActivity, onProposed });
  callbacks.current = { onWorkspace, onActivity, onProposed };

  const name = target.kind === "project" ? target.project : target.sandbox;
  useEffect(() => {
    let cancelled = false;
    let rpc: RpcClient | null = null;
    const start = (sessionId?: string) => (target.kind === "project" ? api.startWorkspace(target.project, sessionId) : api.sandboxChat(target.sandbox, sessionId));
    (async () => {
      const first = await start();
      if (cancelled) return;
      setStarted(first);
      if (target.kind === "project") callbacks.current.onWorkspace?.(first as Workspace);
      let fresh: Workspace | SandboxChat | null = first;
      rpc = new RpcClient({
        // Reconnects resume the same session with a freshly minted socket token; the host
        // remembers the session learned from the first hello_ok, so `null` resumes it too.
        url: async () => {
          const w = fresh ?? (await start(rpc?.hello?.session ?? first.session_id ?? undefined));
          fresh = null;
          return socketUrl(w);
        },
        clientInstance: clientInstance(),
        onStatus: (s, detail) => {
          setStatus({ status: s, detail });
          // A (re)connected session supersedes errors from the drop that preceded it.
          if (s === "open") setError(null);
        },
      });
      rpc.on((ev) => {
        dispatch(ev);
        if ((ev.event === "entry" && ev.payload.kind.type === "tool_result") || ev.event === "idle") callbacks.current.onActivity?.();
      });
      client.current = rpc;
      await rpc.connect();
    })().catch((e: unknown) => {
      if (!cancelled) setError(e);
    });
    return () => {
      cancelled = true;
      rpc?.close();
      client.current = null;
    };
    // `target` is a fresh object every render; its kind and name identify the session.
  }, [target.kind, name]);

  // Keep the newest message in view.
  useEffect(() => {
    const el = log.current;
    if (el !== null) el.scrollTop = el.scrollHeight;
  }, [view.entries.length, view.streaming, view.cards.length]);

  const send = () => {
    const c = client.current;
    const text = prompt.trim();
    if (c === null || text === "") return;
    setPrompt("");
    setRunning(true);
    setError(null);
    c.prompt(text, crypto.randomUUID())
      .catch((e: unknown) => setError(e))
      .finally(() => setRunning(false));
  };

  const answer = (id: number, outcome: AnswerOutcome) => {
    client.current
      ?.answer({ id, outcome })
      .then(() => dispatch({ event: "answered", id }))
      .catch((e: unknown) => setError(e));
  };

  const submit = useAction(async (draft: Draft) => {
    if (target.kind !== "project") throw new Error("only a project's chat proposes releases");
    if (started === null) throw new Error("the workspace is not started");
    const project = target.project;
    const manifest = draftToValue(draft) as unknown as Manifest;
    const result = await api.checkManifest({ manifest, edge_toml: null });
    setCheck(result);
    if (result.refusals.length > 0 || result.edge_errors.length > 0) return;
    // Provenance: the log as received over rpc, in append order; the server verifies its chain.
    const jsonl = view.entries.map((e) => JSON.stringify(e)).join("\n") + "\n";
    // The handshake names the session authoritatively; the workspace may predate it.
    const sessionId = client.current?.hello?.session ?? started.session_id;
    if (sessionId === null || sessionId === undefined) throw new Error("the session has not started yet");
    const uploaded = await api.uploadSession(project, sessionId, jsonl);
    const release = await api.propose(project, { manifest, edge_toml: null, provenance: { kind: "attested", session_id: uploaded.session_id, head_hash: uploaded.head_hash } });
    setReview(null);
    callbacks.current.onProposed?.(release);
  });

  const open = status.status === "open";

  return (
    <section className="chat" aria-label="Chat with aho">
      <header className="pane-head">
        <h2>Chat</h2>
        <Badge tone={STATUS_TONE[status.status]}>{status.status}</Badge>
        {status.detail !== undefined && status.status !== "open" && <span className="muted small grow">{status.detail}</span>}
        {status.status === "closed" && client.current !== null && (
          <button type="button" className="secondary small" onClick={() => client.current?.connect().catch((e: unknown) => setError(e))}>
            Reconnect
          </button>
        )}
      </header>
      <div className="chat-log" ref={log}>
        {view.entries.length === 0 && view.streaming === "" && (
          <p className="chat-empty muted">
            {target.kind === "project"
              ? "Tell aho what to build. It works in this project's workspace; the Code tab shows its files as they change."
              : "Ask aho to build or fix something. It works in this sandbox's /workspace; the Files tab shows its files as they change."}
          </p>
        )}
        <Transcript
          entries={view.entries}
          streaming={view.streaming}
          onReviewDraft={
            target.kind === "project"
              ? (body) => {
                  setCheck(null);
                  submit.clear();
                  setReview(body);
                }
              : undefined
          }
        />
        {view.cards.map((c) => (
          <CardView key={c.id} card={c} onAnswer={(o) => answer(c.id, o)} />
        ))}
      </div>
      <div className="chat-foot">
        <ErrorBox error={error} />
        {view.notices.map((n, i) => (
          <p key={i} className="error">
            {n}
          </p>
        ))}
        <form
          className="prompt"
          onSubmit={(e) => {
            e.preventDefault();
            send();
          }}
        >
          <textarea
            value={prompt}
            onChange={(e) => setPrompt(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !e.shiftKey) {
                e.preventDefault();
                send();
              }
            }}
            placeholder={open ? "Ask aho to build, fix, or deploy… (Enter sends, Shift+Enter for a new line)" : "Connecting…"}
            rows={3}
            disabled={!open}
            aria-label="prompt"
          />
          <div className="row">
            {running && <span className="working muted small grow">aho is working…</span>}
            <button type="button" className="danger" disabled={!open || !running} onClick={() => client.current?.abort().catch((e: unknown) => setError(e))}>
              Abort
            </button>
            <button type="submit" disabled={!open || prompt.trim() === ""}>
              Send
            </button>
          </div>
        </form>
      </div>
      {review !== null && (
        <Modal title="Confirm aho's manifest draft" onClose={() => setReview(null)} wide>
          <DraftConfirm draft={review.draft} onSubmit={(d) => void submit.run(d)} submitting={submit.busy} />
          {check !== null && (check.refusals.length > 0 || check.edge_errors.length > 0) && (
            <div className="error" role="alert">
              <strong>The manifest was refused.</strong> Correct these fields and submit again.
              <ul>
                {check.refusals.map((r) => (
                  <li key={`${r.code}:${r.field}`}>
                    <code>{r.field}</code> {r.message} <em>Fix: {r.fix}</em>
                  </li>
                ))}
                {check.edge_errors.map((e) => (
                  <li key={e}>edge.toml: {e}</li>
                ))}
              </ul>
            </div>
          )}
          <ErrorBox error={submit.error} />
        </Modal>
      )}
    </section>
  );
}
