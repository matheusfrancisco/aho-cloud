// Repos list (server.md "UI"): layer status at the default head, recipe status, last Learn run;
// Admins connect installation repos, which opens the onboarding wizard.
import { Badge, Loading, Modal, PlusIcon, useAction, useLoad } from "@aho/ui";
import { useState } from "react";
import { Link, useNavigate } from "react-router";
import type { OrgApi } from "../api/client";
import { MageError, RunStatusBadge } from "../components/common";
import { LayerSummary, RecipeBadge } from "../components/Layers";
import { useFeed, useMage } from "../context";

function ConnectRepoModal({ api, onClose }: { api: OrgApi; onClose: () => void }) {
  const { link } = useMage();
  const navigate = useNavigate();
  const available = useLoad(() => api.installationRepos(), [api]);
  const [selected, setSelected] = useState<number | null>(null);
  const connect = useAction(async () => {
    if (selected === null) return;
    const repo = await api.connectRepo({ external_id: selected });
    onClose();
    navigate(link(`repos/${repo.id}/onboard`));
  });
  const repos = available.data?.repositories ?? [];
  return (
    <Modal title="Connect a repository" onClose={onClose}>
      <p className="hint">
        Repositories the GitHub app can read and that are not connected yet. Missing one? Grant the app access to it in <Link to={link("settings/integrations")}>Integrations</Link>.
      </p>
      <MageError error={available.error} />
      {available.data === undefined ? (
        available.loading && <Loading what="repositories" />
      ) : repos.length === 0 ? (
        <p className="muted">Every repository the app can read is connected.</p>
      ) : (
        <form
          className="stack"
          onSubmit={(e) => {
            e.preventDefault();
            void connect.run();
          }}
        >
          <fieldset className="stack">
            <legend>Repository</legend>
            {repos.map((r) => (
              <label key={r.external_id} className="choice">
                <input type="radio" name="repo" checked={selected === r.external_id} onChange={() => setSelected(r.external_id)} />
                {r.owner}/{r.name} {r.private && <Badge tone="muted">private</Badge>}
              </label>
            ))}
          </fieldset>
          <button type="submit" disabled={selected === null || connect.busy}>
            {connect.busy ? "Connecting…" : "Connect and onboard"}
          </button>
          <MageError error={connect.error} />
        </form>
      )}
    </Modal>
  );
}

export function ReposPage() {
  const { api, link, can } = useMage();
  const loaded = useLoad(() => api.repos(), [api]);
  const [connecting, setConnecting] = useState(false);
  useFeed((e) => e.kind === "repo" || e.kind === "recipe" || e.kind === "run", loaded.reload);
  const repos = loaded.data?.repos ?? [];

  return (
    <div className="mage-page">
      <header className="mage-head">
        <h1>Repos</h1>
        <span className="grow" />
        {can("admin") && (
          <button type="button" onClick={() => setConnecting(true)}>
            <PlusIcon /> Connect repo
          </button>
        )}
      </header>
      <MageError error={loaded.error} />
      {loaded.data === undefined ? (
        loaded.loading && <Loading what="repos" />
      ) : repos.length === 0 ? (
        <p className="muted">No repositories connected yet.</p>
      ) : (
        <table aria-label="Repos">
          <thead>
            <tr>
              <th>Repo</th>
              <th>Index at default head</th>
              <th>Recipe</th>
              <th>Last Learn run</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {repos.map(({ repo, head, recipe, last_learn }) => (
              <tr key={repo.id}>
                <td>
                  <Link to={link(`repos/${repo.id}`)}>
                    {repo.owner}/{repo.name}
                  </Link>
                  {repo.access === "lost" && (
                    <>
                      {" "}
                      <Badge tone="bad">access lost</Badge>
                    </>
                  )}
                </td>
                <td>
                  <LayerSummary version={head} />
                </td>
                <td>{recipe === null ? <span className="muted">none</span> : <RecipeBadge status={recipe.status} />}</td>
                <td>{last_learn === null ? <span className="muted">—</span> : <Link to={link(`runs/${last_learn.id}`)}>{<RunStatusBadge status={last_learn.status} />}</Link>}</td>
                <td>{recipe?.status.type !== "validated" && <Link to={link(`repos/${repo.id}/onboard`)}>Onboard</Link>}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      {connecting && <ConnectRepoModal api={api} onClose={() => setConnecting(false)} />}
    </div>
  );
}
