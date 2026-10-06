// Confirm screen for a manifest draft: every machine-derived field shows its evidence; weak ones
// (inferred, observed) must be confirmed or corrected before the draft can be submitted.
import { useState } from "react";
import { Badge } from "@aho/ui";
import type { Auth, DataClass, Draft, DraftField, Health, Strength } from "../gen/contract";
import { byStrength, complete, confirm, declare, type Missing, missing, needsConfirmation, STRENGTHS, unconfirmed } from "../lib/draft";
import { useSession } from "../lib/session";

const STRENGTH_TEXT: Record<Strength, { title: string; hint: string }> = {
  inferred: { title: "Inferred", hint: "From a static scan of files. Check each value." },
  observed: { title: "Observed", hint: "Seen at runtime, not hash-chained. Check each value." },
  chained: { title: "Chained", hint: "From hash-chained session log entries." },
  declared: { title: "Declared", hint: "Typed by a human." },
};

function FieldRow({ pointer, field, onConfirm, onCorrect }: { pointer: string; field: DraftField; onConfirm: () => void; onCorrect: (value: unknown) => void }) {
  const [editing, setEditing] = useState(false);
  const [text, setText] = useState("");
  const [parseError, setParseError] = useState<string | null>(null);
  const pending = needsConfirmation(field.strength) && !field.confirmed;

  const save = () => {
    try {
      onCorrect(JSON.parse(text));
      setEditing(false);
      setParseError(null);
    } catch (e) {
      setParseError(`Not JSON: ${e instanceof Error ? e.message : String(e)}`);
    }
  };

  return (
    <li className={`draft-field${pending ? " pending" : ""}`} data-pointer={pointer}>
      <div className="row">
        <code className="pointer">{pointer}</code>
        {pending ? <Badge tone="warn">needs confirmation</Badge> : field.confirmed && <Badge tone="ok">confirmed</Badge>}
      </div>
      {editing ? (
        <div className="correct">
          <label>
            Value (JSON) for {pointer}
            <textarea value={text} onChange={(e) => setText(e.target.value)} rows={Math.min(12, Math.max(2, text.split("\n").length))} spellCheck={false} />
          </label>
          {parseError !== null && <p className="error">{parseError}</p>}
          <div className="row">
            <button type="button" onClick={save}>
              Save
            </button>
            <button type="button" className="secondary" onClick={() => setEditing(false)}>
              Cancel
            </button>
          </div>
        </div>
      ) : (
        <pre className="value">{JSON.stringify(field.value, null, 2)}</pre>
      )}
      {field.evidence.length > 0 && (
        <details open={pending}>
          <summary>Evidence ({field.evidence.length})</summary>
          <ul className="evidence">
            {field.evidence.map((ev, i) => (
              <li key={`${ev.uri}:${i}`}>
                <code>{ev.uri}</code> {ev.note}
              </li>
            ))}
          </ul>
        </details>
      )}
      {!editing && (
        <div className="row">
          {pending && (
            <button type="button" onClick={onConfirm}>
              Confirm {pointer}
            </button>
          )}
          <button
            type="button"
            className="secondary"
            onClick={() => {
              setText(JSON.stringify(field.value, null, 2));
              setEditing(true);
            }}
          >
            Correct {pointer}
          </button>
        </div>
      )}
    </li>
  );
}

const AUTH_CHOICES: Array<{ key: string; label: string; auth: Auth }> = [
  { key: "magic_link", label: "Email sign-in link (works locally)", auth: { mode: "magic_link", allowed_domains: [] } },
  { key: "none", label: "None: the app is public", auth: { mode: "none" } },
  { key: "password", label: "One shared password", auth: { mode: "password" } },
  { key: "google", label: "Google", auth: { mode: "oidc", provider: { kind: "google" } } },
  { key: "github", label: "GitHub", auth: { mode: "oidc", provider: { kind: "github" } } },
  { key: "microsoft", label: "Microsoft", auth: { mode: "oidc", provider: { kind: "microsoft" } } },
  { key: "app_owned", label: "The app signs users in itself", auth: { mode: "app_owned" } },
];

interface Answers {
  auth: string;
  data_class: DataClass;
  region: string;
  ownerName: string;
  ownerEmail: string;
  ceiling: string;
  /** Health path (port) or command (no port) per process index. */
  health: Record<number, string>;
}

/** Why the answers cannot be used yet, or `null`. */
function answersProblem(need: Missing, a: Answers): string | null {
  if (need.noProcesses) return "No process was found. Ask aho to add a Dockerfile (with EXPOSE) or a Procfile.";
  if (need.owner && (a.ownerName.trim() === "" || !a.ownerEmail.includes("@"))) return "Give the owner's name and email.";
  if (need.budget && !(Number(a.ceiling) >= 1)) return "The monthly ceiling must be at least $1.";
  if (need.data_class && a.data_class === "pii" && a.region.trim() === "") return "Personal data needs a region.";
  for (const h of need.health) {
    if ((a.health[h.index] ?? (h.port === null ? "" : "/")).trim() === "") return `Say how to check that ${h.name} is healthy.`;
  }
  return null;
}

