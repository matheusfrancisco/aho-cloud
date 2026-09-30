// "Go live": pick (or add) the project's domain, make sure the release declares it, then approve
// and promote it — staging, then live — each approval signed with the user's passkey.
import { useState } from "react";
import type { DomainView, Project, Release } from "../gen/api";
import { api } from "../lib/api";
import { previewState, waitForPreview } from "../lib/deploy";
import { declares, type DomainChoice, ensureDeclared, ensureDomain, PROMOTE_STEPS, type PromoteStep, promoteToLive, type StepState } from "../lib/golive";
import { nextStage } from "../lib/release";
import { useSession } from "../lib/session";
import { passkeyProof } from "../lib/webauthn";
import { Badge, ErrorBox, StepList } from "../ui/common";
import { useAction } from "../ui/hooks";
import { ExternalIcon } from "../ui/icons";
import { Modal } from "../ui/overlay";
import { ApprovalCard } from "./ApprovalCard";
import { AddDomainForm, DOMAIN_STATE_TONE, DomainRow } from "./DomainsPanel";

type Phase = "domain" | "declare" | "approve" | "done";

interface Option extends DomainChoice {
  /** The project's binding, or `null` for a hostname only the release's manifest names. */
  binding: DomainView | null;
}

function options(domains: readonly DomainView[], release: Release): Option[] {
  const bound: Option[] = domains.map((d) => ({ hostname: d.hostname, mode: d.mode, binding: d }));
  const declaredOnly: Option[] = release.manifest.domains.filter((d) => !domains.some((b) => b.hostname === d.hostname)).map((d) => ({ hostname: d.hostname, mode: d.verification, binding: null }));
  return [...bound, ...declaredOnly];
}

const NO_STEPS: Record<PromoteStep, StepState> = { approve_staging: "pending", promote_staging: "pending", approve_live: "pending", promote_live: "pending" };

type DeclareStep = "bind" | "propose" | "preview";

