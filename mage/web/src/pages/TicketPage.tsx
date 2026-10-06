// Ticket detail (server.md "UI", fix.md "Storage and exposure"): Linear/Slack links, customer,
// target with override, the sanitized body, ranked causes with GitHub permalinks and actions
// (Run diagnostic, fix brief with Copy/Download, Let Mage fix, Open PR), runs, pull requests and
// Resolve. The org feed refreshes what changed.
import { Ago, Badge, CopyButton, Loading, useAction, useLoad } from "@aho/ui";
import { useEffect, useRef, useState } from "react";
import { Link, useNavigate, useParams, useSearchParams } from "react-router";
import type { OrgApi } from "../api/client";
import { CitationList } from "../components/Citations";
import { ExternalLink, MageError, RunStatusBadge, Sha, Tabs } from "../components/common";
import { useFeed, useMage } from "../context";
import type { Cause, FixPullRequest, HarnessKind, OrgSettings, TargetOrigin, TicketDetail } from "../gen/mage";
import { formatConfidence, slackThreadUrl } from "../lib/format";
import { CAUSE_STATUS_LABEL, CAUSE_STATUS_TONE, HARNESS_LABEL, HARNESSES, RUN_KIND_LABEL, TICKET_STATUS_LABEL, TICKET_STATUS_TONE } from "../lib/labels";
import { Markdown, safeHref } from "../lib/markdown";

const SHA_RE = /^[0-9a-f]{40}$/;

function originLabel(o: TargetOrigin): string {
  switch (o.type) {
    case "override":
      return "set in Mage";
    case "directive":
      return "from the ticket's mage-sha/mage-ref line";
    case "deployment":
      return `deployed to ${o.environment}`;
    case "default_head":
      return "default-branch head";
  }
}

function HarnessPicker({ value, onChange, label }: { value: HarnessKind; onChange: (h: HarnessKind) => void; label: string }) {
  return (
    <select aria-label={label} value={value} onChange={(e) => onChange(HARNESSES.find((h) => h === e.target.value) ?? value)}>
      {HARNESSES.map((h) => (
        <option key={h} value={h}>
          {HARNESS_LABEL[h]}
        </option>
      ))}
    </select>
  );
}

function TargetEditor({ api, detail, onSaved }: { api: OrgApi; detail: TicketDetail; onSaved: () => void }) {
  const repos = useLoad(() => api.repos(), [api]);
  const target = detail.ticket.target;
  const [repo, setRepo] = useState(target?.repo_id ?? "");
  const [sha, setSha] = useState("");
  const shaOk = SHA_RE.test(sha);
  const save = useAction(async () => {
    await api.setTarget(detail.ticket.id, { repo_id: repo, sha });
    onSaved();
  });
  return (
    <form
      className="row"
      aria-label="Change target"
      onSubmit={(e) => {
        e.preventDefault();
        if (repo !== "" && shaOk) void save.run();
      }}
    >
      <select aria-label="Repo" value={repo} onChange={(e) => setRepo(e.target.value)}>
        <option value="">Choose a repo</option>
        {(repos.data?.repos ?? []).map((r) => (
          <option key={r.repo.id} value={r.repo.id}>
            {r.repo.owner}/{r.repo.name}
          </option>
        ))}
      </select>
      <input className="mage-mono grow" aria-label="Commit sha" placeholder="40-character commit sha" value={sha} onChange={(e) => setSha(e.target.value.trim().toLowerCase())} aria-invalid={sha !== "" && !shaOk} spellCheck={false} />
      <button type="submit" disabled={save.busy || repo === "" || !shaOk}>
        {save.busy ? "Saving…" : "Set target"}
      </button>
      <MageError error={save.error} />
    </form>
  );
}