function RequiredFields({ need, answers, onChange }: { need: Missing; answers: Answers; onChange: (a: Answers) => void }) {
  const set = (patch: Partial<Answers>) => onChange({ ...answers, ...patch });
  return (
    <div className="required-fields" aria-label="required fields">
      <h3>Needed to deploy</h3>
      <p className="hint">Nothing in the code decides these; choose them.</p>
      {need.noProcesses && <p className="error">No process was found. Ask aho to add a Dockerfile (with EXPOSE) or a Procfile.</p>}
      {need.auth && (
        <label>
          Who can open the app
          <select value={answers.auth} onChange={(e) => set({ auth: e.target.value })}>
            {AUTH_CHOICES.map((c) => (
              <option key={c.key} value={c.key}>
                {c.label}
              </option>
            ))}
          </select>
        </label>
      )}
      {need.health.map((h) => (
        <label key={h.index}>
          {h.port === null ? `Health command for ${h.name} (exit 0 = healthy)` : `Health check path for ${h.name} (:${h.port})`}
          <input
            value={answers.health[h.index] ?? (h.port === null ? "" : "/")}
            placeholder={h.port === null ? "e.g. test -f /tmp/ready" : "/"}
            onChange={(e) => set({ health: { ...answers.health, [h.index]: e.target.value } })}
          />
        </label>
      ))}
      {need.data_class && (
        <label>
          Data it stores
          <select value={answers.data_class} onChange={(e) => set({ data_class: e.target.value as DataClass })}>
            <option value="none">No personal data</option>
            <option value="pii">Personal data (needs a region)</option>
          </select>
        </label>
      )}
      {need.data_class && answers.data_class === "pii" && (
        <label>
          Region
          <input value={answers.region} placeholder="eu-west" onChange={(e) => set({ region: e.target.value })} />
        </label>
      )}
      {need.owner && (
        <>
          <label>
            Owner name
            <input value={answers.ownerName} onChange={(e) => set({ ownerName: e.target.value })} />
          </label>
          <label>
            Owner email (who gets called when it breaks)
            <input type="email" value={answers.ownerEmail} onChange={(e) => set({ ownerEmail: e.target.value })} />
          </label>
        </>
      )}
      {need.budget && (
        <label>
          Monthly ceiling in USD (the app sleeps when reached)
          <input type="number" min={1} value={answers.ceiling} onChange={(e) => set({ ceiling: e.target.value })} />
        </label>
      )}
    </div>
  );
}

function withAnswers(draft: Draft, need: Missing, a: Answers): Draft {
  const health: Record<number, Health> = {};
  for (const h of need.health) {
    const text = (a.health[h.index] ?? (h.port === null ? "" : "/")).trim();
    health[h.index] = h.port === null ? { kind: "command", argv: text.split(/\s+/) } : { kind: "http", path: text.startsWith("/") ? text : `/${text}`, port: null };
  }
  const auth = AUTH_CHOICES.find((c) => c.key === a.auth)?.auth ?? { mode: "none" };
  return complete(draft, need, {
    auth,
    data_class: a.data_class,
    region: a.region.trim(),
    owner: { name: a.ownerName.trim(), email: a.ownerEmail.trim() },
    ceiling_usd_month: Math.floor(Number(a.ceiling)),
    health,
  });
}

export function DraftConfirm({ draft: initial, onSubmit, submitting = false, submitLabel = "Propose release" }: { draft: Draft; onSubmit: (draft: Draft) => void; submitting?: boolean; submitLabel?: string }) {
  const session = useSession();
  const [draft, setDraft] = useState(initial);
  const [answers, setAnswers] = useState<Answers>({
    auth: "magic_link",
    data_class: "none",
    region: "",
    ownerName: session?.user.name ?? "",
    ownerEmail: session?.user.email ?? "",
    ceiling: "20",
    health: {},
  });
  const pending = unconfirmed(draft);
  const groups = byStrength(draft);
  const need = missing(draft);
  const needsAnswers = need.auth || need.data_class || need.owner || need.budget || need.health.length > 0 || need.noProcesses;
  const problem = needsAnswers ? answersProblem(need, answers) : null;

  return (
    <section className="card draft-confirm" aria-label="draft confirmation">
      <h2>Confirm the manifest draft</h2>
      <p>{pending.length === 0 ? "Every field is confirmed." : `${pending.length} field${pending.length === 1 ? "" : "s"} still need${pending.length === 1 ? "s" : ""} your confirmation.`}</p>
      {STRENGTHS.map((s) =>
        groups[s].length === 0 ? null : (
          <div key={s} className={`strength-group strength-${s}`}>
            <h3>
              {STRENGTH_TEXT[s].title} <span className="count">({groups[s].length})</span>
            </h3>
            <p className="hint">{STRENGTH_TEXT[s].hint}</p>
            <ul>
              {groups[s].map(([pointer, field]) => (
                <FieldRow key={pointer} pointer={pointer} field={field} onConfirm={() => setDraft((d) => confirm(d, pointer))} onCorrect={(v) => setDraft((d) => declare(d, pointer, v))} />
              ))}
            </ul>
          </div>
        ),
      )}
      {needsAnswers && <RequiredFields need={need} answers={answers} onChange={setAnswers} />}
      <div className="row">
        <button
          type="button"
          disabled={pending.length > 0 || problem !== null || submitting}
          onClick={() => onSubmit(needsAnswers ? withAnswers(draft, need, answers) : draft)}
        >
          {submitting ? "Submitting…" : submitLabel}
        </button>
        {pending.length > 0 ? <span className="muted">Confirm or correct the highlighted fields first.</span> : problem !== null && <span className="muted">{problem}</span>}
      </div>
    </section>
  );
}
