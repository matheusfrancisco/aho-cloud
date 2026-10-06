// What the host gives Mage's pages (deployment.md "Frontend"): the API base, the signed-in user,
// the current org, and a fetch function (cookies in self-host, the same-origin edge on aho cloud).
// The provider builds the org's API client and keeps one org-feed stream per tab; pages subscribe
// to it to refetch what changed.
import { createContext, type ReactNode, useContext, useEffect, useMemo, useRef } from "react";
import { MageApi, type OrgApi } from "./api/client";
import { browserEventSource, type EventSourceFactory, FeedStream } from "./api/sse";
import type { FeedEvent, MemberRole, SessionMembership, User } from "./gen/mage";
import { atLeast } from "./lib/labels";

export interface MageHost {
  /** Prefix of Mage's routes (`""` = same origin). */
  apiBase: string;
  user: User;
  /** The current org: slug, name and the user's role in it. */
  org: SessionMembership;
  fetch: typeof fetch;
  /** Path the routes are mounted under (e.g. `/o/acme`); page links are built from it. */
  basePath: string;
  /** Self-host session's CSRF token (`SessionView.csrf_token`), sent on mutations; absent on aho cloud. */
  csrfToken?: string;
  /** A request answered 401: the session is gone. */
  onUnauthorized?: () => void;
  /** EventSource constructor; tests pass a fake. */
  eventSource?: EventSourceFactory;
}

type FeedListener = (event: FeedEvent | null) => void;

/** Fans the org feed out to the pages; `null` = resync (events may have been missed). */
class FeedHub {
  readonly #listeners = new Set<FeedListener>();

  subscribe(listener: FeedListener): () => void {
    this.#listeners.add(listener);
    return () => this.#listeners.delete(listener);
  }

  publish(event: FeedEvent | null): void {
    for (const l of this.#listeners) l(event);
  }
}

export interface MageState {
  host: MageHost;
  api: OrgApi;
  role: MemberRole;
  /** Absolute app path of a Mage page, e.g. `link("runs/run_…")`. */
  link: (path: string) => string;
  /** The user's role in the org is at least `min`. */
  can: (min: MemberRole) => boolean;
  openEventSource: EventSourceFactory;
  feed: FeedHub;
}

const MageContext = createContext<MageState | null>(null);

export function useMage(): MageState {
  const state = useContext(MageContext);
  if (state === null) throw new Error("Mage pages must be rendered inside <MageProvider>");
  return state;
}

export function MageProvider({ host, children }: { host: MageHost; children: ReactNode }) {
  // Callbacks read the latest host, so a new CSRF token or handler does not rebuild the client.
  const hostRef = useRef(host);
  hostRef.current = host;
  const { apiBase, fetch, basePath } = host;
  const slug = host.org.org;
  const role = host.org.role;
  const openEventSource = host.eventSource ?? browserEventSource;

  const api = useMemo(
    () =>
      new MageApi({
        apiBase,
        fetch,
        csrfToken: () => hostRef.current.csrfToken ?? null,
        onUnauthorized: () => hostRef.current.onUnauthorized?.(),
      }).org(slug),
    [apiBase, fetch, slug],
  );
  const feed = useMemo(() => new FeedHub(), [slug]);

  useEffect(() => {
    const stream = new FeedStream({
      url: api.feedUrl(),
      open: openEventSource,
      withCredentials: api.api.withCredentials,
      onEvent: (e) => feed.publish(e),
      onResync: () => feed.publish(null),
    }).start();
    return () => stream.close();
  }, [api, feed, openEventSource]);

  const state = useMemo<MageState>(() => {
    const base = basePath.replace(/\/+$/, "");
    return { host, api, role, link: (path) => `${base}/${path.replace(/^\/+/, "")}`, can: (min) => atLeast(role, min), openEventSource, feed };
  }, [host, api, role, basePath, openEventSource, feed]);

  return <MageContext.Provider value={state}>{children}</MageContext.Provider>;
}

/**
 * Calls `onChange` when the org feed reports a change `matches` accepts, or after a resync.
 * Bursts are coalesced into one call per `delayMs`.
 */
export function useFeed(matches: (event: FeedEvent) => boolean, onChange: () => void, delayMs = 250): void {
  const { feed } = useMage();
  const matchesRef = useRef(matches);
  matchesRef.current = matches;
  const onChangeRef = useRef(onChange);
  onChangeRef.current = onChange;
  useEffect(() => {
    let timer: number | undefined;
    const unsubscribe = feed.subscribe((event) => {
      if (event !== null && !matchesRef.current(event)) return;
      if (timer !== undefined) return;
      timer = window.setTimeout(() => {
        timer = undefined;
        onChangeRef.current();
      }, delayMs);
    });
    return () => {
      unsubscribe();
      window.clearTimeout(timer);
    };
  }, [feed, delayMs]);
}
