// Settings → Model (read: Admin; change: Owner): where the gateway sends model calls, the allowed
// models and output cap, and the BYOK Anthropic key. The key is write-only: it goes in once and
// only its fingerprint, who set it and when come back. The form never shows a stored key.
import { Ago, Loading, useAction, useLoad } from "@aho/ui";
import { useState } from "react";
import type { OrgApi } from "../../api/client";
import { MageError } from "../../components/common";
import { useFeed, useMage } from "../../context";
import type { AuditActor, ModelKeyMeta, ModelRouteKind, ModelSettingsView } from "../../gen/mage";

function actorText(a: AuditActor): string {
  switch (a.type) {
    case "user":
      return `user ${a.id}`;
    case "runner":
      return `runner ${a.id}`;
    case "operator":
      return `operator ${a.os_user}`;
    case "system":
      return "Mage";
  }
}

function RouteForm({ api, view, onSaved }: { api: OrgApi; view: ModelSettingsView; onSaved: () => void }) {
  const { can } = useMage();
  const owner = can("owner");
  const [route, setRoute] = useState<ModelRouteKind>(view.route);
  const [models, setModels] = useState(view.allowed_models.join("\n"));
  const [cap, setCap] = useState(view.max_tokens_cap === null ? "" : String(view.max_tokens_cap));
  const capValue = cap.trim() === "" ? undefined : Number(cap);
  const capOk = capValue === undefined || (Number.isInteger(capValue) && capValue > 0);
  const save = useAction(async () => {
    await api.saveModelSettings({
      route,
      allowed_models: models
        .split(/[\n,]/)
        .map((m) => m.trim())
        .filter((m) => m !== ""),
      max_tokens_cap: capValue,
    });
    onSaved();
  });
  return (
    <form
      className="stack"
      onSubmit={(e) => {
        e.preventDefault();
        if (capOk) void save.run();
      }}
    >
      <fieldset className="stack" disabled={!owner}>
        <label>
          Route
          <select value={route} onChange={(e) => setRoute(e.target.value === "vertex" ? "vertex" : "anthropic")}>
            <option value="anthropic">Anthropic API</option>
            {(view.vertex_available || view.route === "vertex") && <option value="vertex">Vertex AI</option>}
          </select>
        </label>
        <label>
          Allowed models (one per line)
          <textarea className="mage-mono" value={models} onChange={(e) => setModels(e.target.value)} spellCheck={false} />
        </label>
        <label>
          Output token cap per request
          <input type="number" min={1} value={cap} onChange={(e) => setCap(e.target.value)} placeholder="no cap" aria-invalid={!capOk} />
        </label>
      </fieldset>
      {owner ? (
        <div className="row">
          <button type="submit" disabled={save.busy || !capOk}>
            {save.busy ? "Saving…" : "Save model settings"}
          </button>
        </div>
      ) : (
        <p className="hint">Only owners change the model route.</p>
      )}
      <MageError error={save.error} />
    </form>
  );
}

/** The BYOK key: metadata only; the input is write-only and cleared as soon as the key is sent. */
export function ModelKeyForm({ api, meta, onChanged }: { api: OrgApi; meta: ModelKeyMeta | null; onChanged: () => void }) {
  const { can } = useMage();
  const owner = can("owner");
  const [key, setKey] = useState("");
  const [removing, setRemoving] = useState(false);
  const set = useAction(async () => {
    const candidate = key.trim();
    // The key leaves the form state before the request: nothing renders it again, even on failure.
    setKey("");
    await api.setModelKey({ api_key: candidate });
    onChanged();
  });
  const remove = useAction(async () => {
    await api.deleteModelKey();
    setRemoving(false);
    onChanged();
  });
  return (
    <section className="stack" aria-label="Anthropic API key">
      <h3>Anthropic API key (BYOK)</h3>
      {meta === null ? (
        <p className="muted">No key set: model calls use the server's key.</p>
      ) : (
        <dl className="facts">
          <dt>Key</dt>
          <dd>
            <code>{meta.fingerprint}</code>
          </dd>
          <dt>Set</dt>
          <dd>
            <Ago ms={Date.parse(meta.set_at)} /> by {actorText(meta.set_by)}
          </dd>
        </dl>
      )}
      {owner ? (
        <>
          <form
            className="row"
            autoComplete="off"
            onSubmit={(e) => {
              e.preventDefault();
              if (key.trim() !== "") void set.run();
            }}
          >
            <input className="grow" type="password" name="mage-byok" autoComplete="new-password" spellCheck={false} aria-label={meta === null ? "New API key" : "Replacement API key"} placeholder="sk-ant-…" value={key} onChange={(e) => setKey(e.target.value)} />
            <button type="submit" disabled={set.busy || key.trim() === ""}>
              {set.busy ? "Checking key…" : meta === null ? "Set key" : "Replace key"}
            </button>
          </form>
          <p className="hint">Mage checks the key with Anthropic before storing it encrypted. It is never shown again.</p>
          <MageError error={set.error} />
          {meta !== null &&
            (removing ? (
              <div className="row">
                <span>Remove the key? Model calls fall back to the server's key.</span>
                <button type="button" className="danger small" onClick={() => void remove.run()} disabled={remove.busy}>
                  {remove.busy ? "Removing…" : "Remove key"}
                </button>
                <button type="button" className="secondary small" onClick={() => setRemoving(false)}>
                  Keep
                </button>
              </div>
            ) : (
              <div>
                <button type="button" className="secondary" onClick={() => setRemoving(true)}>
                  Remove key…
                </button>
              </div>
            ))}
          <MageError error={remove.error} />
        </>
      ) : (
        <p className="hint">Only owners set or remove the key.</p>
      )}
    </section>
  );
}

export function ModelSettings() {
  const { api, can } = useMage();
  const admin = can("admin");
  const loaded = useLoad(() => (admin ? api.modelSettings() : Promise.resolve(null)), [api, admin]);
  useFeed((e) => e.kind === "settings", loaded.reload);
  if (!admin) return <p className="muted">Only admins see the model settings.</p>;
  if (loaded.data === undefined || loaded.data === null) return loaded.loading ? <Loading what="model settings" /> : <MageError error={loaded.error} />;
  const view = loaded.data;
  return (
    <div className="stack">
      <section className="card stack" aria-label="Model route">
        <h3>Model route</h3>
        <RouteForm key={JSON.stringify([view.route, view.allowed_models, view.max_tokens_cap])} api={api} view={view} onSaved={loaded.reload} />
      </section>
      <section className="card">
        <ModelKeyForm api={api} meta={view.key} onChanged={loaded.reload} />
      </section>
    </div>
  );
}
