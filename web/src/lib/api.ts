// Typed control-plane client over the generated `routes`. Every path comes from `routes` +
// `fill`; every body type from the generated contract. Errors surface as `ApiError` with the
// server's stable `code`.
import { fill, routes } from "../gen/api";
import type {
  AddDomain,
  AddMember,
  ApiError as WireApiError,
  Approve,
  CheckManifest,
  CheckResult,
  CreateOrg,
  CreateProject,
  CreateSandbox,
  DeviceApprove,
  DomainView,
  EnvVarView,
  FileContent,
  FileTree,
  GrantStandingOrders,
  LedgerEntry,
  Me,
  Member,
  Org,
  PasskeyChallenge,
  PasskeyFinish,
  PasskeyLoginStart,
  PasskeyRegisterStart,
  Project,
  Promote,
  ProposeRelease,
  Reject,
  Release,
  Rollback,
  Sandbox,
  SandboxChat,
  SandboxSettings,
  Session,
  SessionUploaded,
  SecretView,
  SetEnvVar,
  SetSecret,
  StartSandboxChat,
  TerminalAccess,
  TerminalRequest,
  Workspace,
  WorkspaceDeployed,
} from "../gen/api";
import type { Manifest, Refusal, ReleasePolicy } from "../gen/contract";
import { currentToken, signOut } from "./session";

/** Where aho works: a project's workspace, or a sandbox's `/workspace`. */
export type AhoTarget = { kind: "project"; project: string } | { kind: "sandbox"; sandbox: string };

/** A failed API call: the server's `ApiError` body, or a transport failure described the same way. */
export class ApiError extends Error {
  /** HTTP status; `0` when no response arrived. */
  readonly status: number;
  /** Server code (`unauthorized`, `refused`, `quorum_not_met`, …), `network`, or `http_<status>` for a body that is not an `ApiError`. */
  readonly code: string;
  readonly refusals: Refusal[];

  constructor(status: number, code: string, message: string, refusals: Refusal[] = []) {
    super(message);
    this.name = "ApiError";
    this.status = status;
    this.code = code;
    this.refusals = refusals;
  }
}

function isWireError(v: unknown): v is WireApiError {
  if (typeof v !== "object" || v === null) return false;
  const o = v as Record<string, unknown>;
  return typeof o.code === "string" && typeof o.message === "string" && (o.refusals === undefined || Array.isArray(o.refusals));
}

/** Builds the `ApiError` for a non-2xx response from its status and raw body text. */
export function decodeError(status: number, statusText: string, body: string): ApiError {
  let parsed: unknown;
  try {
    parsed = JSON.parse(body);
  } catch {
    parsed = undefined;
  }
  if (isWireError(parsed)) return new ApiError(status, parsed.code, parsed.message, parsed.refusals ?? []);
  const text = body.trim().slice(0, 200);
  return new ApiError(status, `http_${status}`, text === "" ? `${status} ${statusText}`.trim() : `${status} ${statusText}: ${text}`.trim());
}

type Method = "GET" | "POST" | "PUT" | "DELETE";

export interface ApiOptions {
  /** Prefix for every route; empty = same origin. */
  baseUrl?: string;
  fetch?: typeof fetch;
  /** Bearer token source; defaults to the session store. */
  token?: () => string | null;
  /** Called on 401; defaults to signing out so the router sends the user to the login page. */
  onUnauthorized?: () => void;
}

export class Api {
  readonly #base: string;
  readonly #fetch: typeof fetch;
  readonly #token: () => string | null;
  readonly #onUnauthorized: () => void;

  constructor(options: ApiOptions = {}) {
    this.#base = (options.baseUrl ?? "").replace(/\/+$/, "");
    this.#fetch = options.fetch ?? ((input, init) => globalThis.fetch(input, init));
    this.#token = options.token ?? currentToken;
    this.#onUnauthorized = options.onUnauthorized ?? signOut;
  }

  /** One request. JSON bodies are serialized; strings are sent raw with `contentType`. Resolves `undefined` for empty replies. */
  async request<T>(method: Method, path: string, body?: unknown, contentType?: string): Promise<T> {
    const headers: Record<string, string> = { accept: "application/json" };
    const token = this.#token();
    if (token !== null) headers.authorization = `Bearer ${token}`;
    let payload: string | undefined;
    if (typeof body === "string" && contentType !== undefined) {
      payload = body;
      headers["content-type"] = contentType;
    } else if (body !== undefined) {
      payload = JSON.stringify(body);
      headers["content-type"] = "application/json";
    }
    let res: Response;
    try {
      res = await this.#fetch(this.#base + path, { method, headers, body: payload });
    } catch (e) {
      throw new ApiError(0, "network", `could not reach the control plane (${e instanceof Error ? e.message : String(e)})`);
    }
    const text = await res.text();
    if (!res.ok) {
      const err = decodeError(res.status, res.statusText, text);
      if (res.status === 401) this.#onUnauthorized();
      throw err;
    }
    if (text === "") return undefined as T;
    try {
      return JSON.parse(text) as T;
    } catch {
      throw new ApiError(res.status, "invalid_response", `${method} ${path} answered with a body that is not JSON`);
    }
  }

