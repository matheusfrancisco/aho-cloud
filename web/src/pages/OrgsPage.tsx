import { useState } from "react";
import { Link, useNavigate } from "react-router";
import { api } from "../lib/api";
import { Badge, ErrorBox, Loading } from "../ui/common";
import { useAction, useLoad } from "../ui/hooks";

export function OrgsPage() {
  const me = useLoad(() => api.me(), []);
  const navigate = useNavigate();
  const [slug, setSlug] = useState("");
  const [name, setName] = useState("");
  const create = useAction(async () => {
    const org = await api.createOrg({ slug: slug.trim(), name: name.trim() });
    navigate(`/orgs/${encodeURIComponent(org.slug)}`);
  });

  return (
    <section>
      <h1>Organizations</h1>
      <ErrorBox error={me.error} />
      {me.loading && me.data === undefined ? (
        <Loading what="organizations" />
      ) : me.data !== undefined && me.data.orgs.length === 0 ? (
        <p className="muted">You are not in any organization yet. Create one below, or ask an owner to add {me.data.user.email}.</p>
      ) : (
        <ul className="list">
          {me.data?.orgs.map((m) => (
            <li key={m.org}>
              <Link to={`/orgs/${encodeURIComponent(m.org)}`}>{m.org}</Link> {m.owner && <Badge tone="ok">owner</Badge>}
            </li>
          ))}
        </ul>
      )}
      <h2>New organization</h2>
      <form
        className="row"
        onSubmit={(e) => {
          e.preventDefault();
          void create.run();
        }}
      >
        <label>
          Slug
          <input value={slug} onChange={(e) => setSlug(e.target.value.toLowerCase())} pattern="[a-z][a-z0-9-]{0,39}" placeholder="acme" required />
        </label>
        <label>
          Name
          <input value={name} onChange={(e) => setName(e.target.value)} placeholder="Acme Inc." required />
        </label>
        <button type="submit" disabled={create.busy}>
          Create
        </button>
      </form>
      <ErrorBox error={create.error} />
    </section>
  );
}
