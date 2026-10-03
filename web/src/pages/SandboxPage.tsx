// A sandbox: its state with Start / Stop / Delete, the public URL when a port is set, and tabs
// for a terminal, a chat with the aho inside, its /workspace files, and its settings. The
// terminal and chat stay connected while other tabs show.
import { lazy, Suspense, useEffect, useState } from "react";
import { Link, useNavigate, useParams } from "react-router";
import { Chat } from "../components/Chat";
import { CodePanel } from "../components/CodePanel";
import { SandboxSettingsPanel } from "../components/SandboxSettings";
import type { Sandbox } from "../gen/api";
import { api } from "../lib/api";
import { setCurrentOrg } from "../lib/remembered";
import { Ago, ErrorBox, Loading } from "../ui/common";
import { useAction, useInterval, useLoad } from "../ui/hooks";
import { ExternalIcon, PlayIcon, StopIcon } from "../ui/icons";
import { Modal } from "../ui/overlay";
import { AutoStop, SandboxStateBadge } from "./SandboxesPage";

// xterm.js is only needed here: load it with the terminal, not with the console.
const SandboxTerminal = lazy(() => import("../components/SandboxTerminal").then((m) => ({ default: m.SandboxTerminal })));

const TABS = ["terminal", "chat", "files", "settings"] as const;
type Tab = (typeof TABS)[number];
const TAB_LABEL: Record<Tab, string> = { terminal: "Terminal", chat: "Chat", files: "Files", settings: "Settings" };
/** Tabs holding a live connection: once opened they stay mounted (hidden) so switching away keeps the session. */
const KEPT: Record<Tab, boolean> = { terminal: true, chat: true, files: false, settings: false };

function NotRunning({ sandbox, onStart, starting }: { sandbox: Sandbox; onStart: () => void; starting: boolean }) {
  return (
    <div className="empty">
      {sandbox.state === "starting" ? (
        <p>The sandbox is starting…</p>
      ) : (
        <>
          <p>The sandbox is {sandbox.state}.</p>
          <button type="button" onClick={onStart} disabled={starting}>
            <PlayIcon /> {starting ? "Starting…" : "Start"}
          </button>
        </>
      )}
    </div>
  );
}

