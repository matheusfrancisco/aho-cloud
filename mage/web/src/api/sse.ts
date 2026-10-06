// Mage's SSE streams (server.md "Live updates"). The browser's EventSource reconnects by itself
// after a dropped connection or a server `reconnect` (sending `Last-Event-ID`); these wrappers add
// what it does not do: drop events already applied, stop on `end`, and reopen with backoff when
// the EventSource gives up (readyState CLOSED, e.g. after an error response).
import { sseEvents } from "../gen/mage";
import type { FeedEvent, RunEvent } from "../gen/mage";

/** The part of `EventSource` the streams use (tests pass a fake). */
export interface EventSourceLike {
  readonly readyState: number;
  addEventListener(type: string, listener: (event: MessageEvent<string>) => void): void;
  close(): void;
}

export type EventSourceFactory = (url: string, init: { withCredentials: boolean }) => EventSourceLike;

export const browserEventSource: EventSourceFactory = (url, init) => new EventSource(url, init) as EventSourceLike;

/** `EventSource.CLOSED`: the browser will not reconnect this source. */
const CLOSED = 2;
const MAX_RETRY_MS = 30_000;

interface ConnectOptions {
  open: EventSourceFactory;
  withCredentials: boolean;
  /** Evaluated on every (re)open. */
  url: () => string;
  /** `event:` name → handler of its `data`. */
  on: Record<string, (data: string) => void>;
  /** A new EventSource replaced one the browser gave up on. */
  onReopen: () => void;
  retryMs: number;
}

/** One logical stream over successive EventSources. */
class Connection {
  readonly #o: ConnectOptions;
  #es: EventSourceLike | null = null;
  #timer: number | undefined = undefined;
  #delay: number;
  #closed = false;

  constructor(options: ConnectOptions) {
    this.#o = options;
    this.#delay = options.retryMs;
  }

  open(): void {
    const es = this.#o.open(this.#o.url(), { withCredentials: this.#o.withCredentials });
    this.#es = es;
    for (const [name, handle] of Object.entries(this.#o.on)) {
      es.addEventListener(name, (e) => {
        if (this.#es === es) handle(e.data);
      });
    }
    es.addEventListener("open", () => {
      if (this.#es === es) this.#delay = this.#o.retryMs;
    });
    es.addEventListener("error", () => {
      // CONNECTING: the browser is already retrying with `Last-Event-ID`.
      if (this.#es !== es || this.#closed || es.readyState !== CLOSED) return;
      this.#es = null;
      this.#timer = window.setTimeout(() => {
        this.#timer = undefined;
        if (this.#closed) return;
        this.open();
        this.#o.onReopen();
      }, this.#delay);
      this.#delay = Math.min(this.#delay * 2, MAX_RETRY_MS);
    });
  }

  close(): void {
    this.#closed = true;
    window.clearTimeout(this.#timer);
    this.#timer = undefined;
    this.#es?.close();
    this.#es = null;
  }
}

export interface RunStreamOptions {
  /** URL of the run stream for events after `seq` (`OrgApi.runEventsUrl`). */
  url: (after: number) => string;
  /** Highest `seq` already applied (from the paged history). */
  after: number;
  open: EventSourceFactory;
  withCredentials: boolean;
  /** Each new event once, in arrival order (strictly increasing `seq`). */
  onEvent: (event: RunEvent) => void;
  /** The run is terminal and every event was sent; the stream is closed. */
  onEnd: () => void;
  /** First reopen delay after the browser gave up; doubles up to 30 s. */
  retryMs?: number;
}

/** Live events of one run, resumed from the last applied `seq` on every reopen. */
export class RunStream {
  #lastSeq: number;
  readonly #conn: Connection;

  constructor(o: RunStreamOptions) {
    this.#lastSeq = o.after;
    this.#conn = new Connection({
      open: o.open,
      withCredentials: o.withCredentials,
      url: () => o.url(this.#lastSeq),
      retryMs: o.retryMs ?? 1000,
      onReopen: () => {},
      on: {
        [sseEvents.RUN]: (data) => {
          const event = parse<RunEvent>(data);
          if (event === null || typeof event.seq !== "number" || event.seq <= this.#lastSeq) return;
          this.#lastSeq = event.seq;
          o.onEvent(event);
        },
        [sseEvents.END]: () => {
          this.#conn.close();
          o.onEnd();
        },
      },
    });
  }

  /** Highest `seq` delivered so far. */
  get lastSeq(): number {
    return this.#lastSeq;
  }

  start(): this {
    this.#conn.open();
    return this;
  }

  close(): void {
    this.#conn.close();
  }
}

export interface FeedStreamOptions {
  url: string;
  open: EventSourceFactory;
  withCredentials: boolean;
  onEvent: (event: FeedEvent) => void;
  /** Events may have been missed (the stream was reopened without `Last-Event-ID`): refetch everything shown. */
  onResync: () => void;
  retryMs?: number;
}

/** The org feed: ids of what changed, so pages refetch through `/api/v1`. */
export class FeedStream {
  readonly #conn: Connection;

  constructor(o: FeedStreamOptions) {
    this.#conn = new Connection({
      open: o.open,
      withCredentials: o.withCredentials,
      url: () => o.url,
      retryMs: o.retryMs ?? 2000,
      onReopen: o.onResync,
      on: {
        [sseEvents.FEED]: (data) => {
          const event = parse<FeedEvent>(data);
          if (event !== null && typeof event.kind === "string" && typeof event.subject === "string") o.onEvent(event);
        },
      },
    });
  }

  start(): this {
    this.#conn.open();
    return this;
  }

  close(): void {
    this.#conn.close();
  }
}

function parse<T>(data: string): T | null {
  try {
    const v: unknown = JSON.parse(data);
    return typeof v === "object" && v !== null ? (v as T) : null;
  } catch {
    return null;
  }
}
