// Org members. Owners add people by the email they signed in with; the member count caps how
// many approvals a release can be asked for.
import { useEffect, useState } from "react";
import { useParams } from "react-router";
import { Badge, ErrorBox, Loading, useAction, useLoad } from "@aho/ui";
import { useShell } from "../components/Shell";
import { api } from "../lib/api";
import { setCurrentOrg } from "../lib/remembered";

export function TeamPage() {
  const org = useParams().org ?? "";
  const { me } = useShell();
  useEffect(() => setCurrentOrg(org), [org]);
  const members = useLoad(() => api.members(org), [org]);
  const owner = me?.orgs.some((m) => m.org === org && m.owner) ?? false;
  const [email, setEmail] = useState("");
  const [asOwner, setAsOwner] = useState(false);
  const add = useAction(async () => {
    await api.addMember(org, { email: email.trim(), owner: asOwner });
    setEmail("");
    setAsOwner(false);
    members.reload();
  });

  return (
    <section className="card page-card">
      <h1>Team</h1>
      <p className="hint">
        Members of <strong>{org}</strong> see every project and can approve releases. A release never needs more approvals than the org has members.
      </p>
      <ErrorBox error={members.error} />
      {members.data === undefined ? (
        members.loading && <Loading what="members" />
      ) : (
        <ul className="member-list">
          {members.data.map((m) => (
            <li key={m.user.id}>
              <span className="avatar" aria-hidden="true">
                {(m.user.name || m.user.email).slice(0, 1).toUpperCase()}
              </span>
              <span className="grow">
                <strong>{m.user.name}</strong> <span className="muted">{m.user.email}</span>
              </span>
              {m.owner ? <Badge tone="info">owner</Badge> : <Badge tone="muted">member</Badge>}
            </li>
          ))}
        </ul>
      )}
      {owner ? (
        <form
          className="row add-member"
          onSubmit={(e) => {
            e.preventDefault();
            void add.run();
          }}
        >
          <label className="grow">
            Email
            <input type="email" value={email} onChange={(e) => setEmail(e.target.value)} placeholder="bo@example.com" required />
          </label>
          <label className="check">
            <input type="checkbox" checked={asOwner} onChange={(e) => setAsOwner(e.target.checked)} /> Owner
          </label>
          <button type="submit" disabled={add.busy || email.trim() === ""}>
            {add.busy ? "Adding…" : "Add member"}
          </button>
        </form>
      ) : (
        <p className="muted">Only owners can add members.</p>
      )}
      <ErrorBox error={add.error} />
    </section>
  );
}
