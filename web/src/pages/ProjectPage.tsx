// A project: chat with aho on the left; its code, releases, domains, secrets and ledger on the
// right. "Deploy to preview" ships the workspace files; "Go live" puts the preview on a domain.
import { useEffect, useRef, useState } from "react";
import { Link, useParams } from "react-router";
import { Chat } from "../components/Chat";
import { CodePanel } from "../components/CodePanel";
import { DomainsPanel } from "../components/DomainsPanel";
import { DraftConfirm } from "../components/DraftConfirm";
import { GoLiveModal } from "../components/GoLive";
import { LedgerTimeline } from "../components/LedgerTimeline";
import { ReleasesPanel, type Tracked } from "../components/ReleasesPanel";
import { SecretsPanel } from "../components/Secrets";
import type { Release, Workspace } from "../gen/api";
import type { Draft, Manifest } from "../gen/contract";
import { api } from "../lib/api";
import { DeployFailed, waitForPreview } from "../lib/deploy";
import { draftToValue } from "../lib/draft";
import { previewUrl as rememberedPreview, rememberPreview, setCurrentOrg } from "../lib/remembered";
import { Badge, ErrorBox, Loading } from "../ui/common";
import { useAction, useLoad } from "../ui/hooks";
import { ExternalIcon, GlobeIcon, RocketIcon } from "../ui/icons";
import { Modal } from "../ui/overlay";

const TABS = ["code", "releases", "domains", "secrets", "activity"] as const;
type Tab = (typeof TABS)[number];
const TAB_LABEL: Record<Tab, string> = { code: "Code", releases: "Releases", domains: "Domains", secrets: "Secrets", activity: "Activity" };

