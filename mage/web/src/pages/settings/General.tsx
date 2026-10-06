// Settings → General: harness default, run budgets, triage policy and fix settings (`OrgSettings`).
// Members read them; Admins save them.
import { Loading, useAction, useLoad } from "@aho/ui";
import { type ReactNode, useState } from "react";
import { MageError } from "../../components/common";
import { useFeed, useMage } from "../../context";
import type { BranchFrom, BriefInLinear, Budget, FixSettings, LockfilePolicy, OpenPrMode, OrgSettings, RegressionMode, TriageSettings } from "../../gen/mage";
import { HARNESS_LABEL, HARNESSES } from "../../lib/labels";

function NumberField({ label, value, onChange, step = 1, min = 0, hint }: { label: string; value: number; onChange: (v: number) => void; step?: number; min?: number; hint?: ReactNode }) {
  return (
    <label>
      {label}
      <input
        type="number"
        min={min}
        step={step}
        value={Number.isFinite(value) ? value : ""}
        onChange={(e) => {
          const v = e.target.valueAsNumber;
          if (Number.isFinite(v)) onChange(v);
        }}
      />
      {hint !== undefined && <span className="hint">{hint}</span>}
    </label>
  );
}

function Check({ label, checked, onChange, hint }: { label: string; checked: boolean; onChange: (v: boolean) => void; hint?: ReactNode }) {
  return (
    <label className="choice">
      <input type="checkbox" checked={checked} onChange={(e) => onChange(e.target.checked)} />
      <span>
        {label}
        {hint !== undefined && <span className="hint"> — {hint}</span>}
      </span>
    </label>
  );
}

function Choice<T extends string>({ label, value, options, onChange }: { label: string; value: T; options: ReadonlyArray<readonly [T, string]>; onChange: (v: T) => void }) {
  return (
    <label>
      {label}
      <select value={value} onChange={(e) => onChange(options.find(([v]) => v === e.target.value)?.[0] ?? value)}>
        {options.map(([v, text]) => (
          <option key={v} value={v}>
            {text}
          </option>
        ))}
      </select>
    </label>
  );
}

const HARNESS_OPTIONS = HARNESSES.map((h) => [h, HARNESS_LABEL[h]] as const);

/** Budget in the units people think in: minutes, turns, dollars, attempts. */
function BudgetFields({ title, budget, onChange }: { title: string; budget: Budget; onChange: (b: Budget) => void }) {
  return (
    <fieldset className="row">
      <legend>{title}</legend>
      <NumberField label="Wall time (minutes)" value={budget.max_wall / 60_000} step={1} onChange={(v) => onChange({ ...budget, max_wall: Math.round(v * 60_000) })} />
      <NumberField label="Turns" value={budget.max_turns} onChange={(v) => onChange({ ...budget, max_turns: Math.round(v) })} />
      <NumberField label="Cost (USD)" value={budget.max_cost_micros / 1_000_000} step={0.01} onChange={(v) => onChange({ ...budget, max_cost_micros: Math.round(v * 1_000_000) })} />
      <NumberField label="Gate attempts" value={budget.max_gate_attempts} min={1} onChange={(v) => onChange({ ...budget, max_gate_attempts: Math.round(v) })} />
    </fieldset>
  );
}

/** One entry per line or comma. */
function listText(items: readonly string[]): string {
  return items.join(", ");
}
function parseList(text: string): string[] {
  return text
    .split(/[\n,]/)
    .map((s) => s.trim())
    .filter((s) => s !== "");
}

