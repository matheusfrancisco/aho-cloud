// Following a proposed release to preview: the control plane builds it, scans it for tainted
// secrets and, when clean, activates it at preview on its own. The console only polls.
import type { Release } from "../gen/api";
import type { Api } from "./api";

export type PreviewState = "building" | "activating" | "active" | "failed";

/** Where `release` is on its way to preview. A release already past preview counts as active. */
export function previewState(release: Release): PreviewState {
  switch (release.status) {
    case "building":
      return "building";
    case "awaiting_approval":
      return "activating";
    case "active":
      return "active";
    case "build_failed":
    case "tainted":
    case "rejected":
    case "superseded":
      return "failed";
  }
}

const FAILURE: Partial<Record<Release["status"], string>> = {
  build_failed: "The build failed.",
  tainted: "The release is blocked: a secret value appeared in the session log. Rotate it and deploy again.",
  rejected: "The release was rejected.",
  superseded: "A newer release replaced this one at preview.",
};

/** The release did not reach preview; `release` is its last state. */
export class DeployFailed extends Error {
  readonly release: Release;
  constructor(release: Release, message: string) {
    super(message);
    this.name = "DeployFailed";
    this.release = release;
  }
}

/** Resolves after `ms`, or rejects with the signal's reason once it aborts. */
export function sleep(ms: number, signal?: AbortSignal): Promise<void> {
  const { promise, resolve, reject } = Promise.withResolvers<void>();
  if (signal?.aborted) {
    reject(signal.reason);
    return promise;
  }
  const onAbort = () => {
    clearTimeout(handle);
    reject(signal?.reason);
  };
  const handle = setTimeout(() => {
    signal?.removeEventListener("abort", onAbort);
    resolve();
  }, ms);
  signal?.addEventListener("abort", onAbort, { once: true });
  return promise;
}

export interface WaitOptions {
  intervalMs: number;
  /** A built release that stays unactivated this long is reported as failed: the preview deployment did not come up. */
  activationTimeoutMs?: number;
  signal?: AbortSignal;
  onUpdate?: (release: Release) => void;
}

/** Polls `release` until it is active at preview. Rejects with `DeployFailed` when it cannot get there. */
export async function waitForPreview(client: Pick<Api, "release">, project: string, release: Release, options: WaitOptions): Promise<Release> {
  const activationTimeout = options.activationTimeoutMs ?? 180_000;
  let current = release;
  let builtAt: number | null = null;
  for (;;) {
    const state = previewState(current);
    if (state === "active") return current;
    if (state === "failed") throw new DeployFailed(current, FAILURE[current.status] ?? `The release is ${current.status}.`);
    if (state === "activating") {
      builtAt ??= Date.now();
      if (Date.now() - builtAt >= activationTimeout) {
        throw new DeployFailed(current, "The release was built but did not become active at preview; the preview deployment may have failed.");
      }
    }
    await sleep(options.intervalMs, options.signal);
    current = await client.release(project, current.id);
    options.onUpdate?.(current);
  }
}