function ProjectView({ p, pollMs }: { p: string; pollMs: number }) {
  const loaded = useLoad(async () => {
    const [project, releases, ledger, secrets] = await Promise.all([api.project(p), api.releases(p), api.ledger(p), api.secrets(p)]);
    return { project, releases, ledger, secrets };
  }, [p]);
  const org = loaded.data?.project.org;
  useEffect(() => {
    if (org !== undefined) setCurrentOrg(org);
  }, [org]);

  const [tab, setTab] = useState<Tab>("code");
  const [workspace, setWorkspace] = useState<Workspace | null>(null);
  const [codeTick, setCodeTick] = useState(0);
  const [tracked, setTracked] = useState<Tracked | null>(null);
  const [draft, setDraft] = useState<{ draft: Draft; n: number } | null>(null);
  const [goLive, setGoLive] = useState(false);
  const tracker = useRef<AbortController | null>(null);
  useEffect(() => () => tracker.current?.abort(), []);
  const { reload } = loaded;

  /** Follows `release` to preview in the Releases tab. */
  const track = (release: Release) => {
    tracker.current?.abort();
    const ctrl = new AbortController();
    tracker.current = ctrl;
    setTab("releases");
    setTracked({ release, failure: null });
    reload();
    waitForPreview(api, p, release, { intervalMs: pollMs, signal: ctrl.signal, onUpdate: (r) => setTracked({ release: r, failure: null }) }).then(
      (r) => {
        setTracked({ release: r, failure: null });
        reload();
      },
      (e: unknown) => {
        if (ctrl.signal.aborted) return;
        setTracked((t) => ({ release: e instanceof DeployFailed ? e.release : (t?.release ?? release), failure: e instanceof Error ? e.message : String(e) }));
        reload();
      },
    );
  };

  const deploy = useAction(async (confirmed?: Draft) => {
    const result = await api.workspaceDeploy(p, confirmed === undefined ? undefined : (draftToValue(confirmed) as unknown as Manifest));
    if (result.status === "needs_manifest") {
      setDraft((d) => ({ draft: result.draft, n: (d?.n ?? 0) + 1 }));
      return;
    }
    setDraft(null);
    track(result.release);
  });

  if (loaded.data === undefined) {
    return (
      <section>
        <ErrorBox error={loaded.error} />
        {loaded.loading && <Loading what="project" />}
      </section>
    );
  }
  const { project, releases, ledger, secrets } = loaded.data;
  const previewHead = project.active.find((a) => a.stage === "preview");
  const liveHead = project.active.find((a) => a.stage === "live");
  const previewRelease = previewHead === undefined ? undefined : releases.find((r) => r.id === previewHead.release);
  const liveRelease = liveHead === undefined ? undefined : releases.find((r) => r.id === liveHead.release);
  const liveDomain = liveRelease === undefined ? undefined : project.domains.find((d) => liveRelease.manifest.domains.some((m) => m.hostname === d.hostname));
  const preview = workspace?.preview_url ?? rememberedPreview(p);

  return (
    <div className="project-page">
      <header className="project-head">
        <nav className="crumbs" aria-label="Breadcrumb">
          <Link to={`/orgs/${encodeURIComponent(project.org)}`}>{project.org}</Link> <span aria-hidden="true">/</span> <strong>{project.slug}</strong>
        </nav>
        <div className="row">
          <h1>{project.slug}</h1>
          {previewHead !== undefined &&
            (preview === null ? (
              <Badge tone="info">Preview</Badge>
            ) : (
              <a className="badge badge-info" href={preview} target="_blank" rel="noreferrer">
                Preview <ExternalIcon />
              </a>
            ))}
          {liveHead !== undefined && (
            <Badge tone="ok">
              Live
              {liveDomain !== undefined && (
                <>
                  {" · "}
                  {liveDomain.state === "verified" ? (
                    <a href={`https://${liveDomain.hostname}`} target="_blank" rel="noreferrer">
                      {liveDomain.hostname}
                    </a>
                  ) : (
                    `${liveDomain.hostname} (waiting for DNS)`
                  )}
                </>
              )}
            </Badge>
          )}
          <span className="grow" />
          <button type="button" onClick={() => void deploy.run()} disabled={deploy.busy}>
            <RocketIcon /> {deploy.busy && draft === null ? "Packaging workspace…" : "Deploy to preview"}
          </button>
          <button
            type="button"
            className="secondary"
            onClick={() => setGoLive(true)}
            disabled={previewRelease === undefined}
            title={previewRelease === undefined ? "Deploy to preview first: going live promotes the release running at preview." : undefined}
          >
            <GlobeIcon /> Go live
          </button>
        </div>
      </header>
      <ErrorBox error={loaded.error} />
      {draft === null && <ErrorBox error={deploy.error} />}

      <div className="project-split">
        <Chat
          project={p}
          onWorkspace={(w) => {
            setWorkspace(w);
            rememberPreview(p, w.preview_url);
          }}
          onActivity={() => setCodeTick((t) => t + 1)}
          onProposed={track}
        />
        <section className="card tabs-card">
          <div className="tabs" role="tablist" aria-label="Project">
            {TABS.map((t) => (
              <button key={t} type="button" role="tab" id={`tab-${t}`} aria-controls={`panel-${t}`} aria-selected={tab === t} className={`tab${tab === t ? " active" : ""}`} onClick={() => setTab(t)}>
                {TAB_LABEL[t]}
              </button>
            ))}
          </div>
          <div className="tab-panel" role="tabpanel" id={`panel-${tab}`} aria-labelledby={`tab-${tab}`}>
            {/* The files live in the workspace the chat starts; listing them before it exists would 404. */}
            {tab === "code" && (workspace === null ? <Loading what="the workspace" /> : <CodePanel project={p} refreshKey={codeTick} />)}
            {tab === "releases" && <ReleasesPanel project={project} releases={releases} tracked={tracked} previewUrl={preview} />}
            {tab === "domains" && <DomainsPanel project={p} domains={project.domains} onChanged={reload} />}
            {tab === "secrets" && (
              <>
                <p className="hint">Values are write-only: stored encrypted, never shown again. The fingerprint is what the taint scan looks for.</p>
                <SecretsPanel project={p} secrets={secrets} onChanged={reload} />
              </>
            )}
            {tab === "activity" && <LedgerTimeline project={p} entries={ledger} />}
          </div>
        </section>
      </div>

      {draft !== null && (
        <Modal title="Deploy to preview" onClose={() => setDraft(null)} wide>
          <p className="hint">
            The workspace has no <code>deploy.json</code>. This draft comes from its files and the aho session; the source is the workspace archive. Confirm it to deploy.
          </p>
          <DraftConfirm key={draft.n} draft={draft.draft} submitLabel="Deploy to preview" submitting={deploy.busy} onSubmit={(d) => void deploy.run(d)} />
          <ErrorBox error={deploy.error} />
        </Modal>
      )}
      {goLive && previewRelease !== undefined && <GoLiveModal project={project} release={previewRelease} pollMs={pollMs} onClose={() => setGoLive(false)} onChanged={reload} />}
    </div>
  );
}

/** `pollMs`: how often a release on its way to preview is re-read. */
export function ProjectPage({ pollMs = 2000 }: { pollMs?: number }) {
  const p = useParams().p ?? "";
  // A new project is a new page: chat, tabs and deploy state do not carry over.
  return <ProjectView key={p} p={p} pollMs={pollMs} />;
}