function TriageFields({ triage, set }: { triage: TriageSettings; set: (t: TriageSettings) => void }) {
  return (
    <fieldset className="stack">
      <legend>Triage</legend>
      <Check label="Triage new and changed tickets automatically" checked={triage.auto_triage} onChange={(auto_triage) => set({ ...triage, auto_triage })} />
      <div className="row">
        <Choice label="Harness" value={triage.harness} options={HARNESS_OPTIONS} onChange={(harness) => set({ ...triage, harness })} />
        <label>
          Model
          <input value={triage.model ?? ""} placeholder="server default" onChange={(e) => set({ ...triage, model: e.target.value.trim() === "" ? null : e.target.value.trim() })} />
        </label>
        <NumberField label="Causes at most" value={triage.max_causes} min={1} onChange={(v) => set({ ...triage, max_causes: Math.round(v) })} />
      </div>
      <BudgetFields title="Triage budget" budget={triage.budget} onChange={(budget) => set({ ...triage, budget })} />
      <div className="row">
        <NumberField label="Wait for index layers (seconds)" value={triage.layer_wait_ms / 1000} onChange={(v) => set({ ...triage, layer_wait_ms: Math.round(v * 1000) })} />
        <NumberField label="Automatic retriages per ticket per day" value={triage.auto_retriage_per_day} onChange={(v) => set({ ...triage, auto_retriage_per_day: Math.round(v) })} />
        <NumberField label="Triage cost cap per ticket (USD)" value={triage.ticket_cost_cap_micros / 1_000_000} step={0.01} onChange={(v) => set({ ...triage, ticket_cost_cap_micros: Math.round(v * 1_000_000) })} />
      </div>
    </fieldset>
  );
}

const BRIEF_IN_LINEAR: ReadonlyArray<readonly [BriefInLinear, string]> = [
  ["off", "Off"],
  ["summary", "Summary"],
  ["full", "Full brief"],
];
const OPEN_PR: ReadonlyArray<readonly [OpenPrMode, string]> = [
  ["manual", "When someone clicks Open PR"],
  ["auto", "Automatically when a fix passes"],
];
const BRANCH_FROM: ReadonlyArray<readonly [BranchFrom, string]> = [
  ["default_head", "Current default-branch head"],
  ["gated_sha", "The commit the fix was gated at"],
];
const LOCKFILES: ReadonlyArray<readonly [LockfilePolicy, string]> = [
  ["exclude", "Exclude"],
  ["with_manifest", "Only with their manifest"],
  ["include", "Include"],
];
const REGRESSION: ReadonlyArray<readonly [RegressionMode, string]> = [
  ["off", "Off"],
  ["mage_dir", ".mage/regressions/"],
  ["repo_tests", "The repository's tests"],
];

function FixFields({ fix, set }: { fix: FixSettings; set: (f: FixSettings) => void }) {
  return (
    <fieldset className="stack">
      <legend>Fixes</legend>
      <Check label="Fix briefs" hint="a prompt for your own coding agent after a reproduced diagnosis" checked={fix.brief} onChange={(brief) => set({ ...fix, brief })} />
      <Choice label="Fix brief in Linear" value={fix.brief_in_linear} options={BRIEF_IN_LINEAR} onChange={(brief_in_linear) => set({ ...fix, brief_in_linear })} />
      <Check label="Mage fixes" hint="an agent writes the patch; the fix gate checks it" checked={fix.run} onChange={(run) => set({ ...fix, run })} />
      <Check label="Pull requests" hint="needs the GitHub fix app" checked={fix.pull_requests} onChange={(pull_requests) => set({ ...fix, pull_requests })} />
      <div className="row">
        <Choice label="Open the PR" value={fix.open_pr} options={OPEN_PR} onChange={(open_pr) => set({ ...fix, open_pr })} />
        <Choice label="PR base" value={fix.pr.branch_from} options={BRANCH_FROM} onChange={(branch_from) => set({ ...fix, pr: { ...fix.pr, branch_from } })} />
        <Check label="Draft PRs" checked={fix.pr.draft} onChange={(draft) => set({ ...fix, pr: { ...fix.pr, draft } })} />
      </div>
      <div className="row">
        <label>
          Labels
          <input value={listText(fix.pr.labels)} onChange={(e) => set({ ...fix, pr: { ...fix.pr, labels: parseList(e.target.value) } })} placeholder="bug, needs-review" />
        </label>
        <label>
          Reviewers (GitHub logins)
          <input value={listText(fix.pr.reviewers.users)} onChange={(e) => set({ ...fix, pr: { ...fix.pr, reviewers: { ...fix.pr.reviewers, users: parseList(e.target.value) } } })} />
        </label>
        <label>
          Reviewer teams
          <input value={listText(fix.pr.reviewers.teams)} onChange={(e) => set({ ...fix, pr: { ...fix.pr, reviewers: { ...fix.pr.reviewers, teams: parseList(e.target.value) } } })} />
        </label>
      </div>
      <div className="row">
        <Choice label="Lockfile changes" value={fix.lockfiles} options={LOCKFILES} onChange={(lockfiles) => set({ ...fix, lockfiles })} />
        <Choice label="Regression test" value={fix.regression} options={REGRESSION} onChange={(regression) => set({ ...fix, regression })} />
        <NumberField label="Fix gate repetitions" value={fix.repetitions} min={1} onChange={(v) => set({ ...fix, repetitions: Math.round(v) })} />
      </div>
      <Check label="Allow workflow edits" hint=".github/workflows and .github/actions" checked={fix.allow_workflow_edits} onChange={(allow_workflow_edits) => set({ ...fix, allow_workflow_edits })} />
      <div className="row">
        <NumberField label="Patch size (KiB)" value={fix.limits.max_patch_bytes / 1024} onChange={(v) => set({ ...fix, limits: { ...fix.limits, max_patch_bytes: Math.round(v * 1024) } })} />
        <NumberField label="Files" value={fix.limits.max_files} onChange={(v) => set({ ...fix, limits: { ...fix.limits, max_files: Math.round(v) } })} />
        <NumberField label="Changed lines" value={fix.limits.max_changed_lines} onChange={(v) => set({ ...fix, limits: { ...fix.limits, max_changed_lines: Math.round(v) } })} />
        <NumberField label="File size (KiB)" value={fix.limits.max_file_bytes / 1024} onChange={(v) => set({ ...fix, limits: { ...fix.limits, max_file_bytes: Math.round(v * 1024) } })} />
      </div>
    </fieldset>
  );
}

