// The Releases tab: the deploy in progress (if any), what runs at each stage, and every release.
import { Link } from "react-router";
import { Badge, ExternalIcon, StepList, type StepState } from "@aho/ui";
import type { Project, Release } from "../gen/api";
import { previewState } from "../lib/deploy";
import { groupChanges, STAGES } from "../lib/release";
import { StatusBadge } from "./ApprovalCard";

/** A release being followed to preview; `failure` once it cannot get there. */
export interface Tracked {
  release: Release;
  failure: string | null;
}

function releaseLink(project: string, release: string): string {
  return `/projects/${encodeURIComponent(project)}/releases/${encodeURIComponent(release)}`;
}

export function DeployProgress({ tracked, previewUrl }: { tracked: Tracked; previewUrl: string | null }) {
  const r = tracked.release;
  const state = previewState(r);
  const buildFailed = r.status === "build_failed";
  const failed = tracked.failure !== null;
  const build: StepState = buildFailed ? "failed" : state === "building" ? "running" : "done";
  const activate: StepState = buildFailed ? "pending" : failed ? "failed" : state === "active" ? "done" : state === "activating" ? "running" : "pending";
  return (
    <section className={`deploy-progress${failed ? " failed" : state === "active" ? " done" : ""}`} aria-label="Deploy progress" aria-live="polite">
      <div className="row">
        <strong className="grow">
          Deploying{" "}
          <Link to={releaseLink(r.project, r.id)}>
            <code>{r.id}</code>
          </Link>{" "}
          to preview
        </strong>
        <StatusBadge status={r.status} />
      </div>
      <StepList
        steps={[
          { id: "proposed", label: "Workspace packaged and release proposed", state: "done" },
          { id: "build", label: buildFailed ? "Build failed" : state === "building" ? "Building…" : "Built", state: build },
          { id: "preview", label: state === "active" ? "Active at preview" : "Activate at preview", state: activate, detail: failed && !buildFailed ? tracked.failure : undefined },
        ]}
      />
      {buildFailed && <p className="error">{tracked.failure ?? "The build failed."} Open the release for details, fix the code with aho, and deploy again.</p>}
      {state === "active" && previewUrl !== null && (
        <p className="preview-ready">
          Preview is up:{" "}
          <a href={previewUrl} target="_blank" rel="noreferrer">
            {previewUrl} <ExternalIcon />
          </a>
        </p>
      )}
    </section>
  );
}

export function ReleasesPanel({ project, releases, tracked, previewUrl }: { project: Project; releases: Release[]; tracked: Tracked | null; previewUrl: string | null }) {
  const p = project.slug;
  return (
    <div className="releases-panel">
      {tracked !== null && <DeployProgress tracked={tracked} previewUrl={previewUrl} />}
      <h3>Running now</h3>
      <table className="stages">
        <tbody>
          {STAGES.map((s) => {
            const head = project.active.find((a) => a.stage === s);
            return (
              <tr key={s}>
                <th>{s}</th>
                <td>
                  {head === undefined ? (
                    <span className="muted">nothing active</span>
                  ) : (
                    <Link to={releaseLink(p, head.release)}>
                      <code>{head.release}</code>
                    </Link>
                  )}
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
      <h3>All releases</h3>
      {releases.length === 0 ? (
        <p className="muted">No releases yet. Deploy to preview, or propose one from the terminal.</p>
      ) : (
        <table>
          <thead>
            <tr>
              <th>Release</th>
              <th>Status</th>
              <th>Stage</th>
              <th>Surface vs live</th>
              <th>Provenance</th>
            </tr>
          </thead>
          <tbody>
            {releases.map((r) => {
              const g = groupChanges(r.diff);
              return (
                <tr key={r.id}>
                  <td>
                    <Link to={releaseLink(p, r.id)}>
                      <code>{r.id}</code>
                    </Link>
                  </td>
                  <td>
                    <StatusBadge status={r.status} />
                  </td>
                  <td>{r.stage ?? <span className="muted">—</span>}</td>
                  <td>
                    {g.widen.length > 0 && <Badge tone="warn">{g.widen.length} widen</Badge>} {g.narrow.length > 0 && <Badge tone="ok">{g.narrow.length} narrow</Badge>}{" "}
                    {g.neutral.length > 0 && <Badge tone="muted">{g.neutral.length} neutral</Badge>}
                  </td>
                  <td>{r.provenance.kind}</td>
                </tr>
              );
            })}
          </tbody>
        </table>
      )}
    </div>
  );
}