export function GoLiveModal({ project, release: initial, onClose, onChanged, pollMs = 2000 }: { project: Project; release: Release; onClose: () => void; onChanged: () => void; pollMs?: number }) {
  const session = useSession();
  const [phase, setPhase] = useState<Phase>("domain");
  const [domains, setDomains] = useState<DomainView[]>(project.domains);
  const [release, setRelease] = useState(initial);
  const opts = options(domains, initial);
  const [chosen, setChosen] = useState<string | null>(() => {
    const all = options(project.domains, initial);
    return (all.find((o) => o.binding !== null && declares(initial.manifest, o.hostname)) ?? all[0])?.hostname ?? null;
  });
  const choice = opts.find((o) => o.hostname === chosen) ?? null;
  const [declareSteps, setDeclareSteps] = useState<Record<DeclareStep, StepState>>({ bind: "pending", propose: "pending", preview: "pending" });
  const [steps, setSteps] = useState<Record<PromoteStep, StepState>>(NO_STEPS);

  const upsert = (d: DomainView) => setDomains((ds) => (ds.some((x) => x.hostname === d.hostname) ? ds.map((x) => (x.hostname === d.hostname ? d : x)) : [...ds, d]));
  const mark = (step: DeclareStep, state: StepState) => setDeclareSteps((s) => ({ ...s, [step]: state }));

  const prepare = useAction(async () => {
    if (choice === null) return;
    setPhase("declare");
    setDeclareSteps({ bind: "pending", propose: "pending", preview: "pending" });
    // A copy proposed by an earlier attempt is followed again, unless it failed: then the preview release is copied anew.
    const base = release.id !== initial.id && previewState(release) === "failed" ? initial : release;
    let step: DeclareStep = "bind";
    try {
      mark("bind", "running");
      const { domain, added } = await ensureDomain(api, project.slug, domains, choice);
      upsert(domain);
      if (added) onChanged();
      mark("bind", added ? "done" : "skipped");
      const target: DomainChoice = { hostname: domain.hostname, mode: domain.mode };
      step = "propose";
      let ready: Release;
      if (base.id !== initial.id && declares(base.manifest, target.hostname)) {
        mark("propose", "done");
        step = "preview";
        mark("preview", "running");
        ready = await waitForPreview(api, project.slug, base, { intervalMs: pollMs, onUpdate: setRelease });
        mark("preview", "done");
      } else {
        const already = declares(base.manifest, target.hostname);
        mark("propose", already ? "skipped" : "running");
        if (already) mark("preview", "skipped");
        const declared = await ensureDeclared(api, base, target, {
          intervalMs: pollMs,
          onProposed: (r) => {
            setRelease(r);
            mark("propose", "done");
            step = "preview";
            mark("preview", "running");
            onChanged();
          },
          onUpdate: setRelease,
        });
        if (declared.proposed) mark("preview", "done");
        ready = declared.release;
      }
      setRelease(ready);
      setPhase("approve");
    } catch (e) {
      mark(step, "failed");
      throw e;
    }
  });

  const golive = useAction(async () => {
    if (session === null) throw new Error("Sign in to approve releases.");
    setSteps((s) => Object.fromEntries(Object.entries(s).map(([k, v]) => [k, v === "done" || v === "skipped" ? v : "pending"])) as Record<PromoteStep, StepState>);
    const live = await promoteToLive(api, release, {
      userId: session.user.id,
      sign: (message) => passkeyProof(api, session.user.email, message),
      onStep: (step, state) => setSteps((s) => ({ ...s, [step]: state })),
      onRelease: setRelease,
    });
    setRelease(live);
    onChanged();
    setPhase("done");
  });

  const bound = domains.find((d) => d.hostname === chosen) ?? null;

  return (
    <Modal title="Go live" onClose={onClose} wide>
      <ol className="phases" aria-label="Go-live steps">
        {(["domain", "declare", "approve", "done"] as const).map((p, i) => (
          <li key={p} className={p === phase ? "current" : ""}>
            {i + 1}. {{ domain: "Domain", declare: "Release", approve: "Approve", done: "Live" }[p]}
          </li>
        ))}
      </ol>

      {phase === "domain" && (
        <div className="stack">
          <p className="hint">
            Pick the domain <strong>{project.slug}</strong> goes live on. Release <code>{initial.id}</code>, running at preview now, is what ships.
          </p>
          {opts.length === 0 ? (
            <p className="muted">This project has no domain yet. Add one:</p>
          ) : (
            <fieldset className="domain-choices">
              <legend>Domain</legend>
              {opts.map((o) => (
                <label key={o.hostname} className="choice">
                  <input type="radio" name="golive-domain" value={o.hostname} checked={chosen === o.hostname} onChange={() => setChosen(o.hostname)} />
                  <span className="grow">{o.hostname}</span>
                  {o.binding !== null ? <Badge tone={DOMAIN_STATE_TONE[o.binding.state]}>{o.binding.state}</Badge> : <Badge tone="muted">not added yet</Badge>}
                  {declares(initial.manifest, o.hostname) && <Badge tone="info">declared</Badge>}
                </label>
              ))}
            </fieldset>
          )}
          <AddDomainForm
            project={project.slug}
            onAdded={(d) => {
              upsert(d);
              setChosen(d.hostname);
              onChanged();
            }}
          />
          {bound !== null && (
            <>
              <h3>DNS records for {bound.hostname}</h3>
              <ul className="domains">
                <DomainRow project={project.slug} domain={bound} onChanged={upsert} />
              </ul>
              {bound.state !== "verified" && <p className="hint">You can go live before DNS is verified; the domain starts serving once it is.</p>}
            </>
          )}
          <div className="row end">
            <button type="button" className="secondary" onClick={onClose}>
              Cancel
            </button>
            <button type="button" disabled={choice === null || prepare.busy} onClick={() => void prepare.run()}>
              Continue
            </button>
          </div>
        </div>
      )}

      {phase === "declare" && (
        <div className="stack">
          <StepList
            steps={[
              { id: "bind", label: `Add ${chosen ?? "the domain"} to the project`, state: declareSteps.bind },
              { id: "propose", label: `Propose a release that declares ${chosen ?? "the domain"} (same code and provenance)`, state: declareSteps.propose },
              { id: "preview", label: release.id === initial.id ? "Build it and activate it at preview" : `Build ${release.id} and activate it at preview`, state: declareSteps.preview },
            ]}
          />
          <ErrorBox error={prepare.error} />
          {prepare.error !== null && (
            <div className="row end">
              <button
                type="button"
                className="secondary"
                onClick={() => {
                  // Another domain may be chosen: start again from the release running at preview.
                  setRelease(initial);
                  prepare.clear();
                  setPhase("domain");
                }}
              >
                Back
              </button>
              <button type="button" onClick={() => void prepare.run()}>
                Retry
              </button>
            </div>
          )}
        </div>
      )}

      {phase === "approve" && (
        <div className="stack">
          <ApprovalCard release={release} stage={nextStage(release.stage)} />
          <StepList steps={PROMOTE_STEPS.map((s) => ({ id: s.id, label: s.label, state: steps[s.id] }))} />
          <p className="hint">
            Each approval signs project <code>{release.project}</code>, release <code>{release.id}</code>, manifest <code>{release.manifest_hash}</code> and the stage.
          </p>
          <ErrorBox error={golive.error} />
          <div className="row end">
            <button type="button" className="secondary" onClick={onClose}>
              Cancel
            </button>
            <button type="button" disabled={golive.busy} onClick={() => void golive.run()}>
              {golive.busy ? "Going live…" : golive.error !== null ? "Retry" : "Approve & go live"}
            </button>
          </div>
        </div>
      )}

      {phase === "done" && (
        <div className="stack">
          <p className="ok-banner">
            <code>{release.id}</code> is live.
          </p>
          {bound !== null && bound.state === "verified" ? (
            <p>
              Serving at{" "}
              <a href={`https://${bound.hostname}`} target="_blank" rel="noreferrer">
                https://{bound.hostname} <ExternalIcon />
              </a>
            </p>
          ) : (
            bound !== null && (
              <>
                <p className="note">
                  Waiting for DNS: <strong>{bound.hostname}</strong> is not verified yet. Create these records at your DNS provider, then verify; the domain serves the live release
                  once it is.
                </p>
                <ul className="domains">
                  <DomainRow project={project.slug} domain={bound} onChanged={upsert} />
                </ul>
              </>
            )
          )}
          <div className="row end">
            <button type="button" onClick={onClose}>
              Done
            </button>
          </div>
        </div>
      )}
    </Modal>
  );
}
