// The organization's home: its projects with a status each, its sandboxes, a terminal quick
// start, and the latest ledger activity across the org.
import { type RefObject, useEffect, useRef, useState } from "react";
import { Link, useParams } from "react-router";
import { Ago, Badge, CopyButton, EmptyIllustration, ErrorBox, ExternalIcon, Loading, PlusIcon, SearchIcon, useLoad } from "@aho/ui";
import { StatusBadge } from "../components/ApprovalCard";
import { describeEntry } from "../components/LedgerTimeline";
import { useShell } from "../components/Shell";
import { api } from "../lib/api";
import { previewUrl, setCurrentOrg } from "../lib/remembered";
import { type Activity, filterRows, PROJECT_STATUSES, type ProjectRow, projectRow, type ProjectStatus, recentActivity, statusCounts } from "../lib/status";
import { SANDBOX_PITCH, SandboxStateBadge } from "./SandboxesPage";

const STATUS_LABEL: Record<ProjectStatus, string> = { live: "Live", preview: "Preview", building: "Building", idle: "Idle" };
const STATUS_TONE: Record<ProjectStatus, "ok" | "info" | "warn" | "muted"> = { live: "ok", preview: "info", building: "warn", idle: "muted" };

export function ProjectStatusBadge({ status }: { status: ProjectStatus }) {
  return <Badge tone={STATUS_TONE[status]}>{STATUS_LABEL[status]}</Badge>;
}

/** What the CLI calls when it logs in; `ahocloud login` alone targets this one. */
const DEFAULT_API_URL = "https://api.aho.cloud";

function TerminalCard({ project }: { project: string }) {
  const origin = import.meta.env.VITE_AHO_CLOUD_API || window.location.origin;
  const snippet = `${origin === DEFAULT_API_URL ? "ahocloud login" : `ahocloud login --api-url ${origin}`}\nahocloud -p ${project} deploy`;
  return (
    <section className="card side-card" aria-label="Deploy from your terminal">
      <h2>Deploy from your terminal</h2>
      <p className="hint">Bind this machine once, then ship the current directory as a release.</p>
      <div className="snippet">
        <pre>{snippet}</pre>
        <CopyButton text={snippet} label="Copy commands" />
      </div>
    </section>
  );
}

/** The dashboard shows this many sandboxes; the Sandboxes page lists them all. */
const SANDBOXES_SHOWN = 5;

