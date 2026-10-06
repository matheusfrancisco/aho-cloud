// Settings → Audit (Admin): the org's audit log, newest first, filtered by action, paged with
// `before`.
import { Badge, Loading, Time } from "@aho/ui";
import { useEffect, useState } from "react";
import { MageError } from "../../components/common";
import { useMage } from "../../context";
import type { AuditActor, AuditEntry } from "../../gen/mage";

function actorText(a: AuditActor, names: ReadonlyMap<string, string>): string {
  switch (a.type) {
    case "user":
      return names.get(a.id) ?? a.id;
    case "runner":
      return `runner ${a.id}`;
    case "operator":
      return `operator ${a.os_user}`;
    case "system":
      return "Mage";
  }
}

const PAGE = 100;

export function AuditSettings() {
  const { api, can } = useMage();
  const admin = can("admin");
  const [action, setAction] = useState("");
  const [filter, setFilter] = useState("");
  const [entries, setEntries] = useState<AuditEntry[]>([]);
  const [before, setBefore] = useState<number | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<unknown>(null);
  const [names, setNames] = useState<ReadonlyMap<string, string>>(new Map());

  const load = async (from: number | null, append: boolean) => {
    setLoading(true);
    setError(null);
    try {
      const page = await api.audit({ action: filter === "" ? undefined : filter, before: from ?? undefined, limit: PAGE });
      setEntries((prev) => (append ? [...prev, ...page.entries] : page.entries));
      setBefore(page.next_before);
    } catch (e) {
      setError(e);
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    if (admin) void load(null, false);
    // `load` reads the current filter; reloading on filter change is the intent.
  }, [api, admin, filter]);

  useEffect(() => {
    if (!admin) return;
    api.members().then(
      (m) => setNames(new Map(m.members.map((v) => [v.user.id, v.user.name]))),
      () => setNames(new Map()),
    );
  }, [api, admin]);

  if (!admin) return <p className="muted">Only admins read the audit log.</p>;
  return (
    <div className="stack">
      <form
        className="row"
        onSubmit={(e) => {
          e.preventDefault();
          setFilter(action.trim());
        }}
      >
        <input aria-label="Action" placeholder="Action, e.g. member.invited" value={action} onChange={(e) => setAction(e.target.value)} />
        <button type="submit" className="secondary">
          Filter
        </button>
      </form>
      <MageError error={error} />
      {entries.length === 0 ? (
        loading ? (
          <Loading what="the audit log" />
        ) : (
          <p className="muted">No entries.</p>
        )
      ) : (
        <table aria-label="Audit log">
          <thead>
            <tr>
              <th>When</th>
              <th>Who</th>
              <th>Action</th>
              <th>Target</th>
              <th>Outcome</th>
              <th>Details</th>
            </tr>
          </thead>
          <tbody>
            {entries.map((e) => (
              <tr key={e.id}>
                <td>
                  <Time ms={Date.parse(e.at)} />
                </td>
                <td>{actorText(e.actor, names)}</td>
                <td>
                  <code>{e.action}</code>
                </td>
                <td className="small">
                  {e.target_kind} <code>{e.target_id}</code>
                </td>
                <td>
                  <Badge tone={e.outcome === "success" ? "ok" : e.outcome === "denied" ? "warn" : "bad"}>{e.outcome}</Badge>
                </td>
                <td className="small">
                  {e.details.map((d) => `${d.key}=${d.value}`).join(" ")}
                  {e.ip !== null && <div className="muted">ip {e.ip}</div>}
                  {e.request_id !== null && <div className="muted">request {e.request_id}</div>}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      {before !== null && (
        <div>
          <button type="button" className="secondary" onClick={() => void load(before, true)} disabled={loading}>
            {loading ? "Loading…" : "Older entries"}
          </button>
        </div>
      )}
    </div>
  );
}
