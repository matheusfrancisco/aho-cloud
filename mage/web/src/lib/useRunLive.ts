// A run as the run view shows it (server.md UC10): `GET runs/{run}` plus the paged event history
// from `after=0`, then the run SSE stream from the last `seq`. Events land in one log (each `seq`
// once, in order); status, gate and artifact events bump `changes` so the detail is refetched.
import { type Loaded, useLoad } from "@aho/ui";
import { useEffect, useRef, useState } from "react";
import { RunStream } from "../api/sse";
import { useMage } from "../context";
import type { RunDetail, RunEvent, RunId, RunStatus } from "../gen/mage";
import { mergeEvents } from "./runEvents";

const HISTORY_PAGE = 500;
const REFRESH_DELAY_MS = 300;

/** Event kinds after which the run's detail (status, gates, usage, artifacts) is refetched. */
const REFETCH_ON: Record<RunEvent["kind"]["type"], boolean> = {
  status: true,
  gate_started: true,
  gate_result: true,
  artifact: true,
  assistant_text: false,
  tool_call: false,
  tool_result: false,
  usage: false,
  notice: false,
  error: false,
};

export interface LiveRun {
  detail: Loaded<RunDetail>;
  /** The log so far, ordered by `seq`. */
  events: readonly RunEvent[];
  /** Loading the history failed. */
  eventsError: unknown;
  /** The history is loaded (the stream may still deliver more). */
  historyLoaded: boolean;
  /** The server sent `end`: the run is terminal and every event arrived. */
  ended: boolean;
  /** Newest known status: the latest status event, else the detail's. */
  status: RunStatus | undefined;
  /** Bumped (debounced) after events that change the detail; use as a reload key. */
  changes: number;
}

export function useRunLive(run: RunId): LiveRun {
  const { api, openEventSource } = useMage();
  const [events, setEvents] = useState<readonly RunEvent[]>([]);
  const [eventsError, setEventsError] = useState<unknown>(null);
  const [historyLoaded, setHistoryLoaded] = useState(false);
  const [ended, setEnded] = useState(false);
  const [changes, setChanges] = useState(0);
  const timer = useRef<number | undefined>(undefined);
  const detail = useLoad(() => api.run(run), [api, run, changes]);

  useEffect(() => {
    let cancelled = false;
    let stream: RunStream | null = null;
    setEvents([]);
    setEventsError(null);
    setHistoryLoaded(false);
    setEnded(false);
    const changed = () => {
      if (timer.current !== undefined) return;
      timer.current = window.setTimeout(() => {
        timer.current = undefined;
        setChanges((c) => c + 1);
      }, REFRESH_DELAY_MS);
    };
    const load = async () => {
      let after = 0;
      for (;;) {
        const page = await api.runEvents(run, { after, limit: HISTORY_PAGE });
        if (cancelled) return;
        setEvents((prev) => mergeEvents(prev, page.events));
        for (const e of page.events) after = Math.max(after, e.seq);
        if (!page.more || page.events.length === 0) break;
      }
      setHistoryLoaded(true);
      stream = new RunStream({
        url: (seq) => api.runEventsUrl(run, seq),
        after,
        open: openEventSource,
        withCredentials: api.api.withCredentials,
        onEvent: (e) => {
          setEvents((prev) => mergeEvents(prev, [e]));
          if (REFETCH_ON[e.kind.type]) changed();
        },
        onEnd: () => {
          setEnded(true);
          changed();
        },
      }).start();
    };
    load().catch((e: unknown) => {
      if (!cancelled) setEventsError(e);
    });
    return () => {
      cancelled = true;
      stream?.close();
      window.clearTimeout(timer.current);
      timer.current = undefined;
    };
  }, [api, run, openEventSource]);

  let status: RunStatus | undefined = detail.data?.run.status;
  for (let i = events.length - 1; i >= 0; i--) {
    const k = events[i]!.kind;
    if (k.type === "status") {
      status = k.data;
      break;
    }
  }
  return { detail, events, eventsError, historyLoaded, ended, status, changes };
}