function SandboxesCard({ org }: { org: string }) {
  const { createSandbox } = useShell();
  const list = useLoad(() => api.sandboxes(org), [org]);
  const sandboxes = list.data ?? [];
  return (
    <section className="card side-card flush" aria-label="Sandboxes">
      <div className="card-title row">
        <h2>Sandboxes</h2>
        {list.data !== undefined && <span className="chip-count">{sandboxes.length}</span>}
        <span className="grow" />
        <Link to={`/orgs/${encodeURIComponent(org)}/sandboxes`} className="small">
          View all
        </Link>
      </div>
      <ErrorBox error={list.error} />
      {list.data === undefined ? null : sandboxes.length === 0 ? (
        <div className="pad">
          <p className="hint">{SANDBOX_PITCH}</p>
          <button type="button" className="soft" onClick={createSandbox}>
            <PlusIcon /> New sandbox
          </button>
        </div>
      ) : (
        <ul className="activity">
          {sandboxes.slice(0, SANDBOXES_SHOWN).map((s) => (
            <li key={s.slug} className="row small">
              <Link to={`/sandboxes/${encodeURIComponent(s.slug)}`}>
                <strong>{s.slug}</strong>
              </Link>
              <SandboxStateBadge state={s.state} />
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}

function ActivityCard({ activity }: { activity: Activity[] }) {
  return (
    <section className="card side-card flush" aria-label="Recent activity">
      <h2 className="card-title">Recent activity</h2>
      {activity.length === 0 ? (
        <p className="muted pad">No recent activity.</p>
      ) : (
        <ol className="activity">
          {activity.map(({ project, entry }) => (
            <li key={`${project}:${entry.id}`}>
              <div className="row small">
                <Link to={`/projects/${encodeURIComponent(project)}`}>
                  <strong>{project}</strong>
                </Link>
                <span className="muted">
                  <Ago ms={entry.at} />
                </span>
              </div>
              <div className="activity-text">{describeEntry(project, entry.body)}</div>
            </li>
          ))}
        </ol>
      )}
    </section>
  );
}

function Row({ row }: { row: ProjectRow }) {
  const { project, latest, status } = row;
  const preview = status === "live" || status === "preview" ? previewUrl(project.slug) : null;
  const link = `/projects/${encodeURIComponent(project.slug)}`;
  return (
    <li className="project-row">
      <div className="project-main">
        <Link to={link} className="project-name">
          {project.slug}
        </Link>
        <ProjectStatusBadge status={status} />
        {project.domains.length > 0 && <span className="muted small">{project.domains.map((d) => d.hostname).join(", ")}</span>}
      </div>
      <div className="project-meta">
        {preview !== null && (
          <a href={preview} target="_blank" rel="noreferrer" className="small">
            Preview <ExternalIcon />
          </a>
        )}
        {latest === null ? (
          <span className="muted small">No releases yet</span>
        ) : (
          <span className="small">
            <Link to={`${link}/releases/${encodeURIComponent(latest.id)}`}>
              <code>{latest.id}</code>
            </Link>{" "}
            <StatusBadge status={latest.status} />
          </span>
        )}
      </div>
    </li>
  );
}

/** Focuses `input` on `/` unless the user is typing somewhere. */
function useSlashFocus(input: RefObject<HTMLInputElement | null>): void {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "/" || e.ctrlKey || e.metaKey || e.altKey) return;
      const t = e.target as HTMLElement | null;
      if (t !== null && (t.isContentEditable || ["INPUT", "TEXTAREA", "SELECT"].includes(t.tagName))) return;
      e.preventDefault();
      input.current?.focus();
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [input]);
}

export function OrgPage() {
  const org = useParams().org ?? "";
  const { createProject } = useShell();
  useEffect(() => setCurrentOrg(org), [org]);
  const loaded = useLoad(async () => {
    const projects = await api.projects(org);
    const details = await Promise.all(
      projects.map(async (p) => {
        const [releases, entries] = await Promise.all([api.releases(p.slug), api.ledger(p.slug)]);
        return { row: projectRow(p, releases), ledger: { project: p.slug, entries } };
      }),
    );
    return { rows: details.map((d) => d.row), activity: recentActivity(details.map((d) => d.ledger), 10) };
  }, [org]);
  const [query, setQuery] = useState("");
  const [status, setStatus] = useState<ProjectStatus | null>(null);
  const search = useRef<HTMLInputElement>(null);
  useSlashFocus(search);

  const rows = loaded.data?.rows ?? [];
  const counts = statusCounts(rows);
  const shown = filterRows(rows, query, status);

  return (
    <div className="dashboard">
      <section className="card projects-card flush" aria-label="Projects">
        <header className="card-head">
          <h1>Projects</h1>
          <label className="search">
            <SearchIcon />
            <input ref={search} type="search" value={query} onChange={(e) => setQuery(e.target.value)} placeholder="Search projects…" aria-label="Search projects" />
            <kbd>/</kbd>
          </label>
        </header>
        <div className="chips" role="group" aria-label="Filter by status">
          {PROJECT_STATUSES.map((s) => (
            <button key={s} type="button" className={`chip chip-${STATUS_TONE[s]}`} aria-pressed={status === s} onClick={() => setStatus(status === s ? null : s)}>
              {STATUS_LABEL[s]} <span className="chip-count">{counts[s]}</span>
            </button>
          ))}
        </div>
        <ErrorBox error={loaded.error} />
        {loaded.data === undefined ? (
          loaded.loading && <Loading what="projects" />
        ) : rows.length === 0 ? (
          <div className="empty">
            <EmptyIllustration />
            <p>You don't have any projects yet</p>
            <button type="button" className="soft" onClick={createProject}>
              <PlusIcon /> Create a project
            </button>
          </div>
        ) : shown.length === 0 ? (
          <p className="muted pad">No projects match.</p>
        ) : (
          <ul className="project-list">
            {shown.map((r) => (
              <Row key={r.project.slug} row={r} />
            ))}
          </ul>
        )}
      </section>
      <div className="side-column">
        <SandboxesCard org={org} />
        <TerminalCard project={rows[0]?.project.slug ?? "<project>"} />
        <ActivityCard activity={loaded.data?.activity ?? []} />
      </div>
    </div>
  );
}
