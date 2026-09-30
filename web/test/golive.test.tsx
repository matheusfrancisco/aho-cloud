import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router";
import { afterEach, beforeEach, describe, expect, it, type Mock, vi } from "vitest";
import { GoLiveModal } from "../src/components/GoLive";
import type { ApprovalView, DomainView, Project, Release } from "../src/gen/api";
import type { Stage } from "../src/gen/contract";
import { api, ApiError } from "../src/lib/api";
import { approvalChallenge, approvalMessage } from "../src/lib/approval";
import { ensureDeclared, ensureDomain, promoteToLive } from "../src/lib/golive";
import { signIn } from "../src/lib/session";
import { manifest, release } from "./fixtures";

const ana = { id: "u-ana", email: "ana@example.com", name: "Ana" };
const bytes = (...b: number[]) => new Uint8Array(b).buffer;
const u8 = (b: BufferSource) => [...(ArrayBuffer.isView(b) ? new Uint8Array(b.buffer, b.byteOffset, b.byteLength) : new Uint8Array(b))];

const verified: DomainView = { hostname: "shop.example.com", mode: "cname", state: "verified", records: [{ kind: "CNAME", name: "shop.example.com", value: "edge.aho.cloud" }] };

/** Every API call and passkey assertion, in order. */
let calls: string[];
let get: Mock;

beforeEach(() => {
  calls = [];
  signIn({ token: "t", user: ana });
  get = vi.fn(async () => {
    calls.push("passkey");
    return { id: "AQID", rawId: bytes(1, 2, 3), type: "public-key", response: { clientDataJSON: bytes(1), authenticatorData: bytes(2), signature: bytes(3), userHandle: null } };
  });
  vi.stubGlobal("PublicKeyCredential", class {});
  Object.defineProperty(navigator, "credentials", { value: { get }, configurable: true });
  vi.spyOn(api, "passkeyLoginStart").mockResolvedValue({ ceremony_id: "c1", options: { publicKey: { challenge: "AAAA", rpId: "aho.cloud", allowCredentials: [{ type: "public-key", id: "AQID" }] } } });
});
afterEach(() => {
  vi.unstubAllGlobals();
  Object.defineProperty(navigator, "credentials", { value: undefined, configurable: true });
});

const approval = (stage: Stage): ApprovalView => ({ stage, user: ana, owner: true });

/** A server that tracks one release through approvals and promotions like the control plane. */
function promotions(start: Release) {
  let current = start;
  vi.spyOn(api, "approve").mockImplementation(async (p, r, body) => {
    calls.push(`approve ${r} ${body.stage}`);
    expect(p).toBe("shop");
    expect(body.proof.method).toBe("passkey");
    current = { ...current, approvals: [...current.approvals, approval(body.stage)] };
    return current;
  });
  vi.spyOn(api, "promote").mockImplementation(async (_p, r, body) => {
    calls.push(`promote ${r} ${body.stage}`);
    current = { ...current, stage: body.stage, status: "active" };
    return current;
  });
}

/** The passkey challenge of each assertion, and what it should have been for `stages` of `release`. */
function challenges(r: Release, stages: Stage[]) {
  const got = get.mock.calls.map(([options]) => u8((options as CredentialRequestOptions).publicKey!.challenge));
  const want = stages.map((s) => [...approvalChallenge(approvalMessage("shop", r.id, r.manifest_hash, s))]);
  return { got, want };
}

function renderModal(project: Project, r: Release, onChanged = vi.fn()) {
  render(
    <MemoryRouter>
      <GoLiveModal project={project} release={r} pollMs={2} onClose={() => {}} onChanged={onChanged} />
    </MemoryRouter>,
  );
  return { dialog: screen.getByRole("dialog", { name: "Go live" }), onChanged };
}

