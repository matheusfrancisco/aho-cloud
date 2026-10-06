// Onboarding wizard (server.md "UI"): connect → index progress → Learn run (live) → review/edit
// the proposed recipe (diff vs proposal) → Boot gate verify (live) → copy/download `.mage/env.toml`.
import { CopyButton, FileViewer, Loading, StepList, useAction, useLoad } from "@aho/ui";
import { useState } from "react";
import { Link, useParams, useSearchParams } from "react-router";
import type { OrgApi } from "../api/client";
import { MageError } from "../components/common";
import { LayerSummary } from "../components/Layers";
import { LiveRunPanel } from "../components/LiveRunPanel";
import { RECIPE_PATH, RecipeEditor } from "../components/Recipe";
import { useFeed, useMage } from "../context";
import type { HarnessKind, RepoDetail } from "../gen/mage";
import { HARNESS_LABEL, HARNESSES } from "../lib/labels";
import { Markdown } from "../lib/markdown";
import { headVersion, type WizardStepId, wizardSteps } from "../lib/onboarding";

const STEP_LABEL: Record<WizardStepId, string> = {
  connect: "Connect the repository",
  index: "Index the default branch",
  learn: "Learn how to run it",
  review: "Review the proposed recipe",
  verify: "Verify it boots (Boot gate)",
  export: "Add .mage/env.toml to the repository",
};

function StartLearn({ api, repo, retry, onStarted }: { api: OrgApi; repo: RepoDetail; retry: boolean; onStarted: () => void }) {
  const { can } = useMage();
  const settings = useLoad(() => api.settings(), [api]);
  const [harness, setHarness] = useState<HarnessKind | null>(null);
  const chosen = harness ?? settings.data?.general.harness ?? "omp";
  const head = headVersion(repo);
  const start = useAction(async () => {
    await api.learn(repo.repo.id, { harness: chosen, sha: head?.sha });
    onStarted();
  });
  if (!can("admin")) return <p className="muted">An org admin starts the Learn run.</p>;
  return (
    <div className="stack">
      <p className="hint">An agent explores the repository, writes a recipe that builds and starts the app, and proves it boots.</p>
      <div className="row">
        <select aria-label="Learn harness" value={chosen} onChange={(e) => setHarness(HARNESSES.find((h) => h === e.target.value) ?? chosen)}>
          {HARNESSES.map((h) => (
            <option key={h} value={h}>
              {HARNESS_LABEL[h]}
            </option>
          ))}
        </select>
        <button type="button" onClick={() => void start.run()} disabled={start.busy}>
          {start.busy ? "Starting…" : retry ? "Run Learn again" : "Start Learn run"}
        </button>
      </div>
      <MageError error={start.error} />
    </div>
  );
}

function Export({ toml }: { toml: string }) {
  return (
    <div className="stack">
      <p>
        Commit this file as <code>{RECIPE_PATH}</code> at the repository root so the recipe travels with the code.
      </p>
      <div className="row">
        <CopyButton text={toml} label="Copy recipe" />
        <a href={`data:application/toml;charset=utf-8,${encodeURIComponent(toml)}`} download="env.toml">
          Download env.toml
        </a>
      </div>
      <FileViewer file={{ path: RECIPE_PATH, size: new TextEncoder().encode(toml).length, text: toml, truncated: false }} />
    </div>
  );
}

