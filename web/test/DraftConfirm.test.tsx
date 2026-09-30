import { fireEvent, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { DraftConfirm } from "../src/components/DraftConfirm";
import type { Draft } from "../src/gen/contract";
import { draftToValue, unconfirmed } from "../src/lib/draft";

const draft: Draft = {
  fields: {
    "/processes/0/port": { value: 3000, strength: "inferred", evidence: [{ uri: "file:Dockerfile:12", note: "EXPOSE 3000" }], confirmed: false },
    "/egress": { value: ["api.stripe.com"], strength: "observed", evidence: [{ uri: "net:flow-7", note: "TLS to api.stripe.com:443" }], confirmed: false },
    "/build": { value: { kind: "dockerfile", path: "Dockerfile", context: "." }, strength: "chained", evidence: [{ uri: "aho-log:5f3a#c0ffee", note: "docker build ." }], confirmed: false },
    "/project": { value: "shop", strength: "declared", evidence: [], confirmed: true },
    // Human decisions already made, so these tests are about confirmation only.
    "/processes/0/name": { value: "web", strength: "declared", evidence: [], confirmed: true },
    "/processes/0/health": { value: { kind: "http", path: "/healthz", port: null }, strength: "declared", evidence: [], confirmed: true },
    "/auth": { value: { mode: "none" }, strength: "declared", evidence: [], confirmed: true },
    "/data_class": { value: "none", strength: "declared", evidence: [], confirmed: true },
    "/owner": { value: { name: "Ana", email: "ana@acme.com", escalation: [] }, strength: "declared", evidence: [], confirmed: true },
    "/budget": { value: { ceiling_usd_month: 20, degrade: "sleep" }, strength: "declared", evidence: [], confirmed: true },
  },
};

const submit = () => screen.getByRole("button", { name: "Propose release" });

describe("draft confirm screen", () => {
  it("groups fields by strength, weakest first, with their evidence", () => {
    const { container } = render(<DraftConfirm draft={draft} onSubmit={() => {}} />);
    const groups = [...container.querySelectorAll(".strength-group")].map((g) => g.className.replace("strength-group strength-", ""));
    expect(groups).toEqual(["inferred", "observed", "chained", "declared"]);
    const port = container.querySelector('[data-pointer="/processes/0/port"]') as HTMLElement;
    expect(within(port).getByText("file:Dockerfile:12")).toBeInTheDocument();
    expect(within(port).getByText("needs confirmation")).toBeInTheDocument();
    // Chained evidence is strong enough: no confirmation asked.
    const build = container.querySelector('[data-pointer="/build"]') as HTMLElement;
    expect(within(build).queryByText("needs confirmation")).toBeNull();
    expect(within(build).queryByRole("button", { name: /^Confirm/ })).toBeNull();
  });

  it("keeps submit disabled until every inferred/observed field is confirmed or corrected", async () => {
    const user = userEvent.setup();
    const onSubmit = vi.fn();
    render(<DraftConfirm draft={draft} onSubmit={onSubmit} />);
    expect(screen.getByText("2 fields still need your confirmation.")).toBeInTheDocument();
    expect(submit()).toBeDisabled();

    await user.click(screen.getByRole("button", { name: "Confirm /egress" }));
    expect(screen.getByText("1 field still needs your confirmation.")).toBeInTheDocument();
    expect(submit()).toBeDisabled();
    await user.click(submit());
    expect(onSubmit).not.toHaveBeenCalled();

    // Correct the port instead of confirming it; invalid JSON is refused and changes nothing.
    await user.click(screen.getByRole("button", { name: "Correct /processes/0/port" }));
    const box = screen.getByLabelText(/Value \(JSON\) for \/processes\/0\/port/);
    fireEvent.change(box, { target: { value: "80a" } });
    await user.click(screen.getByRole("button", { name: "Save" }));
    expect(screen.getByText(/Not JSON/)).toBeInTheDocument();
    expect(submit()).toBeDisabled();
    fireEvent.change(box, { target: { value: "8080" } });
    await user.click(screen.getByRole("button", { name: "Save" }));

    expect(submit()).toBeEnabled();
    await user.click(submit());
    expect(onSubmit).toHaveBeenCalledTimes(1);
    const sent = onSubmit.mock.calls[0]![0] as Draft;
    expect(unconfirmed(sent)).toEqual([]);
    expect(sent.fields["/processes/0/port"]).toEqual({ value: 8080, strength: "declared", evidence: draft.fields["/processes/0/port"]!.evidence, confirmed: true });
    expect(sent.fields["/egress"]).toMatchObject({ strength: "observed", confirmed: true });
  });
});

describe("fields nothing in the code decides", () => {
  const partial: Draft = {
    fields: {
      "/project": { value: "shop", strength: "declared", evidence: [], confirmed: true },
      "/processes/0/name": { value: "web", strength: "declared", evidence: [], confirmed: true },
      "/processes/0/port": { value: 8080, strength: "declared", evidence: [], confirmed: true },
      "/processes/1/name": { value: "worker", strength: "declared", evidence: [], confirmed: true },
    },
  };

  it("asks for them, blocks submit until answered, and declares the answers", async () => {
    const user = userEvent.setup();
    const onSubmit = vi.fn();
    render(<DraftConfirm draft={partial} onSubmit={onSubmit} />);
    const required = screen.getByLabelText("required fields");
    // Signed out in this test: no owner to prefill.
    expect(submit()).toBeDisabled();
    expect(screen.getByText("Give the owner's name and email.")).toBeInTheDocument();
    await user.type(within(required).getByLabelText("Owner name"), "Ana");
    await user.type(within(required).getByLabelText("Owner email (who gets called when it breaks)"), "ana@acme.com");
    // The worker has no port, so its health check is a command the human must give.
    expect(screen.getByText("Say how to check that worker is healthy.")).toBeInTheDocument();
    await user.type(within(required).getByLabelText("Health command for worker (exit 0 = healthy)"), "test -f /tmp/ready");
    await user.selectOptions(within(required).getByLabelText("Who can open the app"), "github");
    expect(submit()).toBeEnabled();

    await user.click(submit());
    const value = draftToValue(onSubmit.mock.calls[0]![0] as Draft);
    expect(value).toMatchObject({
      api_version: "deploy.aho.cloud/v1",
      auth: { mode: "oidc", provider: { kind: "github" } },
      data_class: "none",
      owner: { name: "Ana", email: "ana@acme.com" },
      budget: { ceiling_usd_month: 20, degrade: "sleep" },
      processes: [
        { name: "web", health: { kind: "http", path: "/" } },
        { name: "worker", health: { kind: "command", argv: ["test", "-f", "/tmp/ready"] } },
      ],
    });
  });

  it("cannot deploy a draft without any process", () => {
    render(<DraftConfirm draft={{ fields: { "/project": partial.fields["/project"]! } }} onSubmit={() => {}} />);
    expect(submit()).toBeDisabled();
    expect(screen.getAllByText(/No process was found/).length).toBeGreaterThan(0);
  });
});

describe("draftToValue", () => {
  it("assembles pointers into nested objects and arrays like Draft::to_value", () => {
    const value = draftToValue({
      fields: {
        "/processes/1/name": { value: "worker", strength: "declared", evidence: [], confirmed: true },
        "/processes/0/name": { value: "web", strength: "declared", evidence: [], confirmed: true },
        "/processes/0/port": { value: 8080, strength: "declared", evidence: [], confirmed: true },
        "/a~1b/c~0d": { value: true, strength: "declared", evidence: [], confirmed: true },
      },
    });
    expect(value).toEqual({ processes: [{ name: "web", port: 8080 }, { name: "worker" }], "a/b": { "c~d": true } });
  });

  it("rejects conflicting pointers instead of overwriting", () => {
    expect(() =>
      draftToValue({
        fields: {
          "/owner": { value: "ana", strength: "declared", evidence: [], confirmed: true },
          "/owner/email": { value: "ana@example.com", strength: "declared", evidence: [], confirmed: true },
        },
      }),
    ).toThrow(/conflicts/);
    expect(() => draftToValue({ fields: { owner: { value: 1, strength: "declared", evidence: [], confirmed: true } } })).toThrow(/not a JSON pointer/);
  });
});
