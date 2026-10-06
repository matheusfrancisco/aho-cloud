// "New sandbox": a slug (one namespace with projects) and the sandbox's settings, prefilled with
// the defaults, then straight to the sandbox while it boots.
import { useState } from "react";
import { useNavigate } from "react-router";
import { ErrorBox, Modal, useAction } from "@aho/ui";
import { api } from "../lib/api";
import { checkSettings, settingsDraft, type SettingsProblems } from "../lib/sandbox";
import { slugProblem } from "../lib/slug";
import { SettingsFields } from "./SandboxSettings";

export function CreateSandboxModal({ org, onClose }: { org: string; onClose: () => void }) {
  const navigate = useNavigate();
  const [slug, setSlug] = useState("");
  const [draft, setDraft] = useState(() => settingsDraft());
  const [problems, setProblems] = useState<SettingsProblems | null>(null);
  const problem = slugProblem(slug);
  const create = useAction(async () => {
    const checked = checkSettings(draft);
    setProblems(checked.problems);
    if (checked.settings === null) return;
    const sandbox = await api.createSandbox(org, { slug, settings: checked.settings });
    onClose();
    navigate(`/sandboxes/${encodeURIComponent(sandbox.slug)}`);
  });

  return (
    <Modal title="New sandbox" onClose={onClose}>
      <form
        className="stack"
        onSubmit={(e) => {
          e.preventDefault();
          if (problem === null) void create.run();
        }}
      >
        <p className="hint">
          In <strong>{org}</strong>. A sandbox is a VM with aho, for coding without deploying: a terminal, a chat with the aho inside, and a disk at <code>/workspace</code> that survives stops.
        </p>
        <label>
          Sandbox name
          <input
            value={slug}
            onChange={(e) => setSlug(e.target.value.toLowerCase())}
            placeholder="scratch"
            autoComplete="off"
            spellCheck={false}
            aria-invalid={slug !== "" && problem !== null}
            aria-describedby="sandbox-slug-rule"
          />
        </label>
        <p id="sandbox-slug-rule" className={slug !== "" && problem !== null ? "field-error" : "hint"}>
          {slug !== "" && problem !== null ? problem : "1–40 lowercase letters, digits or -, starting with a letter. Projects and sandboxes share names."}
        </p>
        <SettingsFields draft={draft} onChange={setDraft} problems={problems} />
        <div className="row end">
          <button type="button" className="secondary" onClick={onClose}>
            Cancel
          </button>
          <button type="submit" disabled={problem !== null || create.busy}>
            {create.busy ? "Creating…" : "Create sandbox"}
          </button>
        </div>
        <ErrorBox error={create.error} />
      </form>
    </Modal>
  );
}
