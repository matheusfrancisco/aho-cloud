// A release's approval card plus the actions on it: approve (passkey over the approval message),
// reject, promote, roll back to it, and the on-call standing-orders grant.
import { useEffect, useState } from "react";
import { Link, useParams } from "react-router";
import { ErrorBox, Loading, useAction, useLoad } from "@aho/ui";
import { ApprovalCard } from "../components/ApprovalCard";
import { SecretForm } from "../components/Secrets";
import { StandingOrdersForm } from "../components/StandingOrdersForm";
import type { Release } from "../gen/api";
import type { Stage } from "../gen/contract";
import { api } from "../lib/api";
import { approvalMessage } from "../lib/approval";
import { approvalProgress, groupChanges, nextStage, STAGES } from "../lib/release";
import { setCurrentOrg } from "../lib/remembered";
import { currentSession, useSession } from "../lib/session";
import { passkeyProof } from "../lib/webauthn";

function signerEmail(): string {
  const s = currentSession();
  if (s === null) throw new Error("Sign in to sign approvals.");
  return s.user.email;
}

function Actions({ release, stage, reachable, onStage, onDone }: { release: Release; stage: Stage; reachable: Stage[]; onStage: (s: Stage) => void; onDone: () => void }) {
  const session = useSession();
  const [reason, setReason] = useState("");
  const progress = approvalProgress(release, stage);
  const mine = session !== null && progress.approvals.some((a) => a.user.id === session.user.id);
  const blocked = release.status === "tainted" || release.status === "rejected" || release.status === "building" || release.status === "build_failed";

  const approve = useAction(async () => {
    const message = approvalMessage(release.project, release.id, release.manifest_hash, stage);
    const proof = await passkeyProof(api, signerEmail(), message);
    await api.approve(release.project, release.id, { stage, proof });
    onDone();
  });
  const reject = useAction(async () => {
    await api.reject(release.project, release.id, { stage, reason: reason.trim() });
    setReason("");
    onDone();
  });
  const promote = useAction(async () => {
    await api.promote(release.project, release.id, { stage });
    onDone();
  });

  return (
    <section className="card actions" aria-label="release actions">
      <div className="row">
        <label>
          Target stage
          <select value={stage} onChange={(e) => onStage(e.target.value as Stage)}>
            {reachable.map((s) => (
              <option key={s} value={s}>
                {s}
              </option>
            ))}
          </select>
        </label>
        {progress.needed > 0 && (
          <button type="button" onClick={() => void approve.run()} disabled={approve.busy || mine || blocked}>
            {approve.busy ? "Waiting for passkey…" : mine ? `You approved ${stage}` : `Approve for ${stage} with passkey`}
          </button>
        )}
        <button type="button" onClick={() => void promote.run()} disabled={promote.busy || !progress.met || blocked}>
          Promote to {stage}
        </button>
      </div>
      {progress.needed > 0 && (
        <p className="hint">
          Signing binds project <code>{release.project}</code>, release <code>{release.id}</code>, manifest <code>{release.manifest_hash}</code> and stage <code>{stage}</code>.
        </p>
      )}
      <ErrorBox error={approve.error} />
      <ErrorBox error={promote.error} />
      <form
        className="row"
        onSubmit={(e) => {
          e.preventDefault();
          void reject.run();
        }}
      >
        <label className="grow">
          Reason
          <input value={reason} onChange={(e) => setReason(e.target.value)} placeholder="Why this release must not ship" required />
        </label>
        <button type="submit" className="danger" disabled={reject.busy || reason.trim() === ""}>
          Reject for {stage}
        </button>
      </form>
      <ErrorBox error={reject.error} />
    </section>
  );
}

function RollbackPanel({ release, live, onDone }: { release: Release; live: string | null; onDone: () => void }) {
  // `diff` is against live: a rollback target that widens nothing fits within live and needs no signature.
  const widens = groupChanges(release.diff).widen.length > 0;
  const rollback = useAction(async () => {
    const proof = widens ? await passkeyProof(api, signerEmail(), approvalMessage(release.project, release.id, release.manifest_hash, "live")) : null;
    await api.rollback(release.project, { release: release.id, proof });
    onDone();
  });
  // Only a release that was live before and is not live now is a rollback target.
  if (live === null || live === release.id || release.stage !== "live") return null;
  return (
    <section className="card">
      <h2>Roll back live to this release</h2>
      <p className="hint">
        {widens ? "This release reaches further than live, so rolling back to it is a widening and needs your passkey." : "This release fits within live's surface; rolling back needs no signature."}
      </p>
      <button type="button" className="danger" onClick={() => void rollback.run()} disabled={rollback.busy}>
        {rollback.busy ? "Rolling back…" : `Roll back live to ${release.id}`}
      </button>
      <ErrorBox error={rollback.error} />
    </section>
  );
}

export function ReleasePage() {
  const params = useParams();
  const p = params.p ?? "";
  const r = params.r ?? "";
  const loaded = useLoad(async () => {
    const [release, project] = await Promise.all([api.release(p, r), api.project(p)]);
    return { release, project };
  }, [p, r]);
  const [chosen, setChosen] = useState<Stage | null>(null);
  const org = loaded.data?.project.org;
  useEffect(() => {
    if (org !== undefined) setCurrentOrg(org);
  }, [org]);

  if (loaded.data === undefined) {
    return (
      <section>
        <ErrorBox error={loaded.error} />
        {loaded.loading && <Loading what="release" />}
      </section>
    );
  }
  const { release, project } = loaded.data;
  const next = nextStage(release.stage);
  // Stages this release can still be approved for: the next one and any later.
  const reachable = next === null ? [] : STAGES.slice(STAGES.indexOf(next));
  // A choice made on another release (same page instance, new params) does not carry over.
  const stage = chosen !== null && reachable.includes(chosen) ? chosen : next;
  const live = project.active.find((a) => a.stage === "live")?.release ?? null;

  return (
    <section>
      <nav className="crumbs" aria-label="Breadcrumb">
        <Link to={`/orgs/${encodeURIComponent(project.org)}`}>{project.org}</Link> / <Link to={`/projects/${encodeURIComponent(p)}`}>{p}</Link> / releases / {r}
      </nav>
      <ErrorBox error={loaded.error} />
      <ApprovalCard release={release} stage={stage} />
      {release.taint.length > 0 && (
        <section className="card">
          <h2>Rotate tainted secrets</h2>
          {[...new Set(release.taint.map((t) => t.secret))].map((name) => (
            <SecretForm key={name} project={p} name={name} onSaved={loaded.reload} />
          ))}
        </section>
      )}
      {stage === null ? (
        <p className="muted">This release has reached live; there is no further stage.</p>
      ) : (
        <Actions release={release} stage={stage} reachable={reachable} onStage={setChosen} onDone={loaded.reload} />
      )}
      <RollbackPanel release={release} live={live} onDone={loaded.reload} />
      <section className="card">
        <h2>Standing orders for on-call</h2>
        <StandingOrdersForm project={p} />
      </section>
    </section>
  );
}
