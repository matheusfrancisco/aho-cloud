// The approval card: what this release changes against live, what it takes to ship it, and what
// blocks it. Widenings are what a signer vouches for, so they come first and stand out.
import { Badge, Hash } from "@aho/ui";
import type { Release, ReleaseStatus } from "../gen/api";
import type { Change, Direction, Stage } from "../gen/contract";
import { approvalProgress, groupChanges } from "../lib/release";

const STATUS_TONE: Record<ReleaseStatus, "ok" | "warn" | "bad" | "info" | "muted"> = {
  building: "info",
  build_failed: "bad",
  awaiting_approval: "warn",
  tainted: "bad",
  rejected: "bad",
  active: "ok",
  superseded: "muted",
};

export function StatusBadge({ status }: { status: ReleaseStatus }) {
  return <Badge tone={STATUS_TONE[status]}>{status.replaceAll("_", " ")}</Badge>;
}

const GROUPS: ReadonlyArray<{ direction: Direction; title: string; hint: string }> = [
  { direction: "widen", title: "Widens", hint: "New reach for the app. Approvers sign for these." },
  { direction: "narrow", title: "Narrows", hint: "Less reach than live. Free to apply." },
  { direction: "neutral", title: "Neutral", hint: "Changes without new reach." },
];

function ChangeList({ direction, changes }: { direction: Direction; changes: Change[] }) {
  return (
    <ul className={`changes changes-${direction}`}>
      {changes.map((c, i) => (
        <li key={`${c.category}:${i}`} className={`change change-${direction}`}>
          <span className="category">{c.category.replaceAll("_", " ")}</span> {c.detail}
        </li>
      ))}
    </ul>
  );
}

export function ApprovalCard({ release, stage }: { release: Release; stage: Stage | null }) {
  const groups = groupChanges(release.diff);
  const progress = stage === null ? null : approvalProgress(release, stage);
  const prov = release.provenance;

  return (
    <section className="card approval-card" aria-label="approval card">
      <header className="row">
        <h2>
          Release <code>{release.id}</code>
        </h2>
        <StatusBadge status={release.status} />
        {release.stage !== null && <Badge tone="info">at {release.stage}</Badge>}
      </header>
      <dl className="facts">
        <dt>Manifest</dt>
        <dd>
          <Hash value={release.manifest_hash} />
        </dd>
        <dt>Image</dt>
        <dd>{release.image_digest === null ? <span className="muted">not built yet</span> : <Hash value={release.image_digest} />}</dd>
        <dt>Provenance</dt>
        <dd>
          {prov.kind === "attested" ? (
            <>
              aho session <code>{prov.session_id}</code> verified to <Hash value={prov.head_hash} />
            </>
          ) : (
            <>
              unattested{prov.commit !== null && <> · commit <code>{prov.commit}</code></>}
              {prov.ci_run !== null && <> · CI run <code>{prov.ci_run}</code></>}
            </>
          )}
        </dd>
      </dl>

      <h3>Surface changes against live</h3>
      {release.diff.changes.length === 0 && <p className="muted">No surface changes against live.</p>}
      {GROUPS.map(({ direction, title, hint }) =>
        groups[direction].length === 0 ? null : (
          <div key={direction} className={`change-group group-${direction}`} data-direction={direction}>
            <h4>
              {title} <span className="count">({groups[direction].length})</span>
            </h4>
            <p className="hint">{hint}</p>
            <ChangeList direction={direction} changes={groups[direction]} />
          </div>
        ),
      )}

      {progress !== null && stage !== null && (
        <>
          <h3>To reach {stage}</h3>
          {progress.needed === 0 ? (
            <p>No approval needed; promote once built and untainted.</p>
          ) : (
            <p className="requirement">
              <strong>
                {progress.count} of {progress.needed}
              </strong>{" "}
              approval{progress.needed === 1 ? "" : "s"}
              {progress.ownerRequired && (
                <span className={`owner-required ${progress.ownerPresent ? "met" : "unmet"}`}>
                  {" "}
                  · Owner approval required{progress.ownerPresent ? " (present)" : " (missing)"}
                </span>
              )}
              {progress.met && <Badge tone="ok">met</Badge>}
            </p>
          )}
        </>
      )}

      {release.approvals.length > 0 && (
        <>
          <h3>Approvals</h3>
          <ul className="approvals">
            {release.approvals.map((a) => (
              <li key={`${a.stage}:${a.user.id}`}>
                <Badge tone="info">{a.stage}</Badge> {a.user.name} <span className="muted">{a.user.email}</span>
                {a.owner && <Badge tone="ok">owner</Badge>}
              </li>
            ))}
          </ul>
        </>
      )}

      {release.taint.length > 0 && (
        <div className="blocker">
          <h3>Tainted secrets</h3>
          <p>
            These secret values appeared in an uploaded session log, so treat them as leaked. The release stays blocked until each is rotated: revoke the credential at its
            provider, issue a new one, and set the new value below.
          </p>
          <ul>
            {release.taint.map((t) => (
              <li key={`${t.secret}@${t.location}`}>
                <code>{t.secret}</code> found at <code>{t.location}</code>
              </li>
            ))}
          </ul>
        </div>
      )}

      {release.undeclared_egress.length > 0 && (
        <div className="blocker">
          <h3>Undeclared egress</h3>
          <p>The staging run reached hosts the manifest does not declare. Declare them in `egress` (a widening) or stop the app from calling them.</p>
          <ul>
            {release.undeclared_egress.map((h) => (
              <li key={h}>
                <code>{h}</code>
              </li>
            ))}
          </ul>
        </div>
      )}
    </section>
  );
}
