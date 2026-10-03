// The org's sandboxes: VMs with aho for coding without deploying, each with its state, who
// created it, and when it stops on its own. Followed while any is starting.
import { useEffect } from "react";
import { Link, useParams } from "react-router";
import { useShell } from "../components/Shell";
import type { Sandbox, SandboxState } from "../gen/api";
import { api } from "../lib/api";
import { setCurrentOrg } from "../lib/remembered";
import { Ago, Badge, ErrorBox, Loading } from "../ui/common";
import { useInterval, useLoad } from "../ui/hooks";
import { EmptyIllustration, ExternalIcon, PlusIcon } from "../ui/icons";

const STATE_TONE: Record<SandboxState, "ok" | "info" | "muted" | "bad"> = { running: "ok", starting: "info", stopped: "muted", failed: "bad" };

export function SandboxStateBadge({ state }: { state: SandboxState }) {
  return <Badge tone={STATE_TONE[state]}>{state}</Badge>;
}

/** "stops in 3 hours" while it runs, else how long it will run once started. */
export function AutoStop({ sandbox }: { sandbox: Sandbox }) {
  return sandbox.stops_at !== null && (sandbox.state === "running" || sandbox.state === "starting") ? (
    <span>
      stops <Ago ms={sandbox.stops_at * 1000} />
    </span>
  ) : (
    <span>auto-stops {sandbox.settings.auto_stop_hours} h after start</span>
  );
}

export const SANDBOX_PITCH = "A sandbox is a VM with aho, for coding without deploying: a terminal and a chat with the aho inside, and a /workspace disk that survives stops.";

export function SandboxesPage({ pollMs = 3000 }: { pollMs?: number }) {
  const org = useParams().org ?? "";
  const { createSandbox } = useShell();
  useEffect(() => setCurrentOrg(org), [org]);
  const list = useLoad(() => api.sandboxes(org), [org]);
  useInterval(list.data?.some((s) => s.state === "starting") ?? false, pollMs, list.reload);

  return (
    <section className="card flush" aria-label="Sandboxes">
      <header className="card-head">
        <h1>Sandboxes</h1>
        <span className="grow" />
        {list.data !== undefined && list.data.length > 0 && (
          <button type="button" className="soft" onClick={createSandbox}>
            <PlusIcon /> New sandbox
          </button>
        )}
      </header>
      <ErrorBox error={list.error} />
      {list.data === undefined ? (
        list.loading && <Loading what="sandboxes" />
      ) : list.data.length === 0 ? (
        <div className="empty">
          <EmptyIllustration />
          <p>You don't have any sandboxes yet</p>
          <p className="hint">{SANDBOX_PITCH}</p>
          <button type="button" className="soft" onClick={createSandbox}>
            <PlusIcon /> Create a sandbox
          </button>
        </div>
      ) : (
        <ul className="project-list">
          {list.data.map((s) => (
            <li key={s.slug} className="project-row">
              <div className="project-main">
                <Link to={`/sandboxes/${encodeURIComponent(s.slug)}`} className="project-name">
                  {s.slug}
                </Link>
                <SandboxStateBadge state={s.state} />
                <span className="muted small">by {s.created_by.name}</span>
              </div>
              <div className="project-meta small">
                {s.url !== null && (
                  <a href={s.url} target="_blank" rel="noreferrer">
                    Open <ExternalIcon />
                  </a>
                )}
                <span className="muted">
                  <AutoStop sandbox={s} />
                </span>
              </div>
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}
