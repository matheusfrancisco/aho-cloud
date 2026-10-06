// Repo detail (server.md "UI"): indexed versions × knowledge layers, on-demand indexing, the
// recipe (viewer, Admin editor, Boot gate verify), deployments (Admin manual mapping) and customers.
import { Ago, Badge, Loading, Modal, useAction, useLoad } from "@aho/ui";
import { useState } from "react";
import { Link, useNavigate, useParams } from "react-router";
import type { OrgApi } from "../api/client";
import { ExternalLink, MageError, RunStatusBadge, Sha } from "../components/common";
import { LayerBadge, RecipeBadge } from "../components/Layers";
import { RecipeEditor, RecipeReportView, RecipeViewer } from "../components/Recipe";
import { useFeed, useMage } from "../context";
import type { Customer, CustomerInput, ExternalRef, RecipeReport, RepoDetail, VersionReason } from "../gen/mage";
import { LAYER_LABEL, LAYERS } from "../lib/labels";

const SHA_RE = /^[0-9a-f]{40}$/;

function reasonLabel(r: VersionReason): string {
  switch (r.type) {
    case "default_head":
      return "default head";
    case "release":
      return `release ${r.data}`;
    case "deployed":
      return `deployed to ${r.data}`;
    case "on_demand":
      return "on demand";
  }
}