  // ---- auth
  passkeyRegisterStart(body: PasskeyRegisterStart) {
    return this.request<PasskeyChallenge>("POST", routes.PASSKEY_REGISTER_START, body);
  }
  passkeyRegisterFinish(body: PasskeyFinish) {
    return this.request<Session>("POST", routes.PASSKEY_REGISTER_FINISH, body);
  }
  passkeyLoginStart(body: PasskeyLoginStart) {
    return this.request<PasskeyChallenge>("POST", routes.PASSKEY_LOGIN_START, body);
  }
  passkeyLoginFinish(body: PasskeyFinish) {
    return this.request<Session>("POST", routes.PASSKEY_LOGIN_FINISH, body);
  }
  approveDevice(body: DeviceApprove) {
    return this.request<void>("POST", routes.DEVICE_APPROVE, body);
  }
  me() {
    return this.request<Me>("GET", routes.ME);
  }

  // ---- orgs and projects
  createOrg(body: CreateOrg) {
    return this.request<Org>("POST", routes.ORGS, body);
  }
  members(org: string) {
    return this.request<Member[]>("GET", fill(routes.ORG_MEMBERS, { org }));
  }
  /** Owners only; the user must have signed in once. */
  addMember(org: string, body: AddMember) {
    return this.request<Member>("POST", fill(routes.ORG_MEMBERS, { org }), body);
  }
  policy(org: string) {
    return this.request<ReleasePolicy>("GET", fill(routes.ORG_POLICY, { org }));
  }
  projects(org: string) {
    return this.request<Project[]>("GET", fill(routes.ORG_PROJECTS, { org }));
  }
  createProject(org: string, body: CreateProject) {
    return this.request<Project>("POST", fill(routes.ORG_PROJECTS, { org }), body);
  }
  project(p: string) {
    return this.request<Project>("GET", fill(routes.PROJECT, { p }));
  }

  // ---- releases
  checkManifest(body: CheckManifest) {
    return this.request<CheckResult>("POST", routes.MANIFEST_CHECK, body);
  }
  releases(p: string) {
    return this.request<Release[]>("GET", fill(routes.RELEASES, { p }));
  }
  propose(p: string, body: ProposeRelease) {
    return this.request<Release>("POST", fill(routes.RELEASES, { p }), body);
  }
  release(p: string, r: string) {
    return this.request<Release>("GET", fill(routes.RELEASE, { p, r }));
  }
  approve(p: string, r: string, body: Approve) {
    return this.request<Release>("POST", fill(routes.APPROVE, { p, r }), body);
  }
  reject(p: string, r: string, body: Reject) {
    return this.request<Release>("POST", fill(routes.REJECT, { p, r }), body);
  }
  promote(p: string, r: string, body: Promote) {
    return this.request<Release>("POST", fill(routes.PROMOTE, { p, r }), body);
  }
  rollback(p: string, body: Rollback) {
    return this.request<Release>("POST", fill(routes.ROLLBACK, { p }), body);
  }
  grantStandingOrders(p: string, body: GrantStandingOrders) {
    return this.request<void>("POST", fill(routes.STANDING_ORDERS, { p }), body);
  }
  ledger(p: string) {
    return this.request<LedgerEntry[]>("GET", fill(routes.LEDGER, { p }));
  }
  /** Uploads an aho session log (JSONL, append order); the server verifies its hash chain. */
  uploadSession(p: string, sessionId: string, jsonl: string) {
    const path = `${fill(routes.SESSIONS, { p })}?session_id=${encodeURIComponent(sessionId)}`;
    return this.request<SessionUploaded>("POST", path, jsonl, "application/x-ndjson");
  }

  // ---- domains
  domains(p: string) {
    return this.request<DomainView[]>("GET", fill(routes.DOMAINS, { p }));
  }
  addDomain(p: string, body: AddDomain) {
    return this.request<DomainView>("POST", fill(routes.DOMAINS, { p }), body);
  }
  verifyDomain(p: string, host: string) {
    return this.request<DomainView>("POST", fill(routes.DOMAIN_VERIFY, { p, host }));
  }

