import { Link, useParams } from "react-router";
import { StatusBadge } from "../components/ApprovalCard";
import { DomainsPanel } from "../components/DomainsPanel";
import { LedgerTimeline } from "../components/LedgerTimeline";
import { SecretsPanel } from "../components/Secrets";
import { api } from "../lib/api";
import { groupChanges, STAGES } from "../lib/release";
import { Badge, ErrorBox, Loading } from "../ui/common";
import { useLoad } from "../ui/hooks";

export function ProjectPage() {
  const p = useParams().p ?? "";
  const loaded = useLoad(async () => {
    const [project, releases, ledger, secrets] = await Promise.all([api.project(p), api.releases(p), api.ledger(p), api.secrets(p)]);
    return { project, releases, ledger, secrets };
  }, [p]);
  const rel = (r: string) => `/projects/${encodeURIComponent(p)}/releases/${encodeURIComponent(r)}`;

  if (loaded.data === undefined) {
    return (
      <section>
        <ErrorBox error={loaded.error} />
        {loaded.loading && <Loading what="project" />}
      </section>
    );
  }
  const { project, releases, ledger, secrets } = loaded.data;

  return (
    <section>
      <p className="crumbs">
        <Link to={`/orgs/${encodeURIComponent(project.org)}`}>{project.org}</Link> / {project.slug}
      </p>
      <div className="row">
        <h1>{project.slug}</h1>
        <Link className="button" to={`/projects/${encodeURIComponent(p)}/workspace`}>
          Open workspace
        </Link>
        <button type="button" className="secondary" onClick={loaded.reload} disabled={loaded.loading}>
          Refresh
        </button>
      </div>
      <ErrorBox error={loaded.error} />

      <h2>Active releases</h2>
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
                    <Link to={rel(head.release)}>
                      <code>{head.release}</code>
                    </Link>
                  )}
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>

      <h2>Releases</h2>
      {releases.length === 0 ? (
        <p className="muted">No releases yet. Open the workspace to build one with aho, or propose from the CLI.</p>
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
                    <Link to={rel(r.id)}>
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

      <h2>Domains</h2>
      <DomainsPanel project={p} domains={project.domains} onChanged={loaded.reload} />

      <h2>Secrets</h2>
      <p className="hint">Values are write-only: stored encrypted, never shown again. The fingerprint is what the taint scan looks for.</p>
      <SecretsPanel project={p} secrets={secrets} onChanged={loaded.reload} />

      <h2>Ledger</h2>
      <LedgerTimeline project={p} entries={ledger} />
    </section>
  );
}
