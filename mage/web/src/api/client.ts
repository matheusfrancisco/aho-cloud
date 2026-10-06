// Typed client for Mage's UI JSON API (`mage_core::wire::api`). Every path comes from the generated
// `routes` + `fill`, every body type from the generated wire types. Mutations carry `X-CSRF-Token`
// when the host has one (self-host sessions); errors surface as `MageApiError` with the server's
// stable `code` and `request_id`.
import { CodedError } from "@aho/ui";
import { fill, routes } from "../gen/mage";
import type {
  ApiErrorBody,
  ApiErrorCode,
  ArtifactId,
  ArtifactList,
  ArtifactView,
  AuditPage,
  AuditQuery,
  CauseId,
  CommandQueued,
  ConnectIntegrationResponse,
  ConnectRepoRequest,
  Customer,
  CustomerId,
  CustomerInput,
  CustomerList,
  Deployment,
  DeploymentList,
  FixBriefQuery,
  FixBriefView,
  IndexVersionRequest,
  InstallationRepoList,
  IntegrationKind,
  IntegrationList,
  InviteRequest,
  InviteView,
  JobQueued,
  MemberList,
  MemberView,
  ModelKeyMeta,
  ModelSettingsUpdate,
  ModelSettingsView,
  OrgSettings,
  Paged,
  RecipeHistory,
  Repo,
  RepoDetail,
  RepoId,
  RepoList,
  ResolvedId,
  Resolution,
  ResolveTicketRequest,
  RunDetail,
  RunEventPage,
  RunEventsQuery,
  RunId,
  RunListQuery,
  RunStarted,
  RunStatusView,
  RunSummary,
  SavedRecipe,
  SaveRecipeRequest,
  SessionView,
  SetDeploymentRequest,
  SetModelKeyRequest,
  SetRoleRequest,
  SetTargetRequest,
  ShareLink,
  StartHarnessRunRequest,
  StartLearnRequest,
  SteerRequest,
  TicketDetail,
  TicketId,
  TicketListQuery,
  TicketSummary,
  TicketTarget,
  UsageQuery,
  UsageReport,
  UserId,
  VerifyRecipeRequest,
} from "../gen/mage";

/** `error.code` of the server, or what the client saw instead of an error body. */
export type MageErrorCode = ApiErrorCode | "network" | "invalid_response" | `http_${number}`;

/** A failed API call: the server's `ApiErrorBody`, or a transport failure described the same way. */
export class MageApiError extends CodedError {
  /** HTTP status; `0` when no response arrived. */
  readonly status: number;
  /** The server's `request_id` (shown so support can find the request); `null` without an error body. */
  readonly requestId: string | null;
  declare readonly code: MageErrorCode;

  constructor(status: number, code: MageErrorCode, message: string, requestId: string | null = null) {
    super(code, message);
    this.name = "MageApiError";
    this.status = status;
    this.requestId = requestId;
  }
}

const API_ERROR_CODES: Record<ApiErrorCode, true> = { not_found: true, unauthenticated: true, forbidden: true, conflict: true, invalid: true, too_large: true, rate_limited: true, unavailable: true, internal: true };

function isErrorBody(v: unknown): v is ApiErrorBody {
  if (typeof v !== "object" || v === null) return false;
  const e = (v as Record<string, unknown>).error;
  if (typeof e !== "object" || e === null) return false;
  const o = e as Record<string, unknown>;
  return typeof o.code === "string" && Object.hasOwn(API_ERROR_CODES, o.code) && typeof o.message === "string" && typeof o.request_id === "string";
}

/** Builds the `MageApiError` for a non-2xx response from its status and raw body text. */
export function decodeError(status: number, statusText: string, body: string): MageApiError {
  let parsed: unknown;
  try {
    parsed = JSON.parse(body);
  } catch {
    parsed = undefined;
  }
  if (isErrorBody(parsed)) return new MageApiError(status, parsed.error.code, parsed.error.message, parsed.error.request_id);
  const head = `${status} ${statusText}`.trim();
  const text = body.trim().slice(0, 200);
  return new MageApiError(status, `http_${status}`, text === "" ? head : `${head}: ${text}`);
}

type Method = "GET" | "POST" | "PUT" | "DELETE";
type QueryValue = string | number | boolean | null | undefined;

/** `path?k=v&…` with the defined values, URI-encoded, in the given order. */
export function withQuery(path: string, query: Readonly<Record<string, QueryValue>> = {}): string {
  const params = new URLSearchParams();
  for (const [k, v] of Object.entries(query)) {
    if (v !== undefined && v !== null && v !== "") params.append(k, String(v));
  }
  const qs = params.toString();
  return qs === "" ? path : `${path}?${qs}`;
}

