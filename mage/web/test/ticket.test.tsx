import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import type { Cause, TicketDetail } from "../src/gen/mage";
import { cause, repo, runSummary, SHA, SHA2, settings, ticketDetail } from "./fixtures";
import { fakeServer, renderMage } from "./helpers";

const TICKET = "/api/v1/orgs/acme/tickets/tkt_1";
const CAUSE = `${TICKET}/causes/cau_1`;

function setup(detail: TicketDetail, opts: { run?: boolean; pull_requests?: boolean; path?: string } = {}) {
  const server = fakeServer({
    [`GET ${TICKET}`]: () => detail,
    "GET /api/v1/orgs/acme/settings": () => settings(opts),
    "GET /api/v1/orgs/acme/repos": () => ({ repos: [{ repo: repo(), head: null, recipe: null, last_learn: null }, { repo: repo({ id: "rep_api", name: "api" }), head: null, recipe: null, last_learn: null }] }),
    [`POST ${CAUSE}/diagnose`]: () => ({ run_id: "run_diag" }),
    [`POST ${CAUSE}/fix`]: () => ({ run_id: "run_fix" }),
    [`POST ${CAUSE}/pull-request`]: () => ({ job_id: "job_pr" }),
    [`GET ${CAUSE}/fix-brief`]: (c) => ({ markdown: `# Fix brief v${c.query.get("version") ?? 2}\n\nChange \`submit\`.`, version: Number(c.query.get("version") ?? 2), latest_version: 2, diagnose_run: "run_diag", created_at: "2026-10-01T12:00:00Z", artifact_url: "/api/v1/orgs/acme/artifacts/art_brief/content" }),
    [`PUT ${TICKET}/target`]: (c) => ({ ...(c.body as object), origin: { type: "override" } }),
    [`POST ${TICKET}/resolve`]: () => ({ ticket_id: "tkt_1", org_id: "org_acme", confirmed_cause: "cau_1", root_cause: "x", fix_pr_url: null, repro_script: null, resolved_by: "usr_ana", at: "2026-10-02T00:00:00Z" }),
    [`POST ${TICKET}/retriage`]: () => ({ job_id: "job_tr" }),
  });
  renderMage(opts.path ?? "/o/acme/tickets/tkt_1", { fetch: server.fetch, csrfToken: "csrf-T" });
  return server;
}

const location = () => screen.getByTestId("location").textContent;
const causeCard = (rank = 1) => screen.findByRole("article", { name: new RegExp(`^Cause ${rank}:`) });

