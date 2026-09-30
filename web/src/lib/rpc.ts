// aho rpc over a WebSocket (Entry A): one JSON frame per message, `hello` first, replies matched
// by request id, everything else dispatched as events. Reconnects on its own and resubscribes
// from the last entry it saw, so the transcript neither loses nor repeats entries.
import type {
  AnswerParams,
  AnswerResult,
  Entry,
  Event,
  Hello,
  HelloOk,
  Method,
  Methods,
  PromptResult,
  ServerFrame,
  SubscribeResult,
} from "../aho";

/** The server answered a request with an `error` frame. */
export class RpcError extends Error {
  readonly method: string;
  constructor(method: string, message: string) {
    super(`\`${method}\` failed: ${message}`);
    this.name = "RpcError";
    this.method = method;
  }
}

/** The socket closed before a reply arrived, or a request was made while disconnected. */
export class ConnectionClosed extends Error {
  constructor(message: string) {
    super(message);
    this.name = "ConnectionClosed";
  }
}

/** The subset of the browser `WebSocket` the client uses; tests pass a fake. */
export interface SocketLike {
  readonly readyState: number;
  send(data: string): void;
  close(code?: number, reason?: string): void;
  onopen: ((ev: unknown) => void) | null;
  onmessage: ((ev: { data: unknown }) => void) | null;
  onclose: ((ev: { code: number; reason: string }) => void) | null;
  onerror: ((ev: unknown) => void) | null;
}

export type Status = "connecting" | "open" | "reconnecting" | "closed";

/** Close code the session host uses when a newer connection to the same workspace replaced this one. */
export const CLOSE_REPLACED = 4000;

export interface RpcOptions {
  /** Socket URL (with its `?token=`); a function is called on every (re)connect, e.g. to mint a fresh token. */
  url: string | (() => string | Promise<string>);
  /** `hello.client_instance`: stable per browser install. */
  clientInstance: string;
  socket?: (url: string) => SocketLike;
  /** Reconnect backoff: first delay and cap, in ms. */
  backoff?: { initialMs: number; maxMs: number };
  onStatus?: (status: Status, detail?: string) => void;
}

interface Waiting {
  method: string;
  resolve: (payload: unknown) => void;
  reject: (error: Error) => void;
}

const OPEN = 1;
const HELLO_ID = 0;

export class RpcClient {
  readonly #opts: RpcOptions;
  readonly #socketFactory: (url: string) => SocketLike;
  readonly #listeners = new Set<(ev: Event) => void>();
  #socket: SocketLike | null = null;
  #pending = new Map<number, Waiting>();
  #nextId = HELLO_ID + 1;
  #version = 1;
  #hello: HelloOk | null = null;
  #lastEntry: number | null = null;
  #stopped = false;
  #attempt = 0;
  #cancelRetry: (() => void) | null = null;
  #status: Status = "closed";

  constructor(options: RpcOptions) {
    this.#opts = options;
    this.#socketFactory = options.socket ?? ((url) => new WebSocket(url) as unknown as SocketLike);
  }

  get status(): Status {
    return this.#status;
  }

  /** The last `hello_ok`, once connected. */
  get hello(): HelloOk | null {
    return this.#hello;
  }

  /** Id of the newest entry received; reconnects resubscribe `from` it. */
  get lastEntryId(): number | null {
    return this.#lastEntry;
  }

  /** Receives every server-pushed frame (`entry`, `stream`, `idle`, `approval`, `error`). Returns an unsubscribe. */
  on(listener: (ev: Event) => void): () => void {
    this.#listeners.add(listener);
    return () => this.#listeners.delete(listener);
  }

  /** Opens the socket, completes `hello`, and subscribes. Rejects if the first attempt fails; later drops reconnect. */
  async connect(): Promise<HelloOk> {
    this.#stopped = false;
    return this.#open("connecting");
  }

  /** Closes for good: no reconnect; in-flight requests reject. */
  close(): void {
    this.#stopped = true;
    this.#cancelRetry?.();
    this.#cancelRetry = null;
    const s = this.#socket;
    this.#socket = null;
    this.#hello = null;
    s?.close(1000, "client closed");
    this.#failPending("the connection was closed");
    this.#setStatus("closed");
  }

  /** Any method by wire name. */
  request<M extends Method>(method: M, params: Methods[M]["params"]): Promise<Methods[M]["result"]> {
    return this.#send(method, params) as Promise<Methods[M]["result"]>;
  }

  /** Runs one turn; resolves with its final transition. Entries and stream chunks arrive as events meanwhile. */
  prompt(text: string, clientKey?: string): Promise<PromptResult> {
    return this.request("session.prompt", clientKey === undefined ? { text } : { text, client_key: clientKey });
  }

  abort(): Promise<Record<string, never>> {
    return this.request("session.abort", {});
  }

  answer(params: AnswerParams): Promise<AnswerResult> {
    return this.request("approval.answer", params);
  }

  subscribe(from: number | null): Promise<SubscribeResult> {
    return this.request("session.subscribe", from === null ? {} : { from });
  }

