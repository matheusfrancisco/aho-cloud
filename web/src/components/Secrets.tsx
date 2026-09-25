// Secrets are write-only: values go to the control plane and never come back. The list shows
// names, stages, and the keyed fingerprint the taint scan matches on.
import { useState } from "react";
import type { SecretView } from "../gen/api";
import type { Stage } from "../gen/contract";
import { api } from "../lib/api";
import { STAGES } from "../lib/release";
import { ErrorBox, Hash } from "../ui/common";
import { useAction } from "../ui/hooks";

export function SecretForm({ project, name: fixedName, onSaved }: { project: string; name?: string; onSaved?: (s: SecretView) => void }) {
  const [name, setName] = useState(fixedName ?? "");
  const [stage, setStage] = useState<Stage>("live");
  const [value, setValue] = useState("");
  const [saved, setSaved] = useState<string | null>(null);
  const save = useAction(async () => {
    const view = await api.setSecret(project, { name: name.trim(), stage, value });
    setValue("");
    setSaved(`${view.name} set for ${stage}.`);
    onSaved?.(view);
  });

  return (
    <form
      className="secret-form row"
      autoComplete="off"
      onSubmit={(e) => {
        e.preventDefault();
        setSaved(null);
        void save.run();
      }}
    >
      <label>
        Name
        <input value={name} onChange={(e) => setName(e.target.value.toUpperCase())} pattern="[A-Z][A-Z0-9_]*" placeholder="STRIPE_KEY" readOnly={fixedName !== undefined} required />
      </label>
      <label>
        Stage
        <select value={stage} onChange={(e) => setStage(e.target.value as Stage)}>
          {STAGES.map((s) => (
            <option key={s} value={s}>
              {s}
            </option>
          ))}
        </select>
      </label>
      <label>
        Value
        <input type="password" value={value} onChange={(e) => setValue(e.target.value)} autoComplete="new-password" spellCheck={false} required />
      </label>
      <button type="submit" disabled={save.busy || name.trim() === "" || value === ""}>
        {fixedName === undefined ? "Set secret" : `Set new ${fixedName}`}
      </button>
      {saved !== null && <span className="ok">{saved}</span>}
      <ErrorBox error={save.error} />
    </form>
  );
}

/** Secrets of a project (names and fingerprints only); `onChanged` reloads them after a set. */
export function SecretsPanel({ project, secrets, onChanged }: { project: string; secrets: SecretView[]; onChanged: () => void }) {
  return (
    <div>
      {secrets.length === 0 ? (
        <p className="muted">No secrets set.</p>
      ) : (
        <table>
          <thead>
            <tr>
              <th>Name</th>
              <th>Stages</th>
              <th>Fingerprint</th>
            </tr>
          </thead>
          <tbody>
            {secrets.map((s) => (
              <tr key={s.name}>
                <td>
                  <code>{s.name}</code>
                </td>
                <td>{s.stages.join(", ")}</td>
                <td>
                  <Hash value={s.fingerprint} />
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      <SecretForm project={project} onSaved={onChanged} />
    </div>
  );
}
