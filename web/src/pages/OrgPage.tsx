import { useState } from "react";
import { Link, useNavigate, useParams } from "react-router";
import { api } from "../lib/api";
import { STAGES } from "../lib/release";
import { ErrorBox, Loading } from "../ui/common";
import { useAction, useLoad } from "../ui/hooks";

export function OrgPage() {
  const org = useParams().org ?? "";
  const projects = useLoad(() => api.projects(org), [org]);
  const navigate = useNavigate();
  const [slug, setSlug] = useState("");
  const create = useAction(async () => {
    const p = await api.createProject(org, { slug: slug.trim() });
    navigate(`/projects/${encodeURIComponent(p.slug)}`);
  });

  return (
    <section>
      <p className="crumbs">
        <Link to="/">Organizations</Link> / {org}
      </p>
      <h1>Projects</h1>
      <ErrorBox error={projects.error} />
      {projects.data === undefined ? (
        projects.loading && <Loading what="projects" />
      ) : projects.data.length === 0 ? (
        <p className="muted">No projects yet.</p>
      ) : (
        <table>
          <thead>
            <tr>
              <th>Project</th>
              {STAGES.map((s) => (
                <th key={s}>{s}</th>
              ))}
              <th>Domains</th>
            </tr>
          </thead>
          <tbody>
            {projects.data.map((p) => (
              <tr key={p.slug}>
                <td>
                  <Link to={`/projects/${encodeURIComponent(p.slug)}`}>{p.slug}</Link>
                </td>
                {STAGES.map((s) => {
                  const head = p.active.find((a) => a.stage === s);
                  return <td key={s}>{head === undefined ? <span className="muted">—</span> : <code>{head.release}</code>}</td>;
                })}
                <td>{p.domains.map((d) => d.hostname).join(", ")}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      <h2>New project</h2>
      <form
        className="row"
        onSubmit={(e) => {
          e.preventDefault();
          void create.run();
        }}
      >
        <label>
          Slug
          <input value={slug} onChange={(e) => setSlug(e.target.value.toLowerCase())} pattern="[a-z][a-z0-9-]{0,39}" placeholder="shop" required />
        </label>
        <button type="submit" disabled={create.busy}>
          Create project
        </button>
      </form>
      <ErrorBox error={create.error} />
    </section>
  );
}
