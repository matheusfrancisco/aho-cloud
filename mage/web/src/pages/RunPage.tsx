// Run view (server.md "UI"): status timeline, gates and attempts, the live transcript, steering,
// cancel, usage against the budget, the result, and the artifacts.
import { Badge, FileViewer, Loading, Time, Transcript, useAction, useLoad } from "@aho/ui";
import { useMemo, useState } from "react";
import { Link, useParams } from "react-router";
import type { OrgApi } from "../api/client";
import { ArtifactsPanel, TextArtifact } from "../components/Artifacts";
import { CitationList } from "../components/Citations";
import { MageError, RunStatusBadge, Sha } from "../components/common";
import { useMage } from "../context";
import type { ArtifactView, Budget, Gate, RunDetail, RunResult, RunSubject, Usage, VerifyResult } from "../gen/mage";
import { formatConfidence, formatCost, formatDuration } from "../lib/format";
import { GATE_LABEL, GATE_STATUS_TONE, HARNESS_LABEL, RUN_KIND_LABEL, RUN_STATUS_LABEL, TERMINAL } from "../lib/labels";
import { Markdown } from "../lib/markdown";
import { latestUsage, statusTimeline, toTranscript } from "../lib/runEvents";
import { useRunLive } from "../lib/useRunLive";

/** Steer text limit (`SteerRequest::MAX_TEXT_BYTES`). */
const STEER_MAX_BYTES = 8 * 1024;

function Subject({ subject }: { subject: RunSubject }) {
  const { api, link } = useMage();
  const cause = subject.type === "cause" ? subject.data : null;
  // A cause has no page of its own: it is shown on its ticket.
  const owner = useLoad(() => (cause === null ? Promise.resolve(null) : api.api.resolve(cause)), [api, cause]);
  switch (subject.type) {
    case "ticket":
      return <Link to={link(`tickets/${subject.data}`)}>Ticket</Link>;
    case "cause": {
      const ticket = owner.data?.ticket;
      return ticket === undefined ? <span>Cause {subject.data}</span> : <Link to={`${link(`tickets/${ticket}`)}?cause=${encodeURIComponent(subject.data)}`}>Cause on its ticket</Link>;
    }
    case "repo_version":
      return (
        <>
          <Link to={link(`repos/${subject.data.repo_id}`)}>Repo</Link> at <Sha sha={subject.data.sha} />
        </>
      );
    case "run":
      return (
        <>
          Checks <Link to={link(`runs/${subject.data}`)}>{subject.data}</Link>
        </>
      );
  }
}

/** Field-wise newest usage: the run row, or a later cumulative `Usage` event. */
function newestUsage(run: Usage, event: Usage | null): Usage {
  if (event === null) return run;
  return {
    input_tokens: Math.max(run.input_tokens, event.input_tokens),
    output_tokens: Math.max(run.output_tokens, event.output_tokens),
    cost_micros: Math.max(run.cost_micros, event.cost_micros),
    turns: Math.max(run.turns, event.turns),
    wall: Math.max(run.wall, event.wall),
  };
}

function UsageMeter({ usage, budget }: { usage: Usage; budget: Budget }) {
  const rows: Array<[string, number, number, string]> = [
    ["Cost", usage.cost_micros, budget.max_cost_micros, `${formatCost(usage.cost_micros)} of ${formatCost(budget.max_cost_micros)}`],
    ["Turns", usage.turns, budget.max_turns, `${usage.turns} of ${budget.max_turns}`],
    ["Wall time", usage.wall, budget.max_wall, `${formatDuration(usage.wall)} of ${formatDuration(budget.max_wall)}`],
  ];
  return (
    <div className="meter" role="group" aria-label="Usage against budget">
      {rows.map(([label, value, max, text]) => (
        <div key={label} style={{ display: "contents" }}>
          <span>{label}</span>
          <progress max={Math.max(max, 1)} value={Math.min(value, max)} aria-label={label} />
          <span>{text}</span>
        </div>
      ))}
      <span>Tokens</span>
      <span />
      <span>
        {usage.input_tokens.toLocaleString()} in · {usage.output_tokens.toLocaleString()} out
      </span>
    </div>
  );
}