describe("go live", () => {
  it("skips binding and re-proposal when the domain is bound and declared, then approves staging, promotes, approves live, promotes", async () => {
    const user = userEvent.setup();
    const atPreview = release({ id: "r-p", stage: "preview", status: "active" });
    promotions(atPreview);
    const add = vi.spyOn(api, "addDomain");
    const propose = vi.spyOn(api, "propose");
    const { dialog } = renderModal({ slug: "shop", org: "acme", active: [{ stage: "preview", release: "r-p" }], domains: [verified] }, atPreview);

    expect(within(dialog).getByRole("radio", { name: /shop\.example\.com/ })).toBeChecked();
    expect(within(dialog).getByText("DNS records for shop.example.com")).toBeInTheDocument();
    expect(within(dialog).getByText("edge.aho.cloud")).toBeInTheDocument();
    await user.click(within(dialog).getByRole("button", { name: "Continue" }));

    await within(dialog).findByRole("region", { name: "approval card" });
    expect(add).not.toHaveBeenCalled();
    expect(propose).not.toHaveBeenCalled();
    await user.click(within(dialog).getByRole("button", { name: "Approve & go live" }));

    expect(await within(dialog).findByText(/is live\./)).toBeInTheDocument();
    expect(calls).toEqual(["passkey", "approve r-p staging", "promote r-p staging", "passkey", "approve r-p live", "promote r-p live"]);
    const { got, want } = challenges(atPreview, ["staging", "live"]);
    expect(got).toEqual(want);
    expect(within(dialog).getByRole("link", { name: /https:\/\/shop\.example\.com/ })).toHaveAttribute("href", "https://shop.example.com");
    expect(within(dialog).queryByText(/Waiting for DNS/)).toBeNull();
  });

  it("re-proposes with the domain when the release does not declare it, waits for preview, and ships the new release", async () => {
    const user = userEvent.setup();
    const atPreview = release({ id: "r-p", stage: "preview", status: "active", manifest: { ...manifest, domains: [] } });
    const pending: DomainView = { hostname: "www.example.com", mode: "cname", state: "pending", records: [{ kind: "TXT", name: "_aho.www.example.com", value: "aho-verify=abc" }] };
    vi.spyOn(api, "addDomain").mockImplementation(async (_p, body) => {
      calls.push(`add ${body.hostname} ${body.mode}`);
      return pending;
    });
    const copy = release({ id: "r-copy", stage: null, status: "building", image_digest: null, manifest: { ...manifest, domains: [{ hostname: "www.example.com", verification: "cname" }] } });
    const propose = vi.spyOn(api, "propose").mockImplementation(async () => {
      calls.push("propose");
      return copy;
    });
    vi.spyOn(api, "release").mockImplementation(async (_p, r) => {
      calls.push(`poll ${r}`);
      return { ...copy, status: "active", stage: "preview", image_digest: "sha256:ff" };
    });
    promotions({ ...copy, status: "active", stage: "preview" });
    const { dialog, onChanged } = renderModal({ slug: "shop", org: "acme", active: [{ stage: "preview", release: "r-p" }], domains: [] }, atPreview);

    expect(within(dialog).getByRole("button", { name: "Continue" })).toBeDisabled();
    await user.type(within(dialog).getByLabelText("Hostname"), "www.example.com");
    await user.click(within(dialog).getByRole("button", { name: "Add domain" }));
    expect(await within(dialog).findByText("aho-verify=abc")).toBeInTheDocument();
    expect(within(dialog).getByRole("radio", { name: /www\.example\.com/ })).toBeChecked();
    await user.click(within(dialog).getByRole("button", { name: "Continue" }));

    await within(dialog).findByRole("region", { name: "approval card" });
    expect(propose).toHaveBeenCalledWith("shop", {
      manifest: { ...atPreview.manifest, domains: [{ hostname: "www.example.com", verification: "cname" }] },
      edge_toml: null,
      provenance: atPreview.provenance,
    });
    expect(propose.mock.calls[0]![1].manifest.source).toEqual(atPreview.manifest.source);
    await user.click(within(dialog).getByRole("button", { name: "Approve & go live" }));
    expect(await within(dialog).findByText(/is live\./)).toBeInTheDocument();
    expect(calls).toEqual(["add www.example.com cname", "propose", "poll r-copy", "passkey", "approve r-copy staging", "promote r-copy staging", "passkey", "approve r-copy live", "promote r-copy live"]);
    expect(challenges(copy, ["staging", "live"]).got).toEqual(challenges(copy, ["staging", "live"]).want);
    // The domain is not verified yet: say so and keep its records at hand.
    expect(within(dialog).getByText(/Waiting for DNS/)).toBeInTheDocument();
    expect(within(dialog).getByRole("button", { name: "Verify www.example.com" })).toBeInTheDocument();
    expect(onChanged).toHaveBeenCalled();
  });

  it("surfaces a failed promotion on its step and resumes from there on retry", async () => {
    const user = userEvent.setup();
    const atPreview = release({ id: "r-p", stage: "preview", status: "active" });
    promotions(atPreview);
    const promote = api.promote as unknown as Mock;
    const inner = promote.getMockImplementation()!;
    let failed = false;
    promote.mockImplementation(async (p: string, r: string, body: { stage: Stage }) => {
      if (body.stage === "live" && !failed) {
        failed = true;
        calls.push("promote refused");
        throw new ApiError(409, "quorum_not_met", "live needs 2 approvals, has 1");
      }
      return inner(p, r, body);
    });
    const { dialog } = renderModal({ slug: "shop", org: "acme", active: [{ stage: "preview", release: "r-p" }], domains: [verified] }, atPreview);
    await user.click(within(dialog).getByRole("button", { name: "Continue" }));
    await user.click(await within(dialog).findByRole("button", { name: "Approve & go live" }));

    expect(await within(dialog).findByText(/quorum_not_met/)).toBeInTheDocument();
    expect(dialog.querySelector('[data-step="promote_live"]')).toHaveAttribute("data-state", "failed");
    expect(dialog.querySelector('[data-step="promote_staging"]')).toHaveAttribute("data-state", "done");

    await user.click(within(dialog).getByRole("button", { name: "Retry" }));
    expect(await within(dialog).findByText(/is live\./)).toBeInTheDocument();
    // Staging is not redone and the live approval already given is not asked for again.
    expect(calls).toEqual(["passkey", "approve r-p staging", "promote r-p staging", "passkey", "approve r-p live", "promote refused", "promote r-p live"]);
  });
});

