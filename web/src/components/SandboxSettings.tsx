// A sandbox's settings (egress allowlist, public port, auto-stop) and its environment variables.
// The settings fields are shared with the create form. Variables are write-only like secrets:
// names and fingerprints come back, values never do.
import { useState } from "react";
import { CloseIcon, ErrorBox, Hash, Loading, useAction, useLoad } from "@aho/ui";
import type { EnvVarView, Sandbox } from "../gen/api";
import { api } from "../lib/api";
import { AUTO_STOP_MAX_HOURS, checkSettings, egressProblem, envNameProblem, type SettingsDraft, settingsDraft, type SettingsProblems } from "../lib/sandbox";

/** Egress list, public port and auto-stop hours. `problems` shows once the user tried to submit. */
export function SettingsFields({ draft, onChange, problems }: { draft: SettingsDraft; onChange: (d: SettingsDraft) => void; problems: SettingsProblems | null }) {
  const [host, setHost] = useState("");
  const [hostError, setHostError] = useState<string | null>(null);
  const add = () => {
    const h = host.trim().toLowerCase();
    if (h === "") return;
    const problem = egressProblem(h);
    setHostError(problem);
    if (problem !== null) return;
    if (!draft.egress.includes(h)) onChange({ ...draft, egress: [...draft.egress, h] });
    setHost("");
  };
  return (
    <>
      <fieldset>
        <legend>Egress</legend>
        <p className="hint">
          Hosts the VM may reach; everything else is refused. <code>*.example.com</code> covers its subdomains.
        </p>
        {draft.egress.length === 0 ? (
          <p className="muted small">No hosts: the VM reaches nothing outside.</p>
        ) : (
          <ul className="egress-list" aria-label="Allowed hosts">
            {draft.egress.map((h) => (
              <li key={h}>
                <code>{h}</code>
                <button type="button" className="icon-button" aria-label={`Remove ${h}`} onClick={() => onChange({ ...draft, egress: draft.egress.filter((e) => e !== h) })}>
                  <CloseIcon />
                </button>
              </li>
            ))}
          </ul>
        )}
        <div className="row">
          <input
            value={host}
            onChange={(e) => {
              setHost(e.target.value);
              setHostError(null);
            }}
            onKeyDown={(e) => {
              if (e.key === "Enter") {
                e.preventDefault();
                add();
              }
            }}
            placeholder="api.example.com"
            aria-label="Host to allow"
            aria-invalid={hostError !== null}
            autoComplete="off"
            spellCheck={false}
          />
          <button type="button" className="secondary small" onClick={add} disabled={host.trim() === ""}>
            Add host
          </button>
        </div>
        {hostError !== null && <p className="field-error">{hostError}</p>}
      </fieldset>
      <label>
        Public port
        <input value={draft.port} onChange={(e) => onChange({ ...draft, port: e.target.value })} inputMode="numeric" placeholder="none" aria-invalid={problems?.port !== undefined} />
      </label>
      <p className={problems?.port === undefined ? "hint" : "field-error"}>
        {problems?.port ?? "Served at https://<sandbox>.<preview domain>, behind email sign-in open to your org's members. Leave empty for none."}
      </p>
      <label>
        Auto-stop after (hours)
        <input type="number" value={draft.autoStopHours} onChange={(e) => onChange({ ...draft, autoStopHours: e.target.value })} aria-invalid={problems?.autoStopHours !== undefined} />
      </label>
      <p className={problems?.autoStopHours === undefined ? "hint" : "field-error"}>
        {problems?.autoStopHours ?? `The VM stops this long after each start; its disk is kept. 1 to ${AUTO_STOP_MAX_HOURS} hours.`}
      </p>
    </>
  );
}