function GateTable({ gates }: { gates: readonly Gate[] }) {
  const { link } = useMage();
  if (gates.length === 0) return <p className="muted">No gates yet.</p>;
  return (
    <table aria-label="Gates">
      <thead>
        <tr>
          <th>Gate</th>
          <th>Attempt</th>
          <th>Status</th>
          <th>Verify run</th>
        </tr>
      </thead>
      <tbody>
        {gates.map((g) => (
          <tr key={g.id}>
            <td>{GATE_LABEL[g.kind]}</td>
            <td>{g.attempt}</td>
            <td>
              <Badge tone={GATE_STATUS_TONE[g.status.type]}>{g.status.type}</Badge>
              {g.status.type === "failed" && <div className="small">{g.status.reason}</div>}
            </td>
            <td>{g.verify_run === null ? <span className="muted">—</span> : <Link to={link(`runs/${g.verify_run}`)}>{g.verify_run}</Link>}</td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

function VerifyDetails({ result }: { result: VerifyResult }) {
  const o = result.outcome;
  return (
    <>
      <p>
        <Badge tone={o.type === "completed" ? "ok" : "bad"}>{o.type.replaceAll("_", " ")}</Badge> {o.type !== "completed" && o.reason}
      </p>
      {result.observed !== null && (
        <p>
          Observed ({result.observed.kind.replaceAll("_", " ")}): <code>{result.observed.value}</code>
        </p>
      )}
      {result.attempts.length > 0 && (
        <table aria-label="Attempts">
          <thead>
            <tr>
              <th>#</th>
              <th>Kind</th>
              <th>Script</th>
              <th>Exit</th>
              <th>Health</th>
              <th>Duration</th>
            </tr>
          </thead>
          <tbody>
            {result.attempts.map((a) => (
              <tr key={a.index}>
                <td>{a.index}</td>
                <td>{a.kind.replaceAll("_", " ")}</td>
                <td>{a.script_passed ? <Badge tone="ok">passed</Badge> : <Badge tone="bad">failed</Badge>}</td>
                <td>{a.script_exit ?? "—"}</td>
                <td>{a.health_ok ? "ok" : "unhealthy"}</td>
                <td>{formatDuration(a.duration_ms)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      {result.fix_checks !== null && (
        <dl className="facts">
          <dt>Boot after patch</dt>
          <dd>{result.fix_checks.boot ? "ok" : "failed"}</dd>
          <dt>Tests exit</dt>
          <dd>{result.fix_checks.test_exit ?? "—"}</dd>
          <dt>Lint exit</dt>
          <dd>{result.fix_checks.lint_exit ?? "—"}</dd>
        </dl>
      )}
    </>
  );
}

function ResultView({ result, api, artifacts }: { result: RunResult; api: OrgApi; artifacts: readonly ArtifactView[] }) {
  switch (result.kind) {
    case "learn":
      return (
        <>
          <Markdown source={result.notes} />
          <FileViewer file={{ path: ".mage/env.toml", size: new TextEncoder().encode(result.toml).length, text: result.toml, truncated: false }} />
        </>
      );
    case "triage":
      return (
        <>
          <Markdown source={result.summary} />
          {result.causes.length > 0 && (
            <ol>
              {result.causes.map((c, i) => (
                <li key={i}>
                  {c.title} <span className="muted">({formatConfidence(c.confidence)})</span>
                </li>
              ))}
            </ol>
          )}
          {result.missing_info.length > 0 && (
            <>
              <h3>Missing information</h3>
              <ul>
                {result.missing_info.map((q, i) => (
                  <li key={i}>{q}</li>
                ))}
              </ul>
            </>
          )}
        </>
      );
    case "diagnose": {
      const v = result.verdict;
      return (
        <>
          <p>
            Verdict: <Badge tone={v.type === "reproduced" ? "ok" : "warn"}>{v.type.replaceAll("_", " ")}</Badge> {v.type === "inconclusive" && v.reason}
          </p>
          {result.signature !== null && (
            <p>
              Signature ({result.signature.kind.replaceAll("_", " ")}, {result.signature.matcher}): <code>{result.signature.value}</code>
            </p>
          )}
          <Markdown source={result.summary} />
          {result.suggested_fix !== null && (
            <>
              <h3>Suggested fix</h3>
              <Markdown source={result.suggested_fix} />
            </>
          )}
          <CitationList citations={result.fix_sites} repo={null} label="Fix sites" />
          {result.script !== null && (
            <p>
              Repro script: <a href={`#artifact-${result.script}`}>see artifacts</a>
            </p>
          )}
        </>
      );
    }
    case "verify":
      return <VerifyDetails result={result} />;
    case "fix": {
      const patch = artifacts.find((a) => a.id === result.patch);
      return (
        <>
          <p>
            {result.stats.files} file{result.stats.files === 1 ? "" : "s"} changed, <span className="ok">+{result.stats.added}</span> <span className="muted">−{result.stats.removed}</span>
          </p>
          <blockquote>
            <Markdown source={result.summary} />
          </blockquote>
          {result.excluded.length > 0 && (
            <>
              <h3>Excluded paths</h3>
              <ul>
                {result.excluded.map((p) => (
                  <li key={p}>
                    <code>{p}</code>
                  </li>
                ))}
              </ul>
            </>
          )}
          {patch !== undefined && (
            <>
              <a href={api.artifactUrl(patch.id)} download={patch.name}>
                Download .patch
              </a>
              <TextArtifact api={api} artifact={patch} />
            </>
          )}
        </>
      );
    }
  }
}

function Steer({ api, run, enabled }: { api: OrgApi; run: string; enabled: boolean }) {
  const [text, setText] = useState("");
  const [sent, setSent] = useState(false);
  const bytes = new TextEncoder().encode(text).length;
  const send = useAction(async () => {
    await api.steer(run, { text });
    setText("");
    setSent(true);
  });
  return (
    <form
      className="steer stack"
      onSubmit={(e) => {
        e.preventDefault();
        void send.run();
      }}
    >
      <label>
        Steer the agent
        <textarea
          value={text}
          onChange={(e) => {
            setText(e.target.value);
            setSent(false);
          }}
          disabled={!enabled}
          placeholder={enabled ? "Guidance for the agent's next step" : "Steering is possible while the run is running"}
        />
      </label>
      {bytes > STEER_MAX_BYTES && <p className="field-error">At most 8 KiB ({bytes} bytes now).</p>}
      <div className="row">
        <button type="submit" disabled={!enabled || send.busy || text.trim() === "" || bytes > STEER_MAX_BYTES}>
          {send.busy ? "Sending…" : "Send"}
        </button>
        {sent && <span className="muted small">Sent; the agent reads it on its next turn.</span>}
      </div>
      <MageError error={send.error} />
    </form>
  );
}

function CancelRun({ api, run, onDone }: { api: OrgApi; run: string; onDone: () => void }) {
  const [confirming, setConfirming] = useState(false);
  const cancel = useAction(async () => {
    await api.cancel(run);
    setConfirming(false);
    onDone();
  });
  return (
    <>
      {confirming ? (
        <>
          <span>Cancel this run?</span>
          <button type="button" className="danger small" onClick={() => void cancel.run()} disabled={cancel.busy}>
            {cancel.busy ? "Cancelling…" : "Confirm cancel"}
          </button>
          <button type="button" className="secondary small" onClick={() => setConfirming(false)}>
            Keep running
          </button>
        </>
      ) : (
        <button type="button" className="secondary" onClick={() => setConfirming(true)}>
          Cancel run
        </button>
      )}
      <MageError error={cancel.error} />
    </>
  );
}

function RunView({ run }: { run: string }) {
  const { api, link } = useMage();
  const live = useRunLive(run);
  const artifacts = useLoad(() => api.runArtifacts(run), [api, run, live.changes]);
  const messages = useMemo(() => toTranscript(live.events), [live.events]);
  const timeline = useMemo(() => statusTimeline(live.events), [live.events]);
  const detail: RunDetail | undefined = live.detail.data;
  if (detail === undefined) return live.detail.loading ? <Loading what="the run" /> : <MageError error={live.detail.error} />;
  const r = detail.run;
  const status = live.status ?? r.status;
  const terminal = TERMINAL[status];
  const usage = newestUsage(r.usage, latestUsage(live.events));

  return (
    <div className="mage-page">
      <header className="mage-head">
        <h1>{RUN_KIND_LABEL[r.kind]} run</h1>
        <RunStatusBadge status={status} />
        {r.harness !== null && <Badge tone="muted">{HARNESS_LABEL[r.harness]}</Badge>}
        <span className="muted small">attempt {r.attempt}</span>
        <span className="grow" />
        {!terminal && <CancelRun api={api} run={run} onDone={live.detail.reload} />}
      </header>
      <p className="crumbs">
        <Subject subject={r.subject} /> · <code>{r.id}</code>
        {r.parent !== null && (
          <>
            {" "}
            · parent <Link to={link(`runs/${r.parent}`)}>{r.parent}</Link>
          </>
        )}
      </p>
      <MageError error={live.detail.error} />
      {r.error !== null && (
        <div className="error" role="alert">
          {r.error.code.type === "runner" ? `runner failed at ${r.error.code.stage}` : r.error.code.type.replaceAll("_", " ")}: {r.error.message}
        </div>
      )}

      <div className="run-split">
        <section className="card stack" aria-label="Transcript">
          <h2>Transcript</h2>
          <MageError error={live.eventsError} />
          {!live.historyLoaded && live.eventsError === null && <Loading what="events" />}
          {live.historyLoaded && messages.length === 0 && <p className="muted">No events yet.</p>}
          <Transcript messages={messages} assistantLabel={r.harness === null ? "runner" : HARNESS_LABEL[r.harness]} label="Run events" />
          {!terminal && <Steer api={api} run={run} enabled={status === "running"} />}
        </section>

        <div className="stack">
          <section className="card stack" aria-label="Progress">
            <h2>Progress</h2>
            <UsageMeter usage={usage} budget={r.budget} />
            {timeline.length > 0 ? (
              <ol className="timeline" aria-label="Status timeline">
                {timeline.map((s) => (
                  <li key={s.seq}>
                    <div className="row">
                      <span>{RUN_STATUS_LABEL[s.status]}</span>
                      <Time ms={Date.parse(s.at)} />
                    </div>
                  </li>
                ))}
              </ol>
            ) : (
              <p className="muted small">
                Created <Time ms={Date.parse(r.created_at)} />
              </p>
            )}
            <h3>Gates</h3>
            <GateTable gates={detail.gates} />
            {detail.verify_runs.length > 0 && (
              <>
                <h3>Verify runs</h3>
                <ul>
                  {detail.verify_runs.map((v) => (
                    <li key={v.id}>
                      <Link to={link(`runs/${v.id}`)}>{v.id}</Link> <RunStatusBadge status={v.status} />
                    </li>
                  ))}
                </ul>
              </>
            )}
          </section>

          {r.result !== null && (
            <section className="card stack" aria-label="Result">
              <h2>Result</h2>
              <ResultView result={r.result} api={api} artifacts={artifacts.data?.artifacts ?? []} />
            </section>
          )}

          <section className="card stack" aria-label="Artifacts">
            <h2>Artifacts</h2>
            <MageError error={artifacts.error} />
            {artifacts.data === undefined ? artifacts.loading && <Loading what="artifacts" /> : <ArtifactsPanel api={api} artifacts={artifacts.data.artifacts} />}
          </section>
        </div>
      </div>
    </div>
  );
}

export function RunPage() {
  const run = useParams().run ?? "";
  // Another run is another page: the event log does not carry over.
  return <RunView key={run} run={run} />;
}