describe("go-live steps", () => {
  it("binds a hostname only when the project lacks it", async () => {
    const client = { addDomain: vi.fn(async () => verified) };
    expect(await ensureDomain(client, "shop", [verified], { hostname: "shop.example.com", mode: "apex" })).toEqual({ domain: verified, added: false });
    expect(client.addDomain).not.toHaveBeenCalled();
    expect((await ensureDomain(client, "shop", [], { hostname: "shop.example.com", mode: "cname" })).added).toBe(true);
    expect(client.addDomain).toHaveBeenCalledWith("shop", { hostname: "shop.example.com", mode: "cname" });
  });

  it("keeps a release that already declares the hostname, proposing nothing", async () => {
    const client = { propose: vi.fn(), release: vi.fn() };
    const declared = release({ stage: "preview", status: "active" });
    expect(await ensureDeclared(client, declared, { hostname: "shop.example.com", mode: "cname" }, { intervalMs: 1 })).toEqual({ release: declared, proposed: false });
    expect(client.propose).not.toHaveBeenCalled();
  });

  it("does not sign or promote a stage the release already reached", async () => {
    const atStaging = release({ stage: "staging", status: "active", requirement: { approvals: 1, owner_required: true }, approvals: [approval("staging")] });
    const client = { approve: vi.fn(async (_p: string, _r: string, b: { stage: Stage }) => ({ ...atStaging, approvals: [...atStaging.approvals, approval(b.stage)] })), promote: vi.fn(async () => ({ ...atStaging, stage: "live" as const })) };
    const sign = vi.fn(async () => ({ method: "passkey" as const, credential: {} }));
    const steps: string[] = [];
    const done = await promoteToLive(client, atStaging, { userId: ana.id, sign, onStep: (s, st) => steps.push(`${s}:${st}`) });
    expect(done.stage).toBe("live");
    expect(sign).toHaveBeenCalledTimes(1);
    expect(sign).toHaveBeenCalledWith(approvalMessage("shop", atStaging.id, atStaging.manifest_hash, "live"));
    expect(client.approve.mock.calls.map((c) => c[2].stage)).toEqual(["live"]);
    expect(client.promote.mock.calls.map((c) => (c as unknown as [string, string, { stage: Stage }])[2].stage)).toEqual(["live"]);
    expect(steps).toEqual(["approve_staging:skipped", "promote_staging:skipped", "approve_live:running", "approve_live:done", "promote_live:running", "promote_live:done"]);
  });

  it("promotes without a signature when no approval is required", async () => {
    const free = release({ stage: "preview", status: "active", requirement: { approvals: 0, owner_required: false } });
    const client = { approve: vi.fn(), promote: vi.fn(async (_p: string, _r: string, b: { stage: Stage }) => ({ ...free, stage: b.stage })) };
    const sign = vi.fn();
    await promoteToLive(client, free, { userId: ana.id, sign });
    expect(sign).not.toHaveBeenCalled();
    expect(client.approve).not.toHaveBeenCalled();
    expect(client.promote).toHaveBeenCalledTimes(2);
  });
});