  // ---- secrets (write-only: values go in, fingerprints come out)
  secrets(p: string) {
    return this.request<SecretView[]>("GET", fill(routes.SECRETS, { p }));
  }
  setSecret(p: string, body: SetSecret) {
    return this.request<SecretView>("PUT", fill(routes.SECRETS, { p }), body);
  }

  // ---- workspaces (Entry A)
  /** Starts a workspace, or resumes `sessionId`'s with a fresh socket token. The body type is not in the generated API yet. */
  startWorkspace(p: string, sessionId?: string) {
    return this.request<Workspace>("POST", fill(routes.WORKSPACES, { p }), sessionId === undefined ? {} : { session_id: sessionId });
  }
  /** Ships the caller's workspace files: the workspace's `deploy.json` without `manifest`, else the confirmed manifest. */
  workspaceDeploy(p: string, manifest?: Manifest) {
    return this.request<WorkspaceDeployed>("POST", fill(routes.WORKSPACE_DEPLOY, { p }), manifest === undefined ? {} : { manifest });
  }
  /** What a deploy would ship, in path order. */
  workspaceFiles(p: string) {
    return this.request<FileTree>("GET", fill(routes.WORKSPACE_FILES, { p }));
  }
  workspaceFile(p: string, path: string) {
    return this.request<FileContent>("GET", `${fill(routes.WORKSPACE_FILE, { p })}?path=${encodeURIComponent(path)}`);
  }

  // ---- sandboxes (`{s}` = sandbox slug; every member of its org may use it)
  sandboxes(org: string) {
    return this.request<Sandbox[]>("GET", fill(routes.ORG_SANDBOXES, { org }));
  }
  /** Boots it at once; it answers `starting`. */
  createSandbox(org: string, body: CreateSandbox) {
    return this.request<Sandbox>("POST", fill(routes.ORG_SANDBOXES, { org }), body);
  }
  sandbox(s: string) {
    return this.request<Sandbox>("GET", fill(routes.SANDBOX, { s }));
  }
  /** Destroys the VM and its disk. */
  deleteSandbox(s: string) {
    return this.request<void>("DELETE", fill(routes.SANDBOX, { s }));
  }
  startSandbox(s: string) {
    return this.request<Sandbox>("POST", fill(routes.SANDBOX_START, { s }));
  }
  /** Stops the VM; the disk is kept. */
  stopSandbox(s: string) {
    return this.request<Sandbox>("POST", fill(routes.SANDBOX_STOP, { s }));
  }
  /** A single-use token for a terminal socket speaking exec frames (`lib/exec.ts`). */
  sandboxTerminal(s: string, body: TerminalRequest) {
    return this.request<TerminalAccess>("POST", fill(routes.SANDBOX_TERMINAL, { s }), body);
  }
  /** aho rpc to the aho inside: a new session, or `sessionId`'s with a fresh socket token. */
  sandboxChat(s: string, sessionId?: string) {
    const body: StartSandboxChat = { session_id: sessionId ?? null };
    return this.request<SandboxChat>("POST", fill(routes.SANDBOX_CHAT, { s }), body);
  }
  sandboxFiles(s: string) {
    return this.request<FileTree>("GET", fill(routes.SANDBOX_FILES, { s }));
  }
  sandboxFile(s: string, path: string) {
    return this.request<FileContent>("GET", `${fill(routes.SANDBOX_FILE, { s })}?path=${encodeURIComponent(path)}`);
  }
  /** A running sandbox restarts with them; its disk is kept. */
  setSandboxSettings(s: string, body: SandboxSettings) {
    return this.request<Sandbox>("PUT", fill(routes.SANDBOX_SETTINGS, { s }), body);
  }
  // Environment variables are write-only like secrets; values reach the VM at its next start.
  sandboxEnv(s: string) {
    return this.request<EnvVarView[]>("GET", fill(routes.SANDBOX_ENV, { s }));
  }
  setSandboxEnv(s: string, body: SetEnvVar) {
    return this.request<EnvVarView>("PUT", fill(routes.SANDBOX_ENV, { s }), body);
  }
  deleteSandboxEnv(s: string, name: string) {
    return this.request<void>("DELETE", fill(routes.SANDBOX_ENV_VAR, { s, name }));
  }
}

/** The console's client: same-origin, or `VITE_AHO_CLOUD_API` when the API lives elsewhere. */
export const api = new Api({ baseUrl: import.meta.env.VITE_AHO_CLOUD_API ?? "" });
