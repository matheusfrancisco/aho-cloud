// The signed-in console session: bearer token and user, kept in sessionStorage so a tab's login
// ends with the tab.
import { useSyncExternalStore } from "react";
import type { Session, User } from "../gen/api";

const TOKEN_KEY = "aho-cloud.token";
const USER_KEY = "aho-cloud.user";

const listeners = new Set<() => void>();
let snapshot: Session | null | undefined;

function read(): Session | null {
  const token = sessionStorage.getItem(TOKEN_KEY);
  const user = sessionStorage.getItem(USER_KEY);
  if (token === null || user === null) return null;
  try {
    return { token, user: JSON.parse(user) as User };
  } catch {
    return null;
  }
}

function changed(): void {
  snapshot = read();
  for (const l of listeners) l();
}

/** The current session, or `null` when signed out. */
export function currentSession(): Session | null {
  if (snapshot === undefined) snapshot = read();
  return snapshot;
}

/** Bearer token for API calls, or `null` when signed out. */
export function currentToken(): string | null {
  return currentSession()?.token ?? null;
}

export function signIn(session: Session): void {
  sessionStorage.setItem(TOKEN_KEY, session.token);
  sessionStorage.setItem(USER_KEY, JSON.stringify(session.user));
  changed();
}

/** Drops the token; the server session expires on its own. */
export function signOut(): void {
  sessionStorage.removeItem(TOKEN_KEY);
  sessionStorage.removeItem(USER_KEY);
  changed();
}

export function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

/** React view of the session; re-renders on sign-in and sign-out. */
export function useSession(): Session | null {
  return useSyncExternalStore(subscribe, currentSession, currentSession);
}
