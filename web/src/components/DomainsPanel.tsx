// Custom domains: the DNS records to create, the binding state, and a verify button.
import { useState } from "react";
import type { DomainState, DomainView } from "../gen/api";
import type { DomainMode } from "../gen/contract";
import { api } from "../lib/api";
import { Badge, ErrorBox } from "../ui/common";
import { useAction } from "../ui/hooks";

const STATE_TONE: Record<DomainState, "ok" | "warn" | "bad" | "info" | "muted"> = {
  pending: "warn",
  verified: "ok",
  grace: "warn",
  detached: "bad",
  tombstoned: "muted",
};

const STATE_HINT: Record<DomainState, string> = {
  pending: "Create the records below, then verify.",
  verified: "Serving. Keep the records in place.",
  grace: "The TXT record disappeared; serving continues until the grace period ends. Restore it.",
  detached: "No longer serving: the records are gone.",
  tombstoned: "Removed; the hostname shows a tombstone page.",
};

function DomainRow({ project, domain, onChanged }: { project: string; domain: DomainView; onChanged: () => void }) {
  const verify = useAction(async () => {
    await api.verifyDomain(project, domain.hostname);
    onChanged();
  });
  return (
    <li className="domain">
      <div className="row">
        <strong>{domain.hostname}</strong>
        <Badge tone={STATE_TONE[domain.state]}>{domain.state}</Badge>
        <span className="muted">{domain.mode.replace("_", " ")}</span>
        <button type="button" onClick={() => void verify.run()} disabled={verify.busy || domain.state === "tombstoned"}>
          {verify.busy ? "Checking…" : `Verify ${domain.hostname}`}
        </button>
      </div>
      <p className="hint">{STATE_HINT[domain.state]}</p>
      <table className="records">
        <thead>
          <tr>
            <th>Type</th>
            <th>Name</th>
            <th>Value</th>
          </tr>
        </thead>
        <tbody>
          {domain.records.map((r) => (
            <tr key={`${r.kind}:${r.name}:${r.value}`}>
              <td>{r.kind}</td>
              <td>
                <code>{r.name}</code>
              </td>
              <td>
                <code className="wrap">{r.value}</code>
              </td>
            </tr>
          ))}
        </tbody>
      </table>
      <ErrorBox error={verify.error} />
    </li>
  );
}

/** Domains of a project; `onChanged` reloads them after an add or a verification. */
export function DomainsPanel({ project, domains, onChanged }: { project: string; domains: DomainView[]; onChanged: () => void }) {
  const [hostname, setHostname] = useState("");
  const [mode, setMode] = useState<DomainMode>("cname");
  const add = useAction(async () => {
    await api.addDomain(project, { hostname: hostname.trim().toLowerCase(), mode });
    setHostname("");
    onChanged();
  });

  return (
    <div>
      {domains.length === 0 ? <p className="muted">No custom domains yet.</p> : (
        <ul className="domains">
          {domains.map((d) => (
            <DomainRow key={d.hostname} project={project} domain={d} onChanged={onChanged} />
          ))}
        </ul>
      )}
      <form
        className="row"
        onSubmit={(e) => {
          e.preventDefault();
          void add.run();
        }}
      >
        <label>
          Hostname
          <input value={hostname} onChange={(e) => setHostname(e.target.value)} placeholder="shop.example.com" required />
        </label>
        <label>
          Verification
          <select value={mode} onChange={(e) => setMode(e.target.value as DomainMode)}>
            <option value="cname">CNAME (subdomain)</option>
            <option value="apex">A/AAAA (apex)</option>
            <option value="ns_delegated">NS delegation</option>
          </select>
        </label>
        <button type="submit" disabled={add.busy || hostname.trim() === ""}>
          Add domain
        </button>
      </form>
      <ErrorBox error={add.error} />
    </div>
  );
}
