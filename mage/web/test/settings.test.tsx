import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import type { ModelKeyMeta, ModelSettingsView } from "../src/gen/mage";
import { apiError, fakeServer, renderMage } from "./helpers";

const KEY = "sk-ant-api03-SECRET-never-shown-0123456789";
const MODEL = "/api/v1/orgs/acme/settings/model";

function setup(role: "owner" | "admin", initialKey: ModelKeyMeta | null, putKey: () => ModelKeyMeta | Response) {
  let key = initialKey;
  const server = fakeServer({
    [`GET ${MODEL}`]: (): ModelSettingsView => ({ route: "anthropic", allowed_models: ["claude-sonnet-4-5"], max_tokens_cap: null, vertex_available: false, key }),
    [`PUT ${MODEL}/key`]: () => {
      const result = putKey();
      if (!(result instanceof Response)) key = result;
      return result;
    },
    [`DELETE ${MODEL}/key`]: () => {
      key = null;
      return undefined;
    },
  });
  renderMage("/o/acme/settings/model", { fetch: server.fetch, role, csrfToken: "csrf-K" });
  return server;
}

const meta = (fingerprint: string): ModelKeyMeta => ({ fingerprint, set_at: "2026-10-01T12:00:00Z", set_by: { type: "user", id: "usr_ana" } });

describe("BYOK key", () => {
  it("shows only the stored key's fingerprint and who set it, never the key", async () => {
    setup("owner", meta("sk-ant-…a1b2"), () => meta("x"));
    const section = await screen.findByRole("region", { name: "Anthropic API key" });
    expect(within(section).getByText("sk-ant-…a1b2")).toBeInTheDocument();
    expect(section).toHaveTextContent("by user usr_ana");
    const input = within(section).getByLabelText("Replacement API key");
    expect(input).toHaveAttribute("type", "password");
    expect(input).toHaveAttribute("autocomplete", "new-password");
    expect(input).toHaveValue("");
  });

  it("sends a new key write-only and forgets it: the form clears and the page shows the new fingerprint", async () => {
    const user = userEvent.setup();
    const server = setup("owner", null, () => meta("sk-ant-…6789"));
    const section = await screen.findByRole("region", { name: "Anthropic API key" });
    await user.type(within(section).getByLabelText("New API key"), KEY);
    await user.click(within(section).getByRole("button", { name: "Set key" }));
    expect(await within(section).findByText("sk-ant-…6789")).toBeInTheDocument();
    const [put] = server.find("PUT", `${MODEL}/key`);
    expect(put!.body).toEqual({ api_key: KEY });
    expect(put!.headers["x-csrf-token"]).toBe("csrf-K");
    expect(within(section).getByLabelText("Replacement API key")).toHaveValue("");
    expect(document.body.innerHTML).not.toContain("SECRET");
  });

  it("a rejected key is not kept in the form either", async () => {
    const user = userEvent.setup();
    setup("owner", null, () => apiError(422, "invalid", "key rejected by Anthropic", "req-byok"));
    const section = await screen.findByRole("region", { name: "Anthropic API key" });
    await user.type(within(section).getByLabelText("New API key"), KEY);
    await user.click(within(section).getByRole("button", { name: "Set key" }));
    expect(await within(section).findByRole("alert")).toHaveTextContent("key rejected by Anthropic");
    expect(section).toHaveTextContent("Request id req-byok");
    expect(within(section).getByLabelText("New API key")).toHaveValue("");
    expect(document.body.innerHTML).not.toContain("SECRET");
  });

  it("removes the key after confirmation", async () => {
    const user = userEvent.setup();
    const server = setup("owner", meta("sk-ant-…a1b2"), () => meta("x"));
    const section = await screen.findByRole("region", { name: "Anthropic API key" });
    await user.click(within(section).getByRole("button", { name: "Remove key…" }));
    await user.click(within(section).getByRole("button", { name: "Remove key" }));
    expect(await within(section).findByText(/No key set/)).toBeInTheDocument();
    expect(server.find("DELETE", `${MODEL}/key`)).toHaveLength(1);
  });

  it("admins see the fingerprint but cannot set or remove the key", async () => {
    setup("admin", meta("sk-ant-…a1b2"), () => meta("x"));
    const section = await screen.findByRole("region", { name: "Anthropic API key" });
    expect(within(section).getByText("sk-ant-…a1b2")).toBeInTheDocument();
    expect(within(section).queryByLabelText("Replacement API key")).toBeNull();
    expect(within(section).queryByRole("button", { name: "Remove key…" })).toBeNull();
  });
});

describe("members", () => {
  it("lets an admin invite up to their own role, not above", async () => {
    const user = userEvent.setup();
    const server = fakeServer({
      "GET /api/v1/orgs/acme/members": () => ({ members: [{ user: { id: "usr_ana", email: "ana@example.com", name: "Ana" }, role: "admin" }], invites: [] }),
      "POST /api/v1/orgs/acme/members/invites": () => ({ id: "inv_1", login: "bo", role: "admin", invited_by: "usr_ana", created_at: "2026-10-01T00:00:00Z", expires_at: "2026-10-15T00:00:00Z" }),
    });
    renderMage("/o/acme/settings/members", { fetch: server.fetch, role: "admin" });
    const form = await screen.findByRole("form", { name: "Invite" });
    const roles = within(form).getByRole("combobox", { name: "Invited role" });
    expect(within(roles).getAllByRole("option").map((o) => o.textContent)).toEqual(["Member", "Admin"]);
    await user.type(within(form).getByRole("textbox", { name: "GitHub login" }), "bo");
    await user.selectOptions(roles, "Admin");
    await user.click(within(form).getByRole("button", { name: "Invite" }));
    await waitFor(() => expect(server.find("POST", "/api/v1/orgs/acme/members/invites")).toHaveLength(1));
    expect(server.find("POST", "/api/v1/orgs/acme/members/invites")[0]!.body).toEqual({ github_login: "bo", role: "admin" });
  });
});