  #setStatus(status: Status, detail?: string): void {
    this.#status = status;
    this.#opts.onStatus?.(status, detail);
  }

  async #open(status: "connecting" | "reconnecting"): Promise<HelloOk> {
    this.#setStatus(status);
    let url: string;
    try {
      url = typeof this.#opts.url === "string" ? this.#opts.url : await this.#opts.url();
    } catch (e) {
      this.#retryOrGiveUp(`could not get the socket URL (${e instanceof Error ? e.message : String(e)})`);
      throw e;
    }
    if (this.#stopped) throw new ConnectionClosed("the connection was closed");
    const socket = this.#socketFactory(url);
    this.#socket = socket;
    const handshake = Promise.withResolvers<HelloOk>();
    this.#pending.set(HELLO_ID, { method: "hello", resolve: (p) => handshake.resolve(p as HelloOk), reject: handshake.reject });
    socket.onopen = () => {
      const hello: Hello = { min: 1, max: 1, client_instance: this.#opts.clientInstance, has_local_history: false, allow_takeover: false };
      socket.send(JSON.stringify({ v: 1, id: HELLO_ID, method: "hello", params: hello }));
    };
    socket.onmessage = (ev) => {
      if (this.#socket === socket) this.#frame(ev.data);
    };
    socket.onerror = () => {
      // A `close` event always follows; that is where the drop is handled.
    };
    socket.onclose = (ev) => {
      if (this.#socket !== socket) return;
      if (ev.code === CLOSE_REPLACED) {
        // Another window took the workspace over; reconnecting would take it back, forever.
        this.#socket = null;
        this.#hello = null;
        this.#failPending("the workspace was opened in another window");
        this.#setStatus("closed", "this workspace was opened in another window; reload to take it back");
        return;
      }
      this.#dropped(`the socket closed (${ev.code}${ev.reason === "" ? "" : ` ${ev.reason}`})`);
    };
    let ok: HelloOk;
    try {
      ok = await handshake.promise;
    } catch (e) {
      if (this.#socket === socket) {
        // Refused by an `error` frame: the socket is still up. A drop already handled itself.
        this.#socket = null;
        socket.close(1000, "hello refused");
        this.#retryOrGiveUp(e instanceof Error ? e.message : String(e));
      }
      throw e;
    }
    this.#hello = ok;
    this.#version = ok.version;
    this.#attempt = 0;
    this.#setStatus("open");
    await this.subscribe(this.#lastEntry);
    return ok;
  }

  #send(method: string, params: unknown): Promise<unknown> {
    const s = this.#socket;
    if (s === null || s.readyState !== OPEN || this.#hello === null) {
      return Promise.reject(new ConnectionClosed(`not connected; \`${method}\` was not sent`));
    }
    const id = this.#nextId++;
    const reply = Promise.withResolvers<unknown>();
    this.#pending.set(id, { method, resolve: reply.resolve, reject: reply.reject });
    s.send(JSON.stringify({ v: this.#version, id, method, params: params ?? {} }));
    return reply.promise;
  }

  #frame(data: unknown): void {
    if (typeof data !== "string") return;
    let f: ServerFrame;
    try {
      f = JSON.parse(data) as ServerFrame;
    } catch {
      this.#emit({ v: this.#version, reply_to: null, event: "error", payload: { message: `the server sent a frame that is not JSON; it was skipped: ${data.slice(0, 80)}` } });
      return;
    }
    // An unaddressed `error` before `hello_ok` is the server refusing the handshake (version, takeover, not-hello-first).
    const replyTo = f.reply_to ?? (this.#hello === null && f.event === "error" ? HELLO_ID : null);
    const waiting = replyTo === null ? undefined : this.#pending.get(replyTo);
    if (replyTo !== null && waiting !== undefined) {
      this.#pending.delete(replyTo);
      if (f.event === "error") {
        const p: unknown = f.payload;
        const message = typeof p === "object" && p !== null && "message" in p && typeof p.message === "string" ? p.message : JSON.stringify(p);
        waiting.reject(new RpcError(waiting.method, message));
      } else {
        waiting.resolve(f.payload);
      }
      return;
    }
    if (f.event === "entry") {
      const entry = f.payload as unknown as Entry;
      if (this.#lastEntry !== null && entry.id <= this.#lastEntry) return;
      this.#lastEntry = entry.id;
    }
    this.#emit(f as unknown as Event);
  }

  #emit(ev: Event): void {
    for (const l of this.#listeners) l(ev);
  }

  #failPending(reason: string): void {
    const pending = [...this.#pending.values()];
    this.#pending.clear();
    for (const w of pending) w.reject(new ConnectionClosed(`${reason} before \`${w.method}\` was answered`));
  }

  #dropped(reason: string): void {
    this.#socket = null;
    this.#hello = null;
    this.#failPending(reason);
    this.#retryOrGiveUp(reason);
  }

  /** A failed first `connect()` stays closed (the caller sees the rejection); any later loss reconnects with backoff. */
  #retryOrGiveUp(reason: string): void {
    if (this.#stopped) return;
    if (this.#status === "connecting") {
      this.#setStatus("closed", reason);
      return;
    }
    const { initialMs, maxMs } = this.#opts.backoff ?? { initialMs: 500, maxMs: 10_000 };
    const delay = Math.min(maxMs, initialMs * 2 ** this.#attempt);
    this.#attempt++;
    this.#setStatus("reconnecting", reason);
    const handle = setTimeout(() => {
      this.#cancelRetry = null;
      this.#open("reconnecting").catch((e: unknown) => {
        // Transport failures already scheduled the next try; a failed resubscribe on a live socket is reported.
        if (this.#status === "open") this.#emit({ v: this.#version, reply_to: null, event: "error", payload: { message: e instanceof Error ? e.message : String(e) } });
      });
    }, delay);
    this.#cancelRetry = () => clearTimeout(handle);
  }
}