function SettingsForm({ sandbox, onChanged }: { sandbox: Sandbox; onChanged: () => void }) {
  const [draft, setDraft] = useState(() => settingsDraft(sandbox.settings));
  const [problems, setProblems] = useState<SettingsProblems | null>(null);
  const [saved, setSaved] = useState(false);
  const save = useAction(async () => {
    const checked = checkSettings(draft);
    setProblems(checked.problems);
    if (checked.settings === null) return;
    await api.setSandboxSettings(sandbox.slug, checked.settings);
    onChanged();
    setSaved(true);
  });
  const running = sandbox.state === "running" || sandbox.state === "starting";
  return (
    <form
      className="stack sandbox-settings"
      onSubmit={(e) => {
        e.preventDefault();
        setSaved(false);
        void save.run();
      }}
    >
      <SettingsFields draft={draft} onChange={setDraft} problems={problems} />
      {running && <p className="note">Saving restarts the running sandbox with these settings; its disk is kept, open terminals and chats disconnect.</p>}
      <div className="row">
        <button type="submit" disabled={save.busy}>
          {save.busy ? "Saving…" : "Save settings"}
        </button>
        {saved && <span className="ok">Saved.</span>}
      </div>
      <ErrorBox error={save.error} />
    </form>
  );
}

function EnvVars({ sandbox }: { sandbox: string }) {
  const vars = useLoad(() => api.sandboxEnv(sandbox), [sandbox]);
  const [name, setName] = useState("");
  const [value, setValue] = useState("");
  const [saved, setSaved] = useState<string | null>(null);
  const problem = envNameProblem(name);
  const set = useAction(async () => {
    const view = await api.setSandboxEnv(sandbox, { name, value });
    setValue("");
    setName("");
    setSaved(`${view.name} set; it applies at the sandbox's next start.`);
    vars.reload();
  });
  const remove = useAction(async (v: EnvVarView) => {
    await api.deleteSandboxEnv(sandbox, v.name);
    setSaved(null);
    vars.reload();
  });
  return (
    <section aria-label="Environment variables">
      <h3>Environment variables</h3>
      <p className="hint">
        Stored encrypted and never shown again; they reach the VM at its next start. <code>ANTHROPIC_API_KEY</code> powers the aho inside.
      </p>
      <ErrorBox error={vars.error} />
      {vars.data === undefined ? (
        vars.loading && <Loading what="variables" />
      ) : vars.data.length === 0 ? (
        <p className="muted">No variables set.</p>
      ) : (
        <table>
          <thead>
            <tr>
              <th>Name</th>
              <th>Fingerprint</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {vars.data.map((v) => (
              <tr key={v.name}>
                <td>
                  <code>{v.name}</code>
                </td>
                <td>
                  <Hash value={v.fingerprint} />
                </td>
                <td>
                  <button type="button" className="secondary small" aria-label={`Delete ${v.name}`} disabled={remove.busy} onClick={() => void remove.run(v)}>
                    Delete
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      <ErrorBox error={remove.error} />
      <form
        className="secret-form row"
        autoComplete="off"
        onSubmit={(e) => {
          e.preventDefault();
          setSaved(null);
          if (problem === null && value !== "") void set.run();
        }}
      >
        <label>
          Name
          <input value={name} onChange={(e) => setName(e.target.value.toUpperCase())} placeholder="ANTHROPIC_API_KEY" spellCheck={false} aria-invalid={name !== "" && problem !== null} />
        </label>
        <label>
          Value
          <input type="password" value={value} onChange={(e) => setValue(e.target.value)} autoComplete="new-password" spellCheck={false} />
        </label>
        <button type="submit" disabled={set.busy || problem !== null || value === ""}>
          Set variable
        </button>
      </form>
      {name !== "" && problem !== null && <p className="field-error">{problem}</p>}
      {saved !== null && <p className="ok">{saved}</p>}
      <ErrorBox error={set.error} />
    </section>
  );
}

/** `onChanged`: the settings were saved; re-read the sandbox (a running one restarts). */
export function SandboxSettingsPanel({ sandbox, onChanged }: { sandbox: Sandbox; onChanged: () => void }) {
  return (
    <div className="stack">
      <section aria-label="Settings">
        <h3>Settings</h3>
        <SettingsForm sandbox={sandbox} onChanged={onChanged} />
      </section>
      <EnvVars sandbox={sandbox.slug} />
    </div>
  );
}
