// A run followed live inside another page (onboarding wizard): status, gates and the transcript,
// with a link to the full run view.
import { Loading, Transcript } from "@aho/ui";
import { useEffect, useMemo, useRef } from "react";
import { Link } from "react-router";
import { useMage } from "../context";
import type { RunStatus } from "../gen/mage";
import { HARNESS_LABEL, RUN_KIND_LABEL, TERMINAL } from "../lib/labels";
import { toTranscript } from "../lib/runEvents";
import { useRunLive } from "../lib/useRunLive";
import { MageError, RunStatusBadge } from "./common";

/** `onStatus` is called with each newly seen status (e.g. to refetch what a terminal run changed). */
export function LiveRunPanel({ run, onStatus }: { run: string; onStatus?: (status: RunStatus) => void }) {
  const { link } = useMage();
  const live = useRunLive(run);
  const messages = useMemo(() => toTranscript(live.events), [live.events]);
  const onStatusRef = useRef(onStatus);
  onStatusRef.current = onStatus;
  const status = live.status;
  useEffect(() => {
    if (status !== undefined) onStatusRef.current?.(status);
  }, [status]);
  const detail = live.detail.data;
  if (detail === undefined) return live.detail.loading ? <Loading what="the run" /> : <MageError error={live.detail.error} />;
  const r = detail.run;
  return (
    <div className="stack" aria-label={`${RUN_KIND_LABEL[r.kind]} run`} role="region">
      <div className="row">
        <strong>{RUN_KIND_LABEL[r.kind]} run</strong>
        <RunStatusBadge status={status ?? r.status} />
        <span className="grow" />
        <Link to={link(`runs/${r.id}`)}>Open run view</Link>
      </div>
      {r.error !== null && <div className="error">{r.error.message}</div>}
      <MageError error={live.eventsError} />
      {!TERMINAL[status ?? r.status] && messages.length === 0 && <p className="muted">Waiting for the runner…</p>}
      <div className="run-split-transcript">
        <Transcript messages={messages} assistantLabel={r.harness === null ? "runner" : HARNESS_LABEL[r.harness]} label="Run events" />
      </div>
    </div>
  );
}
