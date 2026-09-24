import { useCallback, useEffect, useRef, useState } from "react";

export interface Loaded<T> {
  data: T | undefined;
  error: unknown;
  loading: boolean;
  reload: () => void;
}

/** Runs `load` on mount and whenever `deps` change; stale results from superseded runs are dropped. */
export function useLoad<T>(load: () => Promise<T>, deps: readonly unknown[]): Loaded<T> {
  const [data, setData] = useState<T>();
  const [error, setError] = useState<unknown>(null);
  const [loading, setLoading] = useState(true);
  const [tick, setTick] = useState(0);
  const run = useRef(0);
  const loadRef = useRef(load);
  loadRef.current = load;

  useEffect(() => {
    const mine = ++run.current;
    setLoading(true);
    loadRef.current().then(
      (v) => {
        if (run.current !== mine) return;
        setData(v);
        setError(null);
        setLoading(false);
      },
      (e: unknown) => {
        if (run.current !== mine) return;
        setError(e);
        setLoading(false);
      },
    );
    // `deps` is the caller's dependency list; `load` is read through a ref so it may be inline.
  }, [...deps, tick]);

  const reload = useCallback(() => setTick((t) => t + 1), []);
  return { data, error, loading, reload };
}

export interface Action<A extends unknown[]> {
  run: (...args: A) => Promise<void>;
  busy: boolean;
  error: unknown;
  clear: () => void;
}

/** Wraps a user-triggered async action: one at a time, error kept for display. */
export function useAction<A extends unknown[]>(fn: (...args: A) => Promise<void>): Action<A> {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<unknown>(null);
  const fnRef = useRef(fn);
  fnRef.current = fn;
  const run = useCallback(async (...args: A) => {
    setBusy(true);
    setError(null);
    try {
      await fnRef.current(...args);
    } catch (e) {
      setError(e);
    } finally {
      setBusy(false);
    }
  }, []);
  const clear = useCallback(() => setError(null), []);
  return { run, busy, error, clear };
}
