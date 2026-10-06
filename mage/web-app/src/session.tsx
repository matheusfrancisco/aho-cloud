// The signed-in session (`GET /api/v1/session`: user, memberships, CSRF token, feature flags) and
// the API client bound to it. A 401 anywhere means the session is gone: the shell goes to /login.
import { type EventSourceFactory, MageApi, MageApiError, type SessionView } from "@mage/web";
import { createContext, type ReactNode, useCallback, useContext, useEffect, useMemo, useRef, useState } from "react";

export interface SessionState {
  api: MageApi;
  /** `undefined` while loading; `null` when signed out. */
  session: SessionView | null | undefined;
  /** Loading the session failed for another reason than being signed out. */
  error: unknown;
  reload: () => void;
  /** Forget the session locally after a 401: signing in again returns to the current page. */
  signedOut: () => void;
  /** Forget the session after the user logged out: the next sign-in starts at `/`. */
  loggedOut: () => void;
  /** The session ended through `loggedOut`. */
  didLogOut: boolean;
  fetch: typeof fetch;
  eventSource?: EventSourceFactory;
}

const SessionContext = createContext<SessionState | null>(null);

export function useSession(): SessionState {
  const s = useContext(SessionContext);
  if (s === null) throw new Error("useSession outside <SessionProvider>");
  return s;
}

export function SessionProvider({ fetch, eventSource, children }: { fetch: typeof globalThis.fetch; eventSource?: EventSourceFactory; children: ReactNode }) {
  const [session, setSession] = useState<SessionView | null | undefined>(undefined);
  const [error, setError] = useState<unknown>(null);
  const [tick, setTick] = useState(0);
  const sessionRef = useRef(session);
  sessionRef.current = session;
  const [didLogOut, setDidLogOut] = useState(false);
  const signedOut = useCallback(() => setSession(null), []);
  const loggedOut = useCallback(() => {
    setDidLogOut(true);
    setSession(null);
  }, []);
  const api = useMemo(() => new MageApi({ fetch, csrfToken: () => sessionRef.current?.csrf_token ?? null, onUnauthorized: signedOut }), [fetch, signedOut]);

  useEffect(() => {
    let current = true;
    setError(null);
    api.session().then(
      (s) => {
        if (!current) return;
        setSession(s);
        setDidLogOut(false);
      },
      (e: unknown) => {
        if (!current) return;
        // 401 already marked the session signed out; anything else is shown with a retry.
        if (!(e instanceof MageApiError && e.status === 401)) setError(e);
      },
    );
    return () => {
      current = false;
    };
  }, [api, tick]);

  const reload = useCallback(() => setTick((t) => t + 1), []);
  const value = useMemo<SessionState>(() => ({ api, session, error, reload, signedOut, loggedOut, didLogOut, fetch, eventSource }), [api, session, error, reload, signedOut, loggedOut, didLogOut, fetch, eventSource]);
  return <SessionContext.Provider value={value}>{children}</SessionContext.Provider>;
}

/** Only same-app paths are followed after login (server.md: relative, not `//` or `/\`). */
export function safeReturnTo(path: string | null): string {
  return path !== null && path.startsWith("/") && !path.startsWith("//") && !path.startsWith("/\\") ? path : "/";
}

const ORG_KEY = "mage.org.";

/** The org the user last worked in (server.md "Org selection"). */
export function rememberedOrg(userId: string): string | null {
  return localStorage.getItem(ORG_KEY + userId);
}

export function rememberOrg(userId: string, org: string): void {
  localStorage.setItem(ORG_KEY + userId, org);
}
