// Confirm screen for a manifest draft: every machine-derived field shows its evidence; weak ones
// (inferred, observed) must be confirmed or corrected before the draft can be submitted.
import { useState } from "react";
import type { Draft, DraftField, Strength } from "../gen/contract";
import { byStrength, confirm, declare, needsConfirmation, STRENGTHS, unconfirmed } from "../lib/draft";
import { Badge } from "../ui/common";

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

export function DraftConfirm({ draft: initial, onSubmit, submitting = false, submitLabel = "Propose release" }: { draft: Draft; onSubmit: (draft: Draft) => void; submitting?: boolean; submitLabel?: string }) {
  const [draft, setDraft] = useState(initial);
  const pending = unconfirmed(draft);
  const groups = byStrength(draft);

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
      <div className="row">
        <button type="button" disabled={pending.length > 0 || submitting} onClick={() => onSubmit(draft)}>
          {submitting ? "Submitting…" : submitLabel}
        </button>
        {pending.length > 0 && <span className="muted">Confirm or correct the highlighted fields first.</span>}
      </div>
    </section>
  );
}