/** `{kind}` path segment of the integration routes (`mage_core::wire::api::integration_slug`). */
export const INTEGRATION_SLUG: Readonly<Record<IntegrationKind, string>> = {
  github: "github",
  github_fix: "github-fix",
  linear: "linear",
  slack: "slack",
};

export interface MageApiOptions {
  /** Prefix for every route (`""` = same origin). */
  apiBase?: string;
  fetch?: typeof fetch;
  /** The session's CSRF token (`SessionView.csrf_token`), read on every mutation; `null` sends none (aho cloud edge). */
  csrfToken?: () => string | null;
  /** Called on 401 (the session expired or was revoked). */
  onUnauthorized?: () => void;
}

/** A text artifact's first bytes. */
export interface ArtifactText {
  text: string;
  /** Only the first `maxBytes` were fetched. */
  truncated: boolean;
}

export class MageApi {
  readonly apiBase: string;
  readonly #fetch: typeof fetch;
  readonly #csrf: () => string | null;
  readonly #onUnauthorized: () => void;

  constructor(options: MageApiOptions = {}) {
    this.apiBase = (options.apiBase ?? "").replace(/\/+$/, "");
    this.#fetch = options.fetch ?? ((input, init) => globalThis.fetch(input, init));
    this.#csrf = options.csrfToken ?? (() => null);
    this.#onUnauthorized = options.onUnauthorized ?? (() => {});
  }

  /** Absolute URL of a route path (for links, `<video>`, EventSource). */
  url(path: string): string {
    return this.apiBase + path;
  }

  /** Same-origin cookies for a same-origin API; cross-origin hosts opt into credentials. */
  get withCredentials(): boolean {
    return this.apiBase !== "";
  }

  /** One request; resolves the OK `Response` or throws `MageApiError`. */
  async send(method: Method, path: string, body?: unknown, extraHeaders: Record<string, string> = {}): Promise<Response> {
    const headers: Record<string, string> = { accept: "application/json", ...extraHeaders };
    if (method !== "GET") {
      const csrf = this.#csrf();
      if (csrf !== null) headers["x-csrf-token"] = csrf;
    }
    let payload: string | undefined;
    if (body !== undefined) {
      payload = JSON.stringify(body);
      headers["content-type"] = "application/json";
    }
    let res: Response;
    try {
      res = await this.#fetch(this.url(path), { method, headers, body: payload, credentials: this.withCredentials ? "include" : "same-origin" });
    } catch (e) {
      throw new MageApiError(0, "network", `could not reach Mage (${e instanceof Error ? e.message : String(e)})`);
    }
    if (!res.ok) {
      const err = decodeError(res.status, res.statusText, await res.text());
      if (res.status === 401) this.#onUnauthorized();
      throw err;
    }
    return res;
  }

  /** One JSON request. Resolves `undefined` for empty replies (204). */
  async request<T>(method: Method, path: string, body?: unknown): Promise<T> {
    const res = await this.send(method, path, body);
    const text = await res.text();
    if (text === "") return undefined as T;
    try {
      return JSON.parse(text) as T;
    } catch {
      throw new MageApiError(res.status, "invalid_response", `${method} ${path} answered with a body that is not JSON`);
    }
  }

  session() {
    return this.request<SessionView>("GET", routes.SESSION);
  }
  /** Org (and, for causes, ticket) owning a prefixed id, for short links. */
  resolve(id: string) {
    return this.request<ResolvedId>("GET", fill(routes.RESOLVE, { id }));
  }
  /** Ends this session (`all`: every session of the user). */
  logout(all = false) {
    return this.request<void>("POST", withQuery(routes.AUTH_LOGOUT, { all: all ? "true" : undefined }));
  }
  /** Where "Sign in with …" sends the browser; `returnTo` is where the callback lands. */
  loginUrl(provider: "github" | "google", returnTo: string): string {
    return this.url(withQuery(provider === "github" ? routes.AUTH_GITHUB_LOGIN : routes.AUTH_GOOGLE_LOGIN, { return_to: returnTo }));
  }
  /** The routes of one org (`{org}` = its slug). */
  org(slug: string): OrgApi {
    return new OrgApi(this, slug);
  }
}

/** `/api/v1/orgs/{org}/…` and the org's SSE streams. */
export class OrgApi {
  readonly api: MageApi;
  readonly org: string;

  constructor(api: MageApi, org: string) {
    this.api = api;
    this.org = org;
  }

