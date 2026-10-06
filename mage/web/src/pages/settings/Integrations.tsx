// Settings → Integrations (Admin; the GitHub fix app: Owner): status per integration, connect
// (the server returns the provider URL to send the browser to) and revoke.
import { Badge, Loading, useAction, useLoad } from "@aho/ui";
import { useState } from "react";
import type { OrgApi } from "../../api/client";
import { MageError } from "../../components/common";
import { useFeed, useMage } from "../../context";
import type { Integration, IntegrationKind } from "../../gen/mage";
import { INTEGRATION_LABEL } from "../../lib/labels";
import { safeHref } from "../../lib/markdown";

const KINDS: readonly IntegrationKind[] = ["github", "linear", "slack", "github_fix"];

const PURPOSE: Record<IntegrationKind, string> = {
  github: "Read access to repositories, pushes and deployments.",
  linear: "Tickets come from Linear; Mage comments with its findings.",
  slack: "Replies in the support thread linked to a ticket.",
  github_fix: "Write access for pull requests with fixes (only when pull requests are enabled).",
};

function Revoke({ api, integration, onDone }: { api: OrgApi; integration: Integration; onDone: () => void }) {
  const [confirming, setConfirming] = useState(false);
  const revoke = useAction(async () => {
    await api.revokeIntegration(integration.kind, integration.external_id);
    setConfirming(false);
    onDone();
  });
  return (
    <>
      {confirming ? (
        <>
          <button type="button" className="danger small" onClick={() => void revoke.run()} disabled={revoke.busy}>
            {revoke.busy ? "Revoking…" : "Confirm revoke"}
          </button>
          <button type="button" className="secondary small" onClick={() => setConfirming(false)}>
            Keep
          </button>
        </>
      ) : (
        <button type="button" className="secondary small" onClick={() => setConfirming(true)}>
          Revoke
        </button>
      )}
      <MageError error={revoke.error} />
    </>
  );
}

export function IntegrationsSettings() {
  const { api, can } = useMage();
  const admin = can("admin");
  const loaded = useLoad(() => (admin ? api.integrations() : Promise.resolve(null)), [api, admin]);
  useFeed((e) => e.kind === "integration", loaded.reload);
  const connect = useAction(async (kind: IntegrationKind) => {
    const { url } = await api.connectIntegration(kind);
    const target = safeHref(url);
    if (target === null) throw new Error(`the server returned an unexpected provider URL: ${url}`);
    window.location.assign(target);
  });
  if (!admin) return <p className="muted">Only admins manage integrations.</p>;
  if (loaded.data === undefined || loaded.data === null) return loaded.loading ? <Loading what="integrations" /> : <MageError error={loaded.error} />;
  const all = loaded.data.integrations;
  return (
    <div className="stack">
      <MageError error={connect.error} />
      {KINDS.map((kind) => {
        const mine = all.filter((i) => i.kind === kind);
        const allowed = kind === "github_fix" ? can("owner") : true;
        return (
          <section key={kind} className="card stack" aria-label={INTEGRATION_LABEL[kind]}>
            <div className="row">
              <h3>{INTEGRATION_LABEL[kind]}</h3>
              <span className="grow" />
              {allowed && (
                <button type="button" className={mine.some((i) => i.status === "active") ? "secondary" : undefined} onClick={() => void connect.run(kind)} disabled={connect.busy}>
                  {mine.some((i) => i.status === "active") ? "Connect another" : "Connect"}
                </button>
              )}
            </div>
            <p className="hint">{PURPOSE[kind]}</p>
            {!allowed && <p className="hint">Only owners connect or revoke the fix app (it can write code).</p>}
            {mine.length === 0 ? (
              <p className="muted">Not connected.</p>
            ) : (
              <ul>
                {mine.map((i) => (
                  <li key={i.external_id} className="row">
                    <code>{i.external_id}</code>
                    <Badge tone={i.status === "active" ? "ok" : "muted"}>{i.status}</Badge>
                    {allowed && i.status === "active" && <Revoke api={api} integration={i} onDone={loaded.reload} />}
                  </li>
                ))}
              </ul>
            )}
          </section>
        );
      })}
    </div>
  );
}