function Versions({ api, detail }: { api: OrgApi; detail: RepoDetail }) {
  const [rev, setRev] = useState("");
  const [queued, setQueued] = useState<string | null>(null);
  const index = useAction(async () => {
    await api.indexVersion(detail.repo.id, { rev: rev.trim() });
    setQueued(rev.trim());
    setRev("");
  });
  return (
    <section className="card stack" aria-label="Versions">
      <h2>Indexed versions</h2>
      {detail.versions.length === 0 ? (
        <p className="muted">Nothing indexed yet.</p>
      ) : (
        <div style={{ overflowX: "auto" }}>
          <table className="layers" aria-label="Versions and layers">
            <thead>
              <tr>
                <th>Commit</th>
                <th>Why</th>
                {LAYERS.map((l) => (
                  <th key={l}>{LAYER_LABEL[l]}</th>
                ))}
              </tr>
            </thead>
            <tbody>
              {detail.versions.map((v) => (
                <tr key={v.sha}>
                  <td>
                    <Sha sha={v.sha} />
                  </td>
                  <td className="small">{v.reasons.map(reasonLabel).join(", ")}</td>
                  {LAYERS.map((l) => (
                    <td key={l}>
                      <LayerBadge state={v.layers[l]} />
                    </td>
                  ))}
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      <form
        className="row"
        onSubmit={(e) => {
          e.preventDefault();
          if (rev.trim() !== "") void index.run();
        }}
      >
        <input className="mage-mono grow" aria-label="Branch, tag or sha" placeholder="Branch, tag or sha to index" value={rev} onChange={(e) => setRev(e.target.value)} />
        <button type="submit" className="secondary" disabled={index.busy || rev.trim() === ""}>
          {index.busy ? "Queuing…" : "Index"}
        </button>
        {queued !== null && <span className="muted small">Indexing {queued} queued.</span>}
      </form>
      <MageError error={index.error} />
    </section>
  );
}

function RecipeSection({ api, detail, onChanged }: { api: OrgApi; detail: RepoDetail; onChanged: () => void }) {
  const { can, link } = useMage();
  const navigate = useNavigate();
  const history = useLoad(() => api.recipes(detail.repo.id), [api, detail.repo.id, detail.recipe?.hash]);
  const [editing, setEditing] = useState(false);
  const [report, setReport] = useState<RecipeReport | null>(null);
  const recipe = detail.recipe;
  const verify = useAction(async () => {
    if (recipe === null) return;
    const started = await api.verifyRecipe(detail.repo.id, { hash: recipe.hash });
    navigate(link(`runs/${started.run_id}`));
  });
  return (
    <section className="card stack" aria-label="Recipe">
      <div className="row">
        <h2>Recipe</h2>
        {recipe !== null && <RecipeBadge status={recipe.status} />}
        {recipe?.validated_at_sha !== null && recipe?.validated_at_sha !== undefined && (
          <span className="muted small">
            validated at <Sha sha={recipe.validated_at_sha} />
          </span>
        )}
        <span className="grow" />
        {can("admin") && !editing && (
          <button type="button" className="secondary" onClick={() => setEditing(true)}>
            {recipe === null ? "Write recipe" : "Edit"}
          </button>
        )}
        {can("admin") && recipe !== null && recipe.status.type !== "validated" && !editing && (
          <button type="button" onClick={() => void verify.run()} disabled={verify.busy}>
            {verify.busy ? "Starting…" : "Verify (Boot gate)"}
          </button>
        )}
      </div>
      {recipe?.status.type === "broken" && <p className="error">Broken: {recipe.status.reason}</p>}
      <MageError error={verify.error} />
      {report !== null && <RecipeReportView report={report} />}
      {editing ? (
        <>
          <RecipeEditor
            api={api}
            repo={detail.repo.id}
            initial={recipe?.body ?? ""}
            onSaved={(saved) => {
              setReport(saved.report);
              setEditing(false);
              onChanged();
            }}
          />
          <div>
            <button type="button" className="link" onClick={() => setEditing(false)}>
              Cancel editing
            </button>
          </div>
        </>
      ) : (
        <>
          <MageError error={history.error} />
          <RecipeViewer repo={detail.repo.id} current={history.data?.current ?? recipe} history={history.data?.history ?? []} />
        </>
      )}
    </section>
  );
}

function Deployments({ api, detail, onChanged }: { api: OrgApi; detail: RepoDetail; onChanged: () => void }) {
  const { can } = useMage();
  const [environment, setEnvironment] = useState("");
  const [sha, setSha] = useState("");
  const set = useAction(async () => {
    await api.setDeployment(detail.repo.id, environment.trim(), { sha });
    setEnvironment("");
    setSha("");
    onChanged();
  });
  return (
    <section className="card stack" aria-label="Deployments">
      <h2>Deployments</h2>
      {detail.deployments.length === 0 ? (
        <p className="muted">No deployments recorded.</p>
      ) : (
        <table>
          <thead>
            <tr>
              <th>Environment</th>
              <th>Commit</th>
              <th>Source</th>
              <th>Observed</th>
            </tr>
          </thead>
          <tbody>
            {detail.deployments.map((d) => (
              <tr key={`${d.environment}:${d.observed_at}`}>
                <td>{d.environment}</td>
                <td>
                  <Sha sha={d.sha} />
                </td>
                <td>{d.source.replaceAll("_", " ")}</td>
                <td>
                  <Ago ms={Date.parse(d.observed_at)} />
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      {can("admin") && (
        <form
          className="row"
          aria-label="Map a deployment"
          onSubmit={(e) => {
            e.preventDefault();
            if (environment.trim() !== "" && SHA_RE.test(sha)) void set.run();
          }}
        >
          <input aria-label="Environment" placeholder="Environment (e.g. production)" value={environment} onChange={(e) => setEnvironment(e.target.value)} />
          <input className="mage-mono grow" aria-label="Deployed sha" placeholder="40-character commit sha" value={sha} onChange={(e) => setSha(e.target.value.trim().toLowerCase())} aria-invalid={sha !== "" && !SHA_RE.test(sha)} />
          <button type="submit" className="secondary" disabled={set.busy || environment.trim() === "" || !SHA_RE.test(sha)}>
            {set.busy ? "Saving…" : "Set deployed commit"}
          </button>
        </form>
      )}
      <MageError error={set.error} />
    </section>
  );
}

function refText(r: ExternalRef): string {
  switch (r.type) {
    case "linear_customer":
      return `Linear customer ${r.data}`;
    case "linear_label":
      return `Linear label ${r.data}`;
    case "slack_channel":
      return `Slack ${r.data.team}/${r.data.channel}`;
  }
}

/** One ref per line: `linear_customer:<id>`, `linear_label:<name>`, `slack:<team>/<channel>`. */
function parseRefs(text: string): { refs: ExternalRef[]; bad: string[] } {
  const refs: ExternalRef[] = [];
  const bad: string[] = [];
  for (const raw of text.split("\n")) {
    const line = raw.trim();
    if (line === "") continue;
    const [kind, ...rest] = line.split(":");
    const value = rest.join(":").trim();
    if (kind === "linear_customer" && value !== "") refs.push({ type: "linear_customer", data: value });
    else if (kind === "linear_label" && value !== "") refs.push({ type: "linear_label", data: value });
    else if (kind === "slack" && /^[^/\s]+\/[^/\s]+$/.test(value)) {
      const [team, channel] = value.split("/") as [string, string];
      refs.push({ type: "slack_channel", data: { team, channel } });
    } else bad.push(line);
  }
  return { refs, bad };
}

function CustomerForm({ api, customer, onDone }: { api: OrgApi; customer: Customer | null; onDone: () => void }) {
  const [name, setName] = useState(customer?.name ?? "");
  const [environment, setEnvironment] = useState(customer?.environment ?? "");
  // The `parseRefs` format, one ref per line.
  const [refs, setRefs] = useState(() => (customer?.external_refs ?? []).map((r) => (r.type === "slack_channel" ? `slack:${r.data.team}/${r.data.channel}` : `${r.type}:${r.data}`)).join("\n"));
  const parsed = parseRefs(refs);
  const save = useAction(async () => {
    const body: CustomerInput = { name: name.trim(), external_refs: parsed.refs, environment: environment.trim() === "" ? undefined : environment.trim() };
    if (customer === null) await api.createCustomer(body);
    else await api.updateCustomer(customer.id, body);
    onDone();
  });
  return (
    <form
      className="stack"
      onSubmit={(e) => {
        e.preventDefault();
        if (name.trim() !== "" && parsed.bad.length === 0) void save.run();
      }}
    >
      <label>
        Name
        <input value={name} onChange={(e) => setName(e.target.value)} required />
      </label>
      <label>
        Environment
        <input value={environment} onChange={(e) => setEnvironment(e.target.value)} placeholder="production" />
      </label>
      <label>
        External refs
        <textarea className="mage-mono" value={refs} onChange={(e) => setRefs(e.target.value)} placeholder={"linear_customer:<id>\nlinear_label:<label>\nslack:<team>/<channel>"} />
      </label>
      {parsed.bad.length > 0 && <p className="field-error">Not understood: {parsed.bad.join(", ")}</p>}
      <button type="submit" disabled={save.busy || name.trim() === "" || parsed.bad.length > 0}>
        {save.busy ? "Saving…" : customer === null ? "Add customer" : "Save customer"}
      </button>
      <MageError error={save.error} />
    </form>
  );
}

function Customers({ api }: { api: OrgApi }) {
  const { can } = useMage();
  const loaded = useLoad(() => api.customers(), [api]);
  const [editing, setEditing] = useState<Customer | "new" | null>(null);
  useFeed((e) => e.kind === "settings", loaded.reload);
  const customers = loaded.data?.customers ?? [];
  return (
    <section className="card stack" aria-label="Customers">
      <div className="row">
        <h2>Customers</h2>
        <span className="grow" />
        {can("admin") && (
          <button type="button" className="secondary" onClick={() => setEditing("new")}>
            Add customer
          </button>
        )}
      </div>
      <MageError error={loaded.error} />
      {loaded.data === undefined ? (
        loaded.loading && <Loading what="customers" />
      ) : customers.length === 0 ? (
        <p className="muted">No customers yet.</p>
      ) : (
        <table>
          <thead>
            <tr>
              <th>Name</th>
              <th>Environment</th>
              <th>Refs</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {customers.map((c) => (
              <tr key={c.id}>
                <td>{c.name}</td>
                <td>{c.environment ?? <span className="muted">default</span>}</td>
                <td className="small">{c.external_refs.map(refText).join(", ")}</td>
                <td>
                  {can("admin") && (
                    <button type="button" className="link" onClick={() => setEditing(c)}>
                      Edit
                    </button>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      {editing !== null && (
        <Modal title={editing === "new" ? "Add customer" : `Edit ${editing.name}`} onClose={() => setEditing(null)}>
          <CustomerForm
            api={api}
            customer={editing === "new" ? null : editing}
            onDone={() => {
              setEditing(null);
              loaded.reload();
            }}
          />
        </Modal>
      )}
    </section>
  );
}

function Disconnect({ api, detail }: { api: OrgApi; detail: RepoDetail }) {
  const { link } = useMage();
  const navigate = useNavigate();
  const [confirming, setConfirming] = useState(false);
  const disconnect = useAction(async () => {
    await api.disconnectRepo(detail.repo.id);
    navigate(link("repos"));
  });
  return confirming ? (
    <>
      <span>Disconnect {detail.repo.name}?</span>
      <button type="button" className="danger small" onClick={() => void disconnect.run()} disabled={disconnect.busy}>
        {disconnect.busy ? "Disconnecting…" : "Disconnect"}
      </button>
      <button type="button" className="secondary small" onClick={() => setConfirming(false)}>
        Keep
      </button>
      <MageError error={disconnect.error} />
    </>
  ) : (
    <button type="button" className="secondary" onClick={() => setConfirming(true)}>
      Disconnect
    </button>
  );
}

function RepoView({ id }: { id: string }) {
  const { api, link, can } = useMage();
  const loaded = useLoad(() => api.repo(id), [api, id]);
  useFeed((e) => ((e.kind === "repo" || e.kind === "recipe") && e.subject === id) || e.kind === "run", loaded.reload);
  const detail = loaded.data;
  if (detail === undefined) return loaded.loading ? <Loading what="the repo" /> : <MageError error={loaded.error} />;
  const r = detail.repo;
  return (
    <div className="mage-page">
      <header className="mage-head">
        <h1>
          {r.owner}/{r.name}
        </h1>
        {r.access === "lost" && <Badge tone="bad">access lost</Badge>}
        <span className="muted small">default branch {r.default_branch}</span>
        <span className="grow" />
        <ExternalLink href={`https://github.com/${encodeURIComponent(r.owner)}/${encodeURIComponent(r.name)}`}>GitHub</ExternalLink>
        <Link to={link(`repos/${r.id}/onboard`)}>Onboarding</Link>
        {can("admin") && <Disconnect api={api} detail={detail} />}
      </header>
      <MageError error={loaded.error} />
      {detail.last_learn !== null && (
        <p>
          Last Learn run: <Link to={link(`runs/${detail.last_learn.id}`)}>{detail.last_learn.id}</Link> <RunStatusBadge status={detail.last_learn.status} />
        </p>
      )}
      <Versions api={api} detail={detail} />
      <RecipeSection api={api} detail={detail} onChanged={loaded.reload} />
      <Deployments api={api} detail={detail} onChanged={loaded.reload} />
      <Customers api={api} />
    </div>
  );
}

export function RepoPage() {
  const id = useParams().repo ?? "";
  return <RepoView key={id} id={id} />;
}