  #p(template: string, args: Record<string, string> = {}): string {
    return fill(template, { org: this.org, ...args });
  }

  // ---- tickets and causes
  tickets(query: TicketListQuery = {}) {
    return this.api.request<Paged<TicketSummary>>("GET", withQuery(this.#p(routes.TICKETS), { ...query }));
  }
  ticket(ticket: TicketId) {
    return this.api.request<TicketDetail>("GET", this.#p(routes.TICKET, { ticket }));
  }
  retriage(ticket: TicketId) {
    return this.api.request<JobQueued>("POST", this.#p(routes.TICKET_RETRIAGE, { ticket }));
  }
  setTarget(ticket: TicketId, body: SetTargetRequest) {
    return this.api.request<TicketTarget>("PUT", this.#p(routes.TICKET_TARGET, { ticket }), body);
  }
  resolveTicket(ticket: TicketId, body: ResolveTicketRequest) {
    return this.api.request<Resolution>("POST", this.#p(routes.TICKET_RESOLVE, { ticket }), body);
  }
  diagnose(ticket: TicketId, cause: CauseId, body: StartHarnessRunRequest) {
    return this.api.request<RunStarted>("POST", this.#p(routes.CAUSE_DIAGNOSE, { ticket, cause }), body);
  }
  fixBrief(ticket: TicketId, cause: CauseId, query: FixBriefQuery = {}) {
    return this.api.request<FixBriefView>("GET", withQuery(this.#p(routes.CAUSE_FIX_BRIEF, { ticket, cause }), { ...query }));
  }
  fix(ticket: TicketId, cause: CauseId, body: StartHarnessRunRequest) {
    return this.api.request<RunStarted>("POST", this.#p(routes.CAUSE_FIX, { ticket, cause }), body);
  }
  openPullRequest(ticket: TicketId, cause: CauseId) {
    return this.api.request<JobQueued>("POST", this.#p(routes.CAUSE_PULL_REQUEST, { ticket, cause }));
  }

  // ---- runs and artifacts
  runs(query: RunListQuery = {}) {
    return this.api.request<Paged<RunSummary>>("GET", withQuery(this.#p(routes.RUNS), { ...query }));
  }
  run(run: RunId) {
    return this.api.request<RunDetail>("GET", this.#p(routes.RUN, { run }));
  }
  runEvents(run: RunId, query: RunEventsQuery = {}) {
    return this.api.request<RunEventPage>("GET", withQuery(this.#p(routes.RUN_EVENTS, { run }), { ...query }));
  }
  steer(run: RunId, body: SteerRequest) {
    return this.api.request<CommandQueued>("POST", this.#p(routes.RUN_STEER, { run }), body);
  }
  cancel(run: RunId) {
    return this.api.request<RunStatusView>("POST", this.#p(routes.RUN_CANCEL, { run }));
  }
  runArtifacts(run: RunId) {
    return this.api.request<ArtifactList>("GET", this.#p(routes.RUN_ARTIFACTS, { run }));
  }
  artifact(artifact: ArtifactId) {
    return this.api.request<ArtifactView>("GET", this.#p(routes.ARTIFACT, { artifact }));
  }
  /** Bytes of an artifact (`<video src>`, downloads); the server answers `Range` requests. */
  artifactUrl(artifact: ArtifactId): string {
    return this.api.url(this.#p(routes.ARTIFACT_CONTENT, { artifact }));
  }
  /** The first `maxBytes` of a text artifact (logs, scripts, patches), decoded as UTF-8. */
  async artifactText(artifact: ArtifactView, maxBytes: number): Promise<ArtifactText> {
    const partial = artifact.bytes > maxBytes;
    const res = await this.api.send("GET", this.#p(routes.ARTIFACT_CONTENT, { artifact: artifact.id }), undefined, partial ? { range: `bytes=0-${maxBytes - 1}` } : {});
    const bytes = new Uint8Array(await res.arrayBuffer());
    const kept = bytes.length > maxBytes ? bytes.subarray(0, maxBytes) : bytes;
    // `stream: true` leaves a multi-byte character cut by the range out instead of printing U+FFFD.
    const text = new TextDecoder("utf-8").decode(kept, { stream: partial });
    return { text, truncated: partial };
  }
  /** A 10-minute trace-viewer URL on the trace origin (`Trace` artifacts only). */
  shareArtifact(artifact: ArtifactId) {
    return this.api.request<ShareLink>("POST", this.#p(routes.ARTIFACT_SHARE, { artifact }));
  }

  // ---- repos, recipes, deployments, customers
  repos() {
    return this.api.request<RepoList>("GET", this.#p(routes.REPOS));
  }
  repo(repo: RepoId) {
    return this.api.request<RepoDetail>("GET", this.#p(routes.REPO, { repo }));
  }
  installationRepos() {
    return this.api.request<InstallationRepoList>("GET", this.#p(routes.GITHUB_REPOSITORIES));
  }
  connectRepo(body: ConnectRepoRequest) {
    return this.api.request<Repo>("POST", this.#p(routes.REPOS), body);
  }
  disconnectRepo(repo: RepoId) {
    return this.api.request<void>("DELETE", this.#p(routes.REPO, { repo }));
  }
  indexVersion(repo: RepoId, body: IndexVersionRequest) {
    return this.api.request<JobQueued>("POST", this.#p(routes.REPO_VERSIONS, { repo }), body);
  }
  learn(repo: RepoId, body: StartLearnRequest) {
    return this.api.request<RunStarted>("POST", this.#p(routes.REPO_LEARN, { repo }), body);
  }
  recipes(repo: RepoId) {
    return this.api.request<RecipeHistory>("GET", this.#p(routes.REPO_RECIPES, { repo }));
  }
  saveRecipe(repo: RepoId, body: SaveRecipeRequest) {
    return this.api.request<SavedRecipe>("PUT", this.#p(routes.REPO_RECIPE, { repo }), body);
  }
  verifyRecipe(repo: RepoId, body: VerifyRecipeRequest) {
    return this.api.request<RunStarted>("POST", this.#p(routes.REPO_RECIPE_VERIFY, { repo }), body);
  }
  deployments(repo: RepoId) {
    return this.api.request<DeploymentList>("GET", this.#p(routes.REPO_DEPLOYMENTS, { repo }));
  }
  setDeployment(repo: RepoId, environment: string, body: SetDeploymentRequest) {
    return this.api.request<Deployment>("PUT", this.#p(routes.REPO_DEPLOYMENT, { repo, environment }), body);
  }
  customers() {
    return this.api.request<CustomerList>("GET", this.#p(routes.CUSTOMERS));
  }
  createCustomer(body: CustomerInput) {
    return this.api.request<Customer>("POST", this.#p(routes.CUSTOMERS), body);
  }
  updateCustomer(customer: CustomerId, body: CustomerInput) {
    return this.api.request<Customer>("PUT", this.#p(routes.CUSTOMER, { customer }), body);
  }

  // ---- settings
  settings() {
    return this.api.request<OrgSettings>("GET", this.#p(routes.SETTINGS));
  }
  saveSettings(body: OrgSettings) {
    return this.api.request<OrgSettings>("PUT", this.#p(routes.SETTINGS), body);
  }
  modelSettings() {
    return this.api.request<ModelSettingsView>("GET", this.#p(routes.SETTINGS_MODEL));
  }
  saveModelSettings(body: ModelSettingsUpdate) {
    return this.api.request<ModelSettingsView>("PUT", this.#p(routes.SETTINGS_MODEL), body);
  }
  /** Write-only: the key goes in, display-safe metadata comes out. */
  setModelKey(body: SetModelKeyRequest) {
    return this.api.request<ModelKeyMeta>("PUT", this.#p(routes.SETTINGS_MODEL_KEY), body);
  }
  deleteModelKey() {
    return this.api.request<void>("DELETE", this.#p(routes.SETTINGS_MODEL_KEY));
  }
  integrations() {
    return this.api.request<IntegrationList>("GET", this.#p(routes.INTEGRATIONS));
  }
  /** The provider URL to send the browser to. */
  connectIntegration(kind: IntegrationKind) {
    return this.api.request<ConnectIntegrationResponse>("POST", this.#p(routes.INTEGRATION_CONNECT, { kind: INTEGRATION_SLUG[kind] }));
  }
  revokeIntegration(kind: IntegrationKind, externalId: string) {
    return this.api.request<void>("DELETE", this.#p(routes.INTEGRATION, { kind: INTEGRATION_SLUG[kind], external_id: externalId }));
  }
  members() {
    return this.api.request<MemberList>("GET", this.#p(routes.MEMBERS));
  }
  invite(body: InviteRequest) {
    return this.api.request<InviteView>("POST", this.#p(routes.MEMBER_INVITES), body);
  }
  withdrawInvite(invite: string) {
    return this.api.request<void>("DELETE", this.#p(routes.MEMBER_INVITE, { invite }));
  }
  setRole(user: UserId, body: SetRoleRequest) {
    return this.api.request<MemberView>("PUT", this.#p(routes.MEMBER, { user }), body);
  }
  removeMember(user: UserId) {
    return this.api.request<void>("DELETE", this.#p(routes.MEMBER, { user }));
  }
  audit(query: AuditQuery = {}) {
    return this.api.request<AuditPage>("GET", withQuery(this.#p(routes.AUDIT), { ...query }));
  }
  usage(query: UsageQuery) {
    return this.api.request<UsageReport>("GET", withQuery(this.#p(routes.USAGE), { ...query }));
  }

  // ---- live updates
  feedUrl(): string {
    return this.api.url(this.#p(routes.EVENTS_FEED));
  }
  /** Run events with `seq > after` (first connect; reconnects send `Last-Event-ID`). */
  runEventsUrl(run: RunId, after: number): string {
    return this.api.url(withQuery(this.#p(routes.EVENTS_RUN, { run }), { after }));
  }
}