function SettingsForm({ initial }: { initial: OrgSettings }) {
  const { api, can } = useMage();
  const [draft, setDraft] = useState(initial);
  const [saved, setSaved] = useState(false);
  const edit = (next: OrgSettings) => {
    setDraft(next);
    setSaved(false);
  };
  const save = useAction(async () => {
    const stored = await api.saveSettings(draft);
    setDraft(stored);
    setSaved(true);
  });
  const g = draft.general;
  return (
    <form
      className="stack"
      onSubmit={(e) => {
        e.preventDefault();
        void save.run();
      }}
    >
      {!can("admin") && <p className="hint">Only admins change these settings.</p>}
      <fieldset className="stack" disabled={!can("admin")}>
        <fieldset className="stack">
          <legend>Defaults</legend>
          <Choice label="Harness for Learn, Diagnose and Fix runs" value={g.harness} options={HARNESS_OPTIONS} onChange={(harness) => edit({ ...draft, general: { ...g, harness } })} />
          <BudgetFields title="Learn budget" budget={g.budgets.learn} onChange={(learn) => edit({ ...draft, general: { ...g, budgets: { ...g.budgets, learn } } })} />
          <BudgetFields title="Diagnose budget" budget={g.budgets.diagnose} onChange={(diagnose) => edit({ ...draft, general: { ...g, budgets: { ...g.budgets, diagnose } } })} />
          <BudgetFields title="Fix budget" budget={g.budgets.fix} onChange={(fix) => edit({ ...draft, general: { ...g, budgets: { ...g.budgets, fix } } })} />
        </fieldset>
        <TriageFields triage={draft.triage} set={(triage) => edit({ ...draft, triage })} />
        <FixFields fix={draft.fix} set={(fix) => edit({ ...draft, fix })} />
      </fieldset>
      {can("admin") && (
        <div className="row">
          <button type="submit" disabled={save.busy}>
            {save.busy ? "Saving…" : "Save settings"}
          </button>
          {saved && <span className="ok">Saved.</span>}
        </div>
      )}
      <MageError error={save.error} />
    </form>
  );
}

export function GeneralSettings() {
  const { api } = useMage();
  const loaded = useLoad(() => api.settings(), [api]);
  useFeed((e) => e.kind === "settings", loaded.reload);
  if (loaded.data === undefined) return loaded.loading ? <Loading what="settings" /> : <MageError error={loaded.error} />;
  // Settings changed elsewhere (feed) reset the form; our own save comes back unchanged and keeps it.
  return <SettingsForm key={JSON.stringify(loaded.data)} initial={loaded.data} />;
}
