// "New project": a slug, checked as the user types, then straight to the project to chat with aho.
import { useState } from "react";
import { useNavigate } from "react-router";
import { api } from "../lib/api";
import { slugProblem } from "../lib/slug";
import { ErrorBox } from "../ui/common";
import { useAction } from "../ui/hooks";
import { Modal } from "../ui/overlay";

export function CreateProjectModal({ org, onClose }: { org: string; onClose: () => void }) {
  const navigate = useNavigate();
  const [slug, setSlug] = useState("");
  const problem = slugProblem(slug);
  const create = useAction(async () => {
    const project = await api.createProject(org, { slug });
    onClose();
    navigate(`/projects/${encodeURIComponent(project.slug)}`);
  });

  return (
    <Modal title="New project" onClose={onClose}>
      <form
        className="stack"
        onSubmit={(e) => {
          e.preventDefault();
          if (problem === null) void create.run();
        }}
      >
        <p className="hint">
          In <strong>{org}</strong>. You will chat with aho in the project, see its code, and deploy it to a preview.
        </p>
        <label>
          Project name
          <input
            value={slug}
            onChange={(e) => setSlug(e.target.value.toLowerCase())}
            placeholder="shop"
            autoComplete="off"
            spellCheck={false}
            aria-invalid={slug !== "" && problem !== null}
            aria-describedby="project-slug-rule"
          />
        </label>
        <p id="project-slug-rule" className={slug !== "" && problem !== null ? "field-error" : "hint"}>
          {slug !== "" && problem !== null ? problem : "1–40 lowercase letters, digits or -, starting with a letter."}
        </p>
        <div className="row end">
          <button type="button" className="secondary" onClick={onClose}>
            Cancel
          </button>
          <button type="submit" disabled={problem !== null || create.busy}>
            {create.busy ? "Creating…" : "Create project"}
          </button>
        </div>
        <ErrorBox error={create.error} />
      </form>
    </Modal>
  );
}