describe("ticket detail", () => {
  it("shows Linear and Slack links, the sanitized body and ranked causes with GitHub permalinks", async () => {
    setup(ticketDetail({ ticket: { ...ticketDetail().ticket, slack_thread: { team: "T1", channel: "C9", ts: "1700000000.000100" } } }, [cause({ id: "cau_2", rank: 2, title: "Stale cache" }), cause()]));
    const card = await causeCard(1);
    expect(screen.getByRole("link", { name: "Open in Linear" })).toHaveAttribute("href", "https://linear.app/acme/issue/ENG-412");
    expect(screen.getByRole("link", { name: "Slack thread" })).toHaveAttribute("href", "https://app.slack.com/client/T1/C9/thread/C9-1700000000.000100");
    const details = screen.getByRole("region", { name: "Details" });
    expect(within(details).getByText("Pay").tagName).toBe("STRONG");
    expect(details.querySelector("script")).toBeNull();
    expect(details).not.toHaveTextContent("alert(1)");
    // Ranked: #1 before #2 regardless of the order the server listed them.
    expect(screen.getAllByRole("article").map((a) => a.getAttribute("aria-label"))).toEqual(["Cause 1: Missing await in submit handler", "Cause 2: Stale cache"]);
    expect(within(card).getByRole("link", { name: "src/checkout/submit button.ts:10-14" })).toHaveAttribute("href", `https://github.com/acme/web/blob/${SHA}/src/checkout/submit%20button.ts#L10-L14`);
  });

  it("Run diagnostic posts the picked harness with the CSRF token and opens the run", async () => {
    const user = userEvent.setup();
    const server = setup(ticketDetail());
    const card = await causeCard();
    await user.selectOptions(within(card).getByRole("combobox", { name: "Diagnostic harness" }), "Claude Code");
    await user.click(within(card).getByRole("button", { name: "Run diagnostic" }));
    await waitFor(() => expect(location()).toBe("/o/acme/runs/run_diag"));
    const [post] = server.find("POST", `${CAUSE}/diagnose`);
    expect(post!.body).toEqual({ harness: "claude_code" });
    expect(post!.headers["x-csrf-token"]).toBe("csrf-T");
  });

  it("offers no diagnostic for a cause that is already running and links its run instead", async () => {
    setup(ticketDetail({ ticket: { ...ticketDetail().ticket, status: "diagnosing" } }, [cause({ status: { type: "running", data: "run_9" } })]));
    const card = await causeCard();
    expect(within(card).queryByRole("button", { name: "Run diagnostic" })).toBeNull();
    expect(within(card).getByRole("link", { name: "Diagnostic running" })).toHaveAttribute("href", "/o/acme/runs/run_9");
  });

  const reproduced: Cause = cause({ status: { type: "reproduced", data: { run: "run_diag", gate: "gat_1" } } });
  const diagnosed = (over: Partial<TicketDetail["ticket"]> = {}) => ticketDetail({ ticket: { ...ticketDetail().ticket, status: "diagnosed", ...over } }, [reproduced]);

  it("fix brief tab: renders the brief, picks versions, offers Copy and the .md download", async () => {
    const user = userEvent.setup();
    const server = setup(diagnosed());
    const card = await causeCard();
    await user.click(within(card).getByRole("tab", { name: "Fix brief" }));
    expect(await within(card).findByRole("heading", { name: "Fix brief v2" })).toBeInTheDocument();
    expect(within(card).getByRole("button", { name: "Copy brief" })).toBeInTheDocument();
    expect(within(card).getByRole("link", { name: "Download .md" })).toHaveAttribute("href", "/api/v1/orgs/acme/artifacts/art_brief/content");
    await user.selectOptions(within(card).getByRole("combobox", { name: "Version" }), "v1");
    expect(await within(card).findByRole("heading", { name: "Fix brief v1" })).toBeInTheDocument();
    expect(server.find("GET", `${CAUSE}/fix-brief`).map((c) => c.query.get("version"))).toEqual([null, "1"]);
    // Fix runs are off for this org: no Let Mage fix.
    expect(within(card).queryByRole("button", { name: "Let Mage fix" })).toBeNull();
  });

  it("a short link to a cause's brief opens that cause on its brief tab", async () => {
    setup(diagnosed(), { path: "/o/acme/tickets/tkt_1?cause=cau_1&tab=brief" });
    const card = await causeCard();
    expect(await within(card).findByRole("heading", { name: "Fix brief v2" })).toBeInTheDocument();
    expect(within(card).getByRole("tab", { name: "Fix brief" })).toHaveAttribute("aria-selected", "true");
  });

  it("Let Mage fix posts to the fix route when fix runs are enabled and opens the run", async () => {
    const user = userEvent.setup();
    const server = setup(diagnosed(), { run: true });
    const card = await causeCard();
    await user.click(within(card).getByRole("tab", { name: "Fix brief" }));
    await user.click(await within(card).findByRole("button", { name: "Let Mage fix" }));
    await waitFor(() => expect(location()).toBe("/o/acme/runs/run_fix"));
    expect(server.find("POST", `${CAUSE}/fix`)[0]!.body).toEqual({ harness: "omp" });
  });

  it("Open PR enqueues the pull request for a fix-ready cause when PRs are enabled", async () => {
    const user = userEvent.setup();
    const fixRun = runSummary({ id: "run_fix", kind: "fix", status: "succeeded" });
    const server = setup(ticketDetail({ ticket: { ...ticketDetail().ticket, status: "fix_ready" }, runs: [fixRun] }, [reproduced]), { run: true, pull_requests: true });
    const card = await causeCard();
    await user.click(within(card).getByRole("button", { name: "Open PR" }));
    expect(await within(card).findByText(/Requested; the pull request appears here/)).toBeInTheDocument();
    const [post] = server.find("POST", `${CAUSE}/pull-request`);
    expect(post!.body).toBeUndefined();
    expect(post!.headers["x-csrf-token"]).toBe("csrf-T");
    expect(within(card).getByRole("button", { name: "Open PR" })).toBeDisabled();
  });

  it("no Open PR before the fix is ready or when PRs are off", async () => {
    setup(diagnosed(), { run: true, pull_requests: true });
    const card = await causeCard();
    expect(within(card).queryByRole("button", { name: "Open PR" })).toBeNull();
  });

  it("overrides the target with a repo and a full sha", async () => {
    const user = userEvent.setup();
    const server = setup(ticketDetail());
    await user.click(await screen.findByRole("button", { name: "Change target" }));
    const form = screen.getByRole("form", { name: "Change target" });
    await user.selectOptions(within(form).getByRole("combobox", { name: "Repo" }), await within(form).findByRole("option", { name: "acme/api" }));
    await user.type(within(form).getByRole("textbox", { name: "Commit sha" }), SHA2.slice(0, 12));
    expect(within(form).getByRole("button", { name: "Set target" })).toBeDisabled();
    await user.type(within(form).getByRole("textbox", { name: "Commit sha" }), SHA2.slice(12).toUpperCase());
    await user.click(within(form).getByRole("button", { name: "Set target" }));
    await waitFor(() => expect(server.find("PUT", `${TICKET}/target`)).toHaveLength(1));
    expect(server.find("PUT", `${TICKET}/target`)[0]!.body).toEqual({ repo_id: "rep_api", sha: SHA2 });
  });

  it("resolves with the confirmed cause, root cause and PR URL", async () => {
    const user = userEvent.setup();
    const server = setup(diagnosed());
    await user.click(await screen.findByRole("button", { name: "Resolve…" }));
    const form = screen.getByRole("form", { name: "Resolve" });
    expect(within(form).getByRole("combobox", { name: "Confirmed cause" })).toHaveValue("cau_1");
    await user.type(within(form).getByRole("textbox", { name: "Root cause" }), "submit handler did not await");
    await user.type(within(form).getByRole("textbox", { name: "Fix PR URL" }), "https://github.com/acme/web/pull/7");
    await user.click(within(form).getByRole("button", { name: "Resolve" }));
    await waitFor(() => expect(server.find("POST", `${TICKET}/resolve`)).toHaveLength(1));
    expect(server.find("POST", `${TICKET}/resolve`)[0]!.body).toEqual({ confirmed_cause: "cau_1", root_cause: "submit handler did not await", fix_pr_url: "https://github.com/acme/web/pull/7" });
  });

  it("retriage posts to the retriage route", async () => {
    const user = userEvent.setup();
    const server = setup(ticketDetail());
    await user.click(await screen.findByRole("button", { name: "Retriage" }));
    await waitFor(() => expect(server.find("POST", `${TICKET}/retriage`)).toHaveLength(1));
  });
});
