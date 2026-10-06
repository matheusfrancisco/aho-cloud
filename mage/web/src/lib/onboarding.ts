// Onboarding wizard state (server.md "UI": 1 connect → 2 index progress → 3 Learn run → 4 review/edit
// the proposed recipe → 5 Boot gate verify → 6 copy/download `.mage/env.toml`). Derived from server
// state plus two choices kept in the URL, so a reload resumes where the user was.
import type { StepState } from "@aho/ui";
import type { Recipe, RepoDetail, RunStatus, VersionView } from "../gen/mage";
import { TERMINAL } from "./labels";

export type WizardStepId = "connect" | "index" | "learn" | "review" | "verify" | "export";

export interface WizardInput {
  repo: RepoDetail;
  /** The proposal of the latest Learn run, once it succeeded. */
  proposal: string | null;
  /** Recipe hash the user accepted in step 4 (`?reviewed=`). */
  reviewed: string | null;
  /** Status of the Boot gate Verify run started in step 5 (`?verify=`), when known. */
  verifyStatus: RunStatus | null;
}

export interface WizardStep {
  id: WizardStepId;
  state: StepState;
  detail?: string;
}

export interface Wizard {
  steps: WizardStep[];
  /** The step the user acts on: the first one not done or skipped (`export` once all are). */
  current: WizardStepId;
}

/** The default-branch head version (else the newest), whose layers step 2 waits for. */
export function headVersion(repo: RepoDetail): VersionView | null {
  return repo.versions.find((v) => v.reasons.some((r) => r.type === "default_head")) ?? repo.versions[0] ?? null;
}

/**
 * The Learn run needs the head's source (`files`); the other layers keep indexing in the background
 * and are only reported.
 */
function indexState(head: VersionView | null): Pick<WizardStep, "state" | "detail"> {
  const files = head?.layers.files;
  if (head === null || files === undefined) return { state: "running", detail: "waiting for the first index" };
  if (files.type === "failed" || files.type === "skipped") return { state: "failed", detail: `files: ${files.reason}` };
  if (files.type !== "ready") return { state: "running", detail: "indexing files" };
  const busy = Object.values(head.layers).filter((s) => s.type === "pending" || s.type === "running").length;
  return busy === 0 ? { state: "done" } : { state: "done", detail: `${busy} more layer${busy === 1 ? "" : "s"} still indexing` };
}

function learnState(repo: RepoDetail): Pick<WizardStep, "state" | "detail"> {
  const learn = repo.last_learn;
  if (learn === null) return repo.recipe !== null ? { state: "skipped" } : { state: "pending" };
  if (!TERMINAL[learn.status]) return { state: "running" };
  return learn.status === "succeeded" ? { state: "done" } : { state: "failed", detail: learn.status.replaceAll("_", " ") };
}

function verifyState(recipe: Recipe | null, reviewed: string | null, verifyStatus: RunStatus | null): Pick<WizardStep, "state" | "detail"> {
  if (reviewed === null) return { state: "pending" };
  const current = recipe !== null && recipe.hash === reviewed ? recipe : null;
  if (current?.status.type === "validated") return { state: "done" };
  if (verifyStatus !== null && !TERMINAL[verifyStatus]) return { state: "running" };
  if (current?.status.type === "broken") return { state: "failed", detail: current.status.reason };
  if (verifyStatus !== null && verifyStatus !== "succeeded") return { state: "failed", detail: verifyStatus.replaceAll("_", " ") };
  return { state: "pending" };
}

export function wizardSteps(input: WizardInput): Wizard {
  const { repo, proposal, reviewed, verifyStatus } = input;
  const steps: WizardStep[] = [];
  const push = (id: WizardStepId, s: Pick<WizardStep, "state" | "detail">, after: boolean) => steps.push({ id, ...(after ? s : { state: "pending" }) });
  const passed = (s: WizardStep | undefined) => s !== undefined && (s.state === "done" || s.state === "skipped");

  push("connect", repo.repo.access === "granted" ? { state: "done" } : { state: "failed", detail: "the GitHub app lost access" }, true);
  push("index", indexState(headVersion(repo)), passed(steps[0]));
  push("learn", learnState(repo), passed(steps[1]));
  const reviewable = passed(steps[2]) && (proposal !== null || repo.recipe !== null);
  push("review", reviewed !== null ? { state: "done" } : { state: "pending" }, reviewable);
  push("verify", verifyState(repo.recipe, reviewed, verifyStatus), passed(steps[3]));
  push("export", { state: "done" }, passed(steps[4]));

  const current = steps.find((s) => !passed(s))?.id ?? "export";
  return { steps, current };
}