function FixBrief({ api, ticket, cause }: { api: OrgApi; ticket: string; cause: string }) {
  const [version, setVersion] = useState<number | undefined>(undefined);
  const brief = useLoad(() => api.fixBrief(ticket, cause, { version }), [api, ticket, cause, version]);
  if (brief.data === undefined) return brief.loading ? <Loading what="the fix brief" /> : <MageError error={brief.error} />;
  const b = brief.data;
  const versions = Array.from({ length: b.latest_version }, (_, i) => b.latest_version - i);
  // The brief's immutable artifact: a Mage path, or an absolute http(s) URL.
  const download = b.artifact_url.startsWith("/") && !b.artifact_url.startsWith("//") ? api.api.url(b.artifact_url) : safeHref(b.artifact_url);
  return (
    <div className="stack">
      <div className="row">
        <label className="row">
          Version
          <select value={b.version} onChange={(e) => setVersion(Number(e.target.value))}>
            {versions.map((v) => (
              <option key={v} value={v}>
                v{v}
                {v === b.latest_version ? " (latest)" : ""}
              </option>
            ))}
          </select>
        </label>
        <span className="muted small">
          <Ago ms={Date.parse(b.created_at)} />
        </span>
        <span className="grow" />
        <CopyButton text={b.markdown} label="Copy brief" />
        {download !== null && (
          <a href={download} download={`brief-v${b.version}.md`}>
            Download .md
          </a>
        )}
      </div>
      <Markdown source={b.markdown} />
    </div>
  );
}