function Wizard({ id }: { id: string }) {
  const { api, link, can } = useMage();
  const [params, setParams] = useSearchParams();
  const reviewed = params.get("reviewed");
  const verifyRun = params.get("verify");
  const repo = useLoad(() => api.repo(id), [api, id]);
  const learnId = repo.data?.last_learn?.id ?? null;
  const learnStatus = repo.data?.last_learn?.status ?? null;
  const learn = useLoad(() => (learnId === null ? Promise.resolve(null) : api.run(learnId)), [api, learnId, learnStatus]);
  const [verifyStatusTick, setVerifyStatusTick] = useState(0);
  const verify = useLoad(() => (verifyRun === null ? Promise.resolve(null) : api.run(verifyRun)), [api, verifyRun, verifyStatusTick]);
  useFeed((e) => e.kind === "repo" || e.kind === "recipe" || e.kind === "run", repo.reload);

  const update = (changes: Record<string, string | null>) => {
    const next = new URLSearchParams(params);
    for (const [name, value] of Object.entries(changes)) {
      if (value === null) next.delete(name);
      else next.set(name, value);
    }
    setParams(next, { replace: true });
  };

  const detail = repo.data;
  if (detail === undefined) return repo.loading ? <Loading what="the repo" /> : <MageError error={repo.error} />;
  const result = learn.data?.run.result;
  const proposal = result?.kind === "learn" ? result : null;
  const wizard = wizardSteps({ repo: detail, proposal: proposal?.toml ?? null, reviewed, verifyStatus: verify.data?.run.status ?? null });
  const step = wizard.steps.find((s) => s.id === wizard.current)!;
  const recipe = detail.recipe;

  return (
    <div className="mage-page">
      <header className="mage-head">
        <h1>
          Onboard {detail.repo.owner}/{detail.repo.name}
        </h1>
        <span className="grow" />
        <Link to={link(`repos/${detail.repo.id}`)}>Repo details</Link>
      </header>
      <MageError error={repo.error} />
      <div className="run-split">
        <section className="card" aria-label="Steps">
          <StepList steps={wizard.steps.map((s) => ({ id: s.id, label: STEP_LABEL[s.id], state: s.state, detail: s.detail }))} />
        </section>
        <section className="card stack" aria-label={STEP_LABEL[wizard.current]}>
          <h2>{STEP_LABEL[wizard.current]}</h2>
          {wizard.current === "connect" && (
            <p className="error">
              The GitHub app can no longer read this repository. Grant it access again in <Link to={link("settings/integrations")}>Integrations</Link>.
            </p>
          )}
          {wizard.current === "index" && (
            <>
              {step.state === "failed" ? <p className="error">Indexing failed: {step.detail}</p> : <p className="hint">Mage indexes the default branch head before an agent explores it. This page updates as layers finish.</p>}
              <LayerSummary version={headVersion(detail)} />
            </>
          )}
          {wizard.current === "learn" && (
            <>
              {detail.last_learn !== null && <LiveRunPanel key={detail.last_learn.id} run={detail.last_learn.id} />}
              {(step.state === "pending" || step.state === "failed") && <StartLearn api={api} repo={detail} retry={step.state === "failed"} onStarted={repo.reload} />}
            </>
          )}
          {wizard.current === "review" &&
            (can("admin") ? (
              <>
                {proposal !== null && proposal.notes !== "" && (
                  <details>
                    <summary>Agent notes</summary>
                    <Markdown source={proposal.notes} />
                  </details>
                )}
                <RecipeEditor
                  api={api}
                  repo={detail.repo.id}
                  initial={recipe?.body ?? proposal?.toml ?? ""}
                  proposal={proposal?.toml ?? null}
                  saveLabel="Save and continue"
                  onSaved={(saved) => {
                    update({ verify: null, reviewed: saved.recipe.hash });
                    repo.reload();
                  }}
                />
              </>
            ) : (
              <p className="muted">An org admin reviews the recipe.</p>
            ))}
          {wizard.current === "verify" && reviewed !== null && (
            <VerifyStep
              api={api}
              repo={detail}
              hash={reviewed}
              run={verifyRun}
              failed={step.state === "failed" ? (step.detail ?? "failed") : null}
              onStarted={(run) => update({ verify: run })}
              onSettled={() => {
                setVerifyStatusTick((t) => t + 1);
                repo.reload();
              }}
              onEdit={() => update({ reviewed: null, verify: null })}
            />
          )}
          {wizard.current === "export" && recipe !== null && <Export toml={recipe.body} />}
        </section>
      </div>
    </div>
  );
}

function VerifyStep({ api, repo, hash, run, failed, onStarted, onSettled, onEdit }: { api: OrgApi; repo: RepoDetail; hash: string; run: string | null; failed: string | null; onStarted: (run: string) => void; onSettled: () => void; onEdit: () => void }) {
  const { can } = useMage();
  const start = useAction(async () => {
    const started = await api.verifyRecipe(repo.repo.id, { hash, sha: headVersion(repo)?.sha });
    onStarted(started.run_id);
  });
  return (
    <div className="stack">
      <p className="hint">A fresh environment builds the app from this recipe and checks every service comes up healthy.</p>
      {run !== null && <LiveRunPanel key={run} run={run} onStatus={onSettled} />}
      {failed !== null && <p className="error">The Boot gate did not pass: {failed}</p>}
      {can("admin") && (run === null || failed !== null) && (
        <div className="row">
          <button type="button" onClick={() => void start.run()} disabled={start.busy}>
            {start.busy ? "Starting…" : run === null ? "Run Boot gate" : "Run Boot gate again"}
          </button>
          <button type="button" className="secondary" onClick={onEdit}>
            Edit the recipe
          </button>
        </div>
      )}
      <MageError error={start.error} />
    </div>
  );
}

export function OnboardPage() {
  const id = useParams().repo ?? "";
  return <Wizard key={id} id={id} />;
}
