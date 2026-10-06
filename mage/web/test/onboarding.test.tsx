import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import type { Recipe, RepoDetail, RunStatus, SaveRecipeRequest } from "../src/gen/mage";
import { type WizardInput, wizardSteps } from "../src/lib/onboarding";
import { READY, repo, repoDetail, runDetail, runSummary, SHA, version } from "./fixtures";
import { fakeServer, renderMage } from "./helpers";

const PROPOSAL = 'version = 1\n[services.web]\nrun = "npm start"\n';

function recipe(hash: string, status: Recipe["status"], body = PROPOSAL): Recipe {
  return { org_id: "org_acme", repo_id: "rep_web", hash, body, validated_at_sha: status.type === "validated" ? SHA : null, status };
}

const learn = (status: RunStatus) => runSummary({ id: "run_learn", kind: "learn", status, subject: { type: "repo_version", data: { repo_id: "rep_web", sha: SHA } } });
const indexed = version({ files: READY, symbols: READY, history: { type: "skipped", reason: "shallow clone" } });

function input(repo: Partial<RepoDetail>, over: Partial<WizardInput> = {}): WizardInput {
  return { repo: repoDetail(repo), proposal: null, reviewed: null, verifyStatus: null, ...over };
}

const states = (i: WizardInput) => wizardSteps(i).steps.map((s) => `${s.id}:${s.state}`);

describe("onboarding steps", () => {
  it("walks connect → index → learn → review → verify → export as server state and choices advance", () => {
    // Connected, nothing indexed yet.
    let w = wizardSteps(input({}));
    expect(w.current).toBe("index");
    expect(states(input({}))).toEqual(["connect:done", "index:running", "learn:pending", "review:pending", "verify:pending", "export:pending"]);

    // Learn needs the source: step 2 stays open until `files` is ready; other layers only report.
    expect(wizardSteps(input({ versions: [version({ files: { type: "running" }, symbols: { type: "pending" } })] })).current).toBe("index");
    const partly = wizardSteps(input({ versions: [version({ files: READY, symbols: { type: "running" }, app_map: { type: "pending" } })] }));
    expect([partly.current, partly.steps[1]!.state, partly.steps[1]!.detail]).toEqual(["learn", "done", "2 more layers still indexing"]);

    // Indexed (skipped layers count as finished): Learn is next.
    w = wizardSteps(input({ versions: [indexed] }));
    expect(w.current).toBe("learn");
    expect(w.steps[2]!.state).toBe("pending");

    // Learn running, then succeeded with a proposal: review.
    expect(wizardSteps(input({ versions: [indexed], last_learn: learn("running") })).steps[2]!.state).toBe("running");
    const learned = { versions: [indexed], last_learn: learn("succeeded"), recipe: recipe("h1", { type: "draft" }) };
    expect(wizardSteps(input(learned, { proposal: PROPOSAL })).current).toBe("review");

    // Reviewed (saved as hash h2): verify, then the Boot gate run, then validated → export.
    expect(wizardSteps(input({ ...learned, recipe: recipe("h2", { type: "draft" }) }, { proposal: PROPOSAL, reviewed: "h2" })).current).toBe("verify");
    expect(states(input({ ...learned, recipe: recipe("h2", { type: "draft" }) }, { proposal: PROPOSAL, reviewed: "h2", verifyStatus: "running" }))[4]).toBe("verify:running");
    w = wizardSteps(input({ ...learned, recipe: recipe("h2", { type: "validated", at: "2026-10-01T13:00:00Z" }) }, { proposal: PROPOSAL, reviewed: "h2", verifyStatus: "succeeded" }));
    expect(w.current).toBe("export");
    expect(w.steps.every((s) => s.state === "done")).toBe(true);
  });

  it("reports failures on the step they belong to and stops there", () => {
    const lost = wizardSteps(input({ repo: repo({ access: "lost" }), versions: [indexed] }));
    expect([lost.current, lost.steps[0]!.state, lost.steps[1]!.state]).toEqual(["connect", "failed", "pending"]);

    const badIndex = wizardSteps(input({ versions: [version({ files: { type: "failed", reason: "clone failed" } })] }));
    expect([badIndex.current, badIndex.steps[1]!.detail]).toEqual(["index", "files: clone failed"]);

    const badLearn = wizardSteps(input({ versions: [indexed], last_learn: learn("timed_out") }));
    expect([badLearn.current, badLearn.steps[2]!.state, badLearn.steps[2]!.detail]).toEqual(["learn", "failed", "timed out"]);

    const learned = { versions: [indexed], last_learn: learn("succeeded") };
    const broken = wizardSteps(input({ ...learned, recipe: recipe("h2", { type: "broken", reason: "web never healthy", at: "2026-10-01T13:00:00Z" }) }, { proposal: PROPOSAL, reviewed: "h2", verifyStatus: "failed" }));
    expect([broken.current, broken.steps[4]!.state, broken.steps[4]!.detail]).toEqual(["verify", "failed", "web never healthy"]);
  });

  it("skips Learn for a repo that already has a recipe and reviews that one", () => {
    const w = wizardSteps(input({ versions: [indexed], recipe: recipe("h1", { type: "validated", at: "2026-10-01T13:00:00Z" }) }));
    expect(w.steps[2]!.state).toBe("skipped");
    expect(w.current).toBe("review");
    // Accepting the validated recipe as is finishes the wizard.
    expect(wizardSteps(input({ versions: [indexed], recipe: recipe("h1", { type: "validated", at: "2026-10-01T13:00:00Z" }) }, { reviewed: "h1" })).current).toBe("export");
  });
});

