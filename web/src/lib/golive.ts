// "Go live": put the release running at preview on the project's domain. The domain is bound to
// the project, the release must declare it (else a copy that does is proposed and followed to
// preview), then it is approved and promoted stage by stage: staging, then live.
import type { StepState } from "@aho/ui";
import type { ApprovalProof, DomainView, Release } from "../gen/api";
import type { DomainMode, Manifest, Stage } from "../gen/contract";
import type { Api } from "./api";
import { approvalMessage } from "./approval";
import { waitForPreview, type WaitOptions } from "./deploy";
import { approvalProgress, STAGES } from "./release";

export interface DomainChoice {
  hostname: string;
  mode: DomainMode;
}

export function declares(manifest: Manifest, hostname: string): boolean {
  return manifest.domains.some((d) => d.hostname === hostname);
}

/** The project's binding for `choice`, added when the project does not have it yet. */
export async function ensureDomain(client: Pick<Api, "addDomain">, project: string, domains: readonly DomainView[], choice: DomainChoice): Promise<{ domain: DomainView; added: boolean }> {
  const existing = domains.find((d) => d.hostname === choice.hostname);
  if (existing !== undefined) return { domain: existing, added: false };
  return { domain: await client.addDomain(project, { hostname: choice.hostname, mode: choice.mode }), added: true };
}

/**
 * A release that declares the domain: `release` itself when it already does; otherwise a new
 * proposal with the same source and provenance and the domain appended, followed to preview.
 */
export async function ensureDeclared(
  client: Pick<Api, "propose" | "release">,
  release: Release,
  domain: DomainChoice,
  wait: WaitOptions & { onProposed?: (release: Release) => void },
): Promise<{ release: Release; proposed: boolean }> {
  if (declares(release.manifest, domain.hostname)) return { release, proposed: false };
  const manifest: Manifest = { ...release.manifest, domains: [...release.manifest.domains, { hostname: domain.hostname, verification: domain.mode }] };
  const proposed = await client.propose(release.project, { manifest, edge_toml: null, provenance: release.provenance });
  wait.onProposed?.(proposed);
  return { release: await waitForPreview(client, release.project, proposed, wait), proposed: true };
}

export type PromoteStep = "approve_staging" | "promote_staging" | "approve_live" | "promote_live";

export const PROMOTE_STEPS: ReadonlyArray<{ id: PromoteStep; stage: Stage; label: string }> = [
  { id: "approve_staging", stage: "staging", label: "Approve for staging with your passkey" },
  { id: "promote_staging", stage: "staging", label: "Promote to staging" },
  { id: "approve_live", stage: "live", label: "Approve for live with your passkey" },
  { id: "promote_live", stage: "live", label: "Promote to live" },
];

export interface PromoteOptions {
  /** The signer; an approval they already gave is not asked for again. */
  userId: string;
  /** Signs an approval message (passkey assertion or device key). */
  sign: (message: string) => Promise<ApprovalProof>;
  onStep?: (step: PromoteStep, state: StepState) => void;
  onRelease?: (release: Release) => void;
}

/**
 * Approves and promotes `release` at staging, then at live. Stages it already reached are
 * skipped, and so is an approval that is not needed (quorum met, or already signed by the user),
 * so a failed run can be retried from where it stopped.
 */
export async function promoteToLive(client: Pick<Api, "approve" | "promote">, release: Release, options: PromoteOptions): Promise<Release> {
  let current = release;
  for (const stage of ["staging", "live"] as const) {
    const approveStep: PromoteStep = `approve_${stage}`;
    const promoteStep: PromoteStep = `promote_${stage}`;
    if (current.stage !== null && STAGES.indexOf(current.stage) >= STAGES.indexOf(stage)) {
      options.onStep?.(approveStep, "skipped");
      options.onStep?.(promoteStep, "skipped");
      continue;
    }
    const progress = approvalProgress(current, stage);
    let step: PromoteStep = approveStep;
    try {
      if (progress.met || progress.approvals.some((a) => a.user.id === options.userId)) {
        options.onStep?.(approveStep, "skipped");
      } else {
        options.onStep?.(approveStep, "running");
        const proof = await options.sign(approvalMessage(current.project, current.id, current.manifest_hash, stage));
        current = await client.approve(current.project, current.id, { stage, proof });
        options.onRelease?.(current);
        options.onStep?.(approveStep, "done");
      }
      step = promoteStep;
      options.onStep?.(promoteStep, "running");
      current = await client.promote(current.project, current.id, { stage });
      options.onRelease?.(current);
      options.onStep?.(promoteStep, "done");
    } catch (e) {
      options.onStep?.(step, "failed");
      throw e;
    }
  }
  return current;
}