function PullRequests({ prs }: { prs: readonly FixPullRequest[] }) {
  if (prs.length === 0) return null;
  return (
    <section className="card stack" aria-label="Pull requests">
      <h2>Pull requests</h2>
      <ul>
        {prs.map((pr) => (
          <li key={pr.id} className="row">
            {pr.url !== null ? <ExternalLink href={pr.url}>#{pr.number}</ExternalLink> : <span className="muted">not opened yet</span>}
            <code>{pr.branch}</code>
            <Badge tone={pr.state.type === "opened" ? "ok" : pr.state.type === "closed" ? "muted" : "info"}>{pr.state.type === "closed" ? (pr.state.merged ? "merged" : "closed") : pr.state.type.replaceAll("_", " ")}</Badge>
          </li>
        ))}
      </ul>
    </section>
  );
}

type CauseTab = "cause" | "brief";

function CauseCard({ api, detail, cause, settings, focused, initialTab, onChanged }: { api: OrgApi; detail: TicketDetail; cause: Cause; settings: OrgSettings | undefined; focused: boolean; initialTab: CauseTab; onChanged: () => void }) {
  const { link } = useMage();
  const navigate = useNavigate();
  const ticket = detail.ticket;
  const [tab, setTab] = useState<CauseTab>(initialTab);
  const [harness, setHarness] = useState<HarnessKind | null>(null);
  const chosen = harness ?? settings?.general.harness ?? "omp";
  const s = cause.status;
  const live = cause.superseded_at === null;
  const reproduced = s.type === "reproduced";
  const canDiagnose = live && (s.type === "untested" || s.type === "not_reproduced" || s.type === "inconclusive") && (ticket.status === "triaged" || ticket.status === "diagnosing");
  const canFix = live && reproduced && settings?.fix.run === true && (ticket.status === "diagnosed" || ticket.status === "fix_ready");
  const fixedBy = detail.runs.find((r) => r.kind === "fix" && r.status === "succeeded" && r.subject.type === "cause" && r.subject.data === cause.id);
  const opened = detail.pull_requests.some((pr) => fixedBy !== undefined && pr.fix_run === fixedBy.id && pr.state.type !== "planned");
  const canOpenPr = live && settings?.fix.pull_requests === true && ticket.status === "fix_ready" && fixedBy !== undefined && !opened;
  const [prQueued, setPrQueued] = useState(false);
  const root = useRef<HTMLElement>(null);
  useEffect(() => {
    // Short links (`/causes/{cause}/brief`) land here with `?cause=`; bring that cause into view.
    if (focused) root.current?.scrollIntoView?.({ block: "start" });
  }, [focused]);

  const diagnose = useAction(async () => {
    const started = await api.diagnose(ticket.id, cause.id, { harness: chosen });
    navigate(link(`runs/${started.run_id}`));
  });
  const fix = useAction(async () => {
    const started = await api.fix(ticket.id, cause.id, { harness: chosen });
    navigate(link(`runs/${started.run_id}`));
  });
  const openPr = useAction(async () => {
    await api.openPullRequest(ticket.id, cause.id);
    setPrQueued(true);
    onChanged();
  });

  const tabs: Array<{ id: CauseTab; label: string }> = reproduced
    ? [
        { id: "cause", label: "Cause" },
        { id: "brief", label: "Fix brief" },
      ]
    : [{ id: "cause", label: "Cause" }];

  return (
    <article ref={root} className={`cause stack${live ? "" : " superseded"}`} id={`cause-${cause.id}`} aria-label={`Cause ${cause.rank}: ${cause.title}`} data-focused={focused || undefined}>
      <header className="row">
        <strong>
          #{cause.rank} {cause.title}
        </strong>
        <span className="muted small">{formatConfidence(cause.confidence)}</span>
        <Badge tone={CAUSE_STATUS_TONE[s.type]}>{CAUSE_STATUS_LABEL[s.type]}</Badge>
        {s.type !== "untested" && (
          <Link className="small" to={link(`runs/${s.type === "running" ? s.data : s.data.run}`)}>
            {s.type === "running" ? "Diagnostic running" : "Diagnostic run"}
          </Link>
        )}
        <span className="muted small">method: {cause.method}</span>
      </header>
      <Tabs label={`Cause ${cause.rank}`} tabs={tabs} selected={reproduced ? tab : "cause"} onSelect={setTab}>
        {tab === "brief" && reproduced ? (
          <div className="stack">
            <FixBrief api={api} ticket={ticket.id} cause={cause.id} />
            {canFix && (
              <div className="row">
                <HarnessPicker value={chosen} onChange={setHarness} label="Fix harness" />
                <button type="button" onClick={() => void fix.run()} disabled={fix.busy}>
                  {fix.busy ? "Starting…" : "Let Mage fix"}
                </button>
              </div>
            )}
            <MageError error={fix.error} />
          </div>
        ) : (
          <div className="stack">
            <Markdown source={cause.explanation} />
            <CitationList citations={cause.evidence} repo={detail.repo} />
            {cause.repro_steps.length > 0 && (
              <>
                <h4>Repro steps</h4>
                <ol>
                  {cause.repro_steps.map((step, i) => (
                    <li key={i}>{step}</li>
                  ))}
                </ol>
              </>
            )}
            <dl className="facts">
              <dt>Confirm if</dt>
              <dd>{cause.confirm_if}</dd>
              <dt>Rule out if</dt>
              <dd>{cause.rule_out_if}</dd>
              {s.type === "inconclusive" && (
                <>
                  <dt>Inconclusive</dt>
                  <dd>{s.data.reason}</dd>
                </>
              )}
            </dl>
            {canDiagnose && (
              <div className="row">
                <HarnessPicker value={chosen} onChange={setHarness} label="Diagnostic harness" />
                <button type="button" onClick={() => void diagnose.run()} disabled={diagnose.busy}>
                  {diagnose.busy ? "Starting…" : "Run diagnostic"}
                </button>
              </div>
            )}
            <MageError error={diagnose.error} />
          </div>
        )}
      </Tabs>
      {canOpenPr && (
        <div className="row">
          <button type="button" onClick={() => void openPr.run()} disabled={openPr.busy || prQueued}>
            {openPr.busy ? "Requesting…" : "Open PR"}
          </button>
          {prQueued && <span className="muted small">Requested; the pull request appears here once GitHub opens it.</span>}
        </div>
      )}
      <MageError error={openPr.error} />
    </article>
  );
}

function ResolveForm({ api, detail, onResolved }: { api: OrgApi; detail: TicketDetail; onResolved: () => void }) {
  const reproduced = detail.causes.find((c) => c.status.type === "reproduced" && c.superseded_at === null);
  const prUrl = detail.pull_requests.find((pr) => pr.url !== null)?.url ?? "";
  const [cause, setCause] = useState(reproduced?.id ?? "");
  const [rootCause, setRootCause] = useState("");
  const [fixPrUrl, setFixPrUrl] = useState(prUrl);
  const resolve = useAction(async () => {
    await api.resolveTicket(detail.ticket.id, {
      root_cause: rootCause.trim(),
      confirmed_cause: cause === "" ? undefined : cause,
      fix_pr_url: fixPrUrl.trim() === "" ? undefined : fixPrUrl.trim(),
    });
    onResolved();
  });
  return (
    <form
      className="stack"
      aria-label="Resolve"
      onSubmit={(e) => {
        e.preventDefault();
        if (rootCause.trim() !== "") void resolve.run();
      }}
    >
      <label>
        Confirmed cause
        <select value={cause} onChange={(e) => setCause(e.target.value)}>
          <option value="">None of these</option>
          {detail.causes.map((c) => (
            <option key={c.id} value={c.id}>
              #{c.rank} {c.title}
            </option>
          ))}
        </select>
      </label>
      <label>
        Root cause
        <textarea value={rootCause} onChange={(e) => setRootCause(e.target.value)} required />
      </label>
      <label>
        Fix PR URL
        <input type="url" value={fixPrUrl} onChange={(e) => setFixPrUrl(e.target.value)} placeholder="https://github.com/…/pull/…" />
      </label>
      <div className="row">
        <button type="submit" disabled={resolve.busy || rootCause.trim() === ""}>
          {resolve.busy ? "Resolving…" : "Resolve"}
        </button>
      </div>
      <MageError error={resolve.error} />
    </form>
  );
}

function TicketView({ id }: { id: string }) {
  const { api, link } = useMage();
  const [params] = useSearchParams();
  const focusCause = params.get("cause");
  const focusTab: CauseTab = params.get("tab") === "brief" ? "brief" : "cause";
  const loaded = useLoad(() => api.ticket(id), [api, id]);
  const settings = useLoad(() => api.settings(), [api]);
  const [editingTarget, setEditingTarget] = useState(false);
  const [resolving, setResolving] = useState(false);
  const retriage = useAction(async () => {
    await api.retriage(id);
    loaded.reload();
  });
  const detail = loaded.data;

  useFeed(
    (e) => (e.kind === "ticket" && e.subject === id) || (e.kind === "cause" && (detail?.causes.some((c) => c.id === e.subject) ?? false)) || (e.kind === "run" && (detail?.runs.some((r) => r.id === e.subject) ?? false)),
    loaded.reload,
  );

  if (detail === undefined) return loaded.loading ? <Loading what="the ticket" /> : <MageError error={loaded.error} />;
  const t = detail.ticket;
  const current = detail.causes.filter((c) => c.superseded_at === null).sort((a, b) => a.rank - b.rank);
  const superseded = detail.causes.filter((c) => c.superseded_at !== null);
  const open = t.status !== "resolved" && t.status !== "closed";
  const changedSinceTriage = t.triaged_hash !== null && t.triaged_hash !== t.content_hash;
  const card = (c: Cause) => <CauseCard key={c.id} api={api} detail={detail} cause={c} settings={settings.data} focused={c.id === focusCause} initialTab={c.id === focusCause ? focusTab : "cause"} onChanged={loaded.reload} />;

  return (
    <div className="mage-page">
      <header className="mage-head">
        <h1>
          {t.source.identifier} {t.title}
        </h1>
        <Badge tone={TICKET_STATUS_TONE[t.status]}>{TICKET_STATUS_LABEL[t.status]}</Badge>
        {changedSinceTriage && <Badge tone="warn">changed since triage</Badge>}
        <span className="grow" />
        <ExternalLink href={t.source.url}>Open in Linear</ExternalLink>
        {t.slack_thread !== null && <ExternalLink href={slackThreadUrl(t.slack_thread)}>Slack thread</ExternalLink>}
        {(t.status === "triaged" || t.status === "triage_failed" || t.status === "triaging") && (
          <button type="button" className="secondary" onClick={() => void retriage.run()} disabled={retriage.busy}>
            {retriage.busy ? "Requesting…" : "Retriage"}
          </button>
        )}
      </header>
      <MageError error={retriage.error} />
      <MageError error={loaded.error} />

      <section className="card stack" aria-label="Details">
        <dl className="facts">
          <dt>Customer</dt>
          <dd>{detail.customer?.name ?? <span className="muted">unknown</span>}</dd>
          <dt>Target</dt>
          <dd>
            {t.target === null ? (
              <span className="note">No repo or commit yet: choose one so triage can start.</span>
            ) : (
              <span className="row">
                {detail.repo !== null && (
                  <span>
                    {detail.repo.owner}/{detail.repo.name}
                  </span>
                )}
                <Sha sha={t.target.sha} />
                {detail.assumed && <Badge tone="warn">assumed</Badge>}
                <span className="muted small">{originLabel(t.target.origin)}</span>
              </span>
            )}
            {open && (
              <button type="button" className="link" onClick={() => setEditingTarget((v) => !v)}>
                {editingTarget ? "Close" : "Change target"}
              </button>
            )}
            {editingTarget && (
              <TargetEditor
                api={api}
                detail={detail}
                onSaved={() => {
                  setEditingTarget(false);
                  loaded.reload();
                }}
              />
            )}
          </dd>
          {t.error_strings.length > 0 && (
            <>
              <dt>Errors quoted</dt>
              <dd>
                <ul className="citations">
                  {t.error_strings.map((e, i) => (
                    <li key={i}>
                      <code>{e}</code>
                    </li>
                  ))}
                </ul>
              </dd>
            </>
          )}
        </dl>
        <Markdown source={t.body} />
      </section>

      <section className="stack" aria-label="Causes">
        <h2>Causes</h2>
        {current.length === 0 ? <p className="muted">{t.status === "triaging" || t.status === "new" ? "Triage has not proposed causes yet." : "No live causes."}</p> : current.map(card)}
        {superseded.length > 0 && (
          <details>
            <summary>Superseded causes ({superseded.length})</summary>
            <div className="stack">{superseded.map(card)}</div>
          </details>
        )}
      </section>

      <PullRequests prs={detail.pull_requests} />

      {detail.runs.length > 0 && (
        <section className="card stack" aria-label="Runs">
          <h2>Runs</h2>
          <table>
            <thead>
              <tr>
                <th>Run</th>
                <th>Status</th>
                <th>Started</th>
              </tr>
            </thead>
            <tbody>
              {detail.runs.map((r) => (
                <tr key={r.id}>
                  <td>
                    <Link to={link(`runs/${r.id}`)}>{RUN_KIND_LABEL[r.kind]}</Link>
                  </td>
                  <td>
                    <RunStatusBadge status={r.status} />
                  </td>
                  <td>
                    <Ago ms={Date.parse(r.created_at)} />
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </section>
      )}

      <section className="card stack" aria-label="Resolution">
        <h2>Resolution</h2>
        {detail.resolution !== null ? (
          <dl className="facts">
            <dt>Root cause</dt>
            <dd>{detail.resolution.root_cause}</dd>
            {detail.resolution.fix_pr_url !== null && (
              <>
                <dt>Fix PR</dt>
                <dd>
                  <ExternalLink href={detail.resolution.fix_pr_url}>{detail.resolution.fix_pr_url}</ExternalLink>
                </dd>
              </>
            )}
            <dt>Resolved</dt>
            <dd>
              <Ago ms={Date.parse(detail.resolution.at)} />
            </dd>
          </dl>
        ) : !open ? (
          <p className="muted">Closed without a recorded resolution.</p>
        ) : resolving ? (
          <ResolveForm
            api={api}
            detail={detail}
            onResolved={() => {
              setResolving(false);
              loaded.reload();
            }}
          />
        ) : (
          <div>
            <button type="button" className="secondary" onClick={() => setResolving(true)}>
              Resolve…
            </button>
          </div>
        )}
      </section>
    </div>
  );
}

export function TicketPage() {
  const id = useParams().ticket ?? "";
  return <TicketView key={id} id={id} />;
}