function SandboxView({ s, pollMs }: { s: string; pollMs: number }) {
  const navigate = useNavigate();
  const loaded = useLoad(() => api.sandbox(s), [s]);
  const { reload } = loaded;
  const sandbox = loaded.data;
  const org = sandbox?.org;
  useEffect(() => {
    if (org !== undefined) setCurrentOrg(org);
  }, [org]);
  useInterval(sandbox?.state === "starting", pollMs, reload);

  const [tab, setTab] = useState<Tab>("terminal");
  const [visited, setVisited] = useState<ReadonlySet<Tab>>(new Set(["terminal"]));
  const [filesTick, setFilesTick] = useState(0);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const start = useAction(async () => {
    await api.startSandbox(s);
    reload();
  });
  const stop = useAction(async () => {
    await api.stopSandbox(s);
    reload();
  });
  const remove = useAction(async () => {
    await api.deleteSandbox(s);
    navigate(`/orgs/${encodeURIComponent(org ?? "")}/sandboxes`);
  });

  if (sandbox === undefined) {
    return (
      <section>
        <ErrorBox error={loaded.error} />
        {loaded.loading && <Loading what="sandbox" />}
      </section>
    );
  }
  const running = sandbox.state === "running";
  const notRunning = <NotRunning sandbox={sandbox} onStart={() => void start.run()} starting={start.busy} />;

  return (
    <div className="sandbox-page">
      <header className="project-head">
        <nav className="crumbs" aria-label="Breadcrumb">
          <Link to={`/orgs/${encodeURIComponent(sandbox.org)}/sandboxes`}>{sandbox.org}</Link> <span aria-hidden="true">/</span> <strong>{sandbox.slug}</strong>
        </nav>
        <div className="row">
          <h1>{sandbox.slug}</h1>
          <SandboxStateBadge state={sandbox.state} />
          {sandbox.url !== null && (
            <a className="badge badge-info" href={sandbox.url} target="_blank" rel="noreferrer">
              {sandbox.url.replace(/^https?:\/\//, "")} <ExternalIcon />
            </a>
          )}
          <span className="grow" />
          {sandbox.state === "stopped" || sandbox.state === "failed" ? (
            <button type="button" onClick={() => void start.run()} disabled={start.busy}>
              <PlayIcon /> {start.busy ? "Starting…" : "Start"}
            </button>
          ) : (
            <button type="button" className="secondary" onClick={() => void stop.run()} disabled={stop.busy}>
              <StopIcon /> {stop.busy ? "Stopping…" : "Stop"}
            </button>
          )}
          <button type="button" className="danger" onClick={() => setConfirmDelete(true)}>
            Delete
          </button>
        </div>
        <p className="muted small sandbox-meta">
          Created by {sandbox.created_by.name} <Ago ms={sandbox.created_at * 1000} /> · {sandbox.cpus} vCPU · {sandbox.mem_mib / 1024} GB memory · {sandbox.disk_mib / 1024} GB disk · <AutoStop sandbox={sandbox} />
        </p>
      </header>
      {sandbox.state === "failed" && sandbox.error !== null && (
        <div className="error" role="alert">
          <strong>The sandbox failed:</strong> {sandbox.error}
        </div>
      )}
      <ErrorBox error={loaded.error} />
      <ErrorBox error={start.error} />
      <ErrorBox error={stop.error} />

      <section className="card tabs-card sandbox-tabs">
        <div className="tabs" role="tablist" aria-label="Sandbox">
          {TABS.map((t) => (
            <button
              key={t}
              type="button"
              role="tab"
              id={`tab-${t}`}
              aria-controls={`panel-${t}`}
              aria-selected={tab === t}
              className={`tab${tab === t ? " active" : ""}`}
              onClick={() => {
                setTab(t);
                setVisited((v) => new Set(v).add(t));
              }}
            >
              {TAB_LABEL[t]}
            </button>
          ))}
        </div>
        {TABS.filter((t) => t === tab || (KEPT[t] && visited.has(t))).map((t) => (
          <div key={t} className="tab-panel sandbox-panel" role="tabpanel" id={`panel-${t}`} aria-labelledby={`tab-${t}`} hidden={t !== tab}>
            {t === "terminal" &&
              (running ? (
                <Suspense fallback={<Loading what="the terminal" />}>
                  <SandboxTerminal sandbox={s} active={tab === "terminal"} />
                </Suspense>
              ) : (
                notRunning
              ))}
            {t === "chat" && (running ? <Chat target={{ kind: "sandbox", sandbox: s }} onActivity={() => setFilesTick((n) => n + 1)} /> : notRunning)}
            {t === "files" && (running ? <CodePanel target={{ kind: "sandbox", sandbox: s }} refreshKey={filesTick} /> : notRunning)}
            {t === "settings" && <SandboxSettingsPanel sandbox={sandbox} onChanged={reload} />}
          </div>
        ))}
      </section>

      {confirmDelete && (
        <Modal title={`Delete ${sandbox.slug}?`} onClose={() => setConfirmDelete(false)}>
          <p>The VM and its disk are destroyed, with everything in /workspace. This cannot be undone.</p>
          <div className="row end">
            <button type="button" className="secondary" onClick={() => setConfirmDelete(false)}>
              Cancel
            </button>
            <button type="button" className="danger" onClick={() => void remove.run()} disabled={remove.busy}>
              {remove.busy ? "Deleting…" : "Delete sandbox"}
            </button>
          </div>
          <ErrorBox error={remove.error} />
        </Modal>
      )}
    </div>
  );
}

/** `pollMs`: how often a starting sandbox is re-read. */
export function SandboxPage({ pollMs = 2000 }: { pollMs?: number }) {
  const s = useParams().s ?? "";
  // Another sandbox is another page: tabs and connections do not carry over.
  return <SandboxView key={s} s={s} pollMs={pollMs} />;
}