describe("onboarding wizard", () => {
  it("review → save → Boot gate → export, with the diff against the proposal", async () => {
    const user = userEvent.setup();
    let current = recipe("h1", { type: "draft" });
    let verifyStatus: RunStatus = "running";
    const server = fakeServer({
      "GET /api/v1/orgs/acme/repos/rep_web": () => repoDetail({ versions: [indexed], last_learn: learn("succeeded"), recipe: current }),
      "GET /api/v1/orgs/acme/runs/run_learn": () => runDetail({ id: "run_learn", kind: "learn", status: "succeeded", result: { kind: "learn", toml: PROPOSAL, notes: "Starts **web** only." } }),
      "PUT /api/v1/orgs/acme/repos/rep_web/recipe": (c) => {
        // What the client sent, typed by `OrgApi.saveRecipe`.
        const sent = c.body as SaveRecipeRequest;
        current = recipe("h2", { type: "draft" }, sent.toml);
        return { recipe: current, report: { errors: [], warnings: [{ path: "services.web", code: "timeouts", message: "no start timeout" }] } };
      },
      "POST /api/v1/orgs/acme/repos/rep_web/recipe/verify": () => ({ run_id: "run_verify" }),
      "GET /api/v1/orgs/acme/runs/run_verify": () => runDetail({ id: "run_verify", kind: "verify", harness: null, status: verifyStatus, subject: { type: "run", data: "run_learn" } }),
      "GET /api/v1/orgs/acme/runs/run_verify/events": () => ({ events: [], more: false }),
      "GET /api/v1/orgs/acme/runs/run_learn/events": () => ({ events: [], more: false }),
    });
    const view = renderMage("/o/acme/repos/rep_web/onboard", { fetch: server.fetch });

    const review = await screen.findByRole("region", { name: "Review the proposed recipe" });
    const editor = within(review).getByRole("textbox", { name: ".mage/env.toml" });
    expect(editor).toHaveValue(PROPOSAL);
    expect(within(review).queryByLabelText("Changes from the proposal")).toBeNull();
    await user.clear(editor);
    await user.paste('version = 1\n[services.web]\nrun = "npm run serve"\n');
    const diff = within(review).getByLabelText("Changes from the proposal");
    expect([...diff.querySelectorAll(".del")].map((d) => d.textContent)).toEqual(['-run = "npm start"']);
    expect([...diff.querySelectorAll(".add")].map((d) => d.textContent)).toEqual(['+run = "npm run serve"']);
    await user.click(within(review).getByRole("button", { name: "Save and continue" }));

    const verify = await screen.findByRole("region", { name: "Verify it boots (Boot gate)" });
    expect(screen.getByTestId("location")).toHaveTextContent("/o/acme/repos/rep_web/onboard?reviewed=h2");
    await user.click(within(verify).getByRole("button", { name: "Run Boot gate" }));
    await waitFor(() => expect(server.find("POST", "/api/v1/orgs/acme/repos/rep_web/recipe/verify")).toHaveLength(1));
    expect(server.find("POST", "/api/v1/orgs/acme/repos/rep_web/recipe/verify")[0]!.body).toEqual({ hash: "h2", sha: SHA });
    expect(await within(verify).findByRole("region", { name: "Verify run" })).toBeInTheDocument();
    expect(within(verify).queryByRole("button", { name: "Run Boot gate" })).toBeNull();

    // The gate passes: the recipe is validated and the wizard ends on the export step.
    verifyStatus = "succeeded";
    current = { ...current, status: { type: "validated", at: "2026-10-01T13:00:00Z" } };
    view.unmount();
    renderMage("/o/acme/repos/rep_web/onboard?reviewed=h2&verify=run_verify", { fetch: server.fetch });
    const done = await screen.findByRole("region", { name: "Add .mage/env.toml to the repository" });
    expect(within(done).getByRole("button", { name: "Copy recipe" })).toBeInTheDocument();
    expect(within(done).getByRole("link", { name: "Download env.toml" })).toHaveAttribute("href", `data:application/toml;charset=utf-8,${encodeURIComponent(current.body)}`);
  });

  it("starts the Learn run with the picked harness at the indexed head", async () => {
    const user = userEvent.setup();
    let last: ReturnType<typeof learn> | null = null;
    const server = fakeServer({
      "GET /api/v1/orgs/acme/repos/rep_web": () => repoDetail({ versions: [indexed], last_learn: last }),
      "GET /api/v1/orgs/acme/settings": () => ({ general: { harness: "omp" } }),
      "POST /api/v1/orgs/acme/repos/rep_web/learn": () => {
        last = learn("queued");
        return { run_id: "run_learn" };
      },
      "GET /api/v1/orgs/acme/runs/run_learn": () => runDetail({ id: "run_learn", kind: "learn", status: "queued" }),
      "GET /api/v1/orgs/acme/runs/run_learn/events": () => ({ events: [], more: false }),
    });
    renderMage("/o/acme/repos/rep_web/onboard", { fetch: server.fetch });
    const step = await screen.findByRole("region", { name: "Learn how to run it" });
    await user.selectOptions(within(step).getByRole("combobox", { name: "Learn harness" }), "Claude Code");
    await user.click(within(step).getByRole("button", { name: "Start Learn run" }));
    expect(await within(step).findByRole("region", { name: "Learn run" })).toBeInTheDocument();
    expect(server.find("POST", "/api/v1/orgs/acme/repos/rep_web/learn")[0]!.body).toEqual({ harness: "claude_code", sha: SHA });
  });
});
