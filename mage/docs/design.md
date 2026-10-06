# ahomage — design (working draft)

ahomage ("Mage" for short) is a support tool. It learns a GitHub codebase and how to run it, lists
likely causes for each Linear ticket with code evidence, and on **Run diagnostic** tries to
reproduce the problem, returning **reproduced / not reproduced / inconclusive** with a re-runnable
repro script, video, trace, and logs. It starts internal and becomes a public product for other
companies.

Implementation internals (ports and adapters, per part): [internals/README.md](internals/README.md).
Code location, self-host vs aho-cloud modes, frontend packages: [internals/deployment.md](internals/deployment.md).

## Decisions

| Topic | Decision |
|---|---|
| Harness | Two initial harnesses: **oh-my-pi (omp)** and **Claude Code**, both external processes driven over stdio by a Rust adapter. aho harness added later behind the same adapter. |
| v1 execution | Code runs on a **runner host** next to the harness. No sandbox. Trusted repos only (hoop's own). |
| v2 execution | Firecracker microVMs via `shared/vmm` (extracted from aho-cloud's `runtime-agent`; Docker backend in dev). On aho-cloud, Mage can use the aho-cloud runtime-agent directly. |
| Machines | Testing: Mac (arm64; v2 VMs via the Docker backend of `shared/vmm`). Production: GCP, x86_64, nested virtualization (Intel N2/C3 or AMD N4D). Self-hosters: any Linux with KVM for v2 runners. |
| Storage | Postgres (metadata, symbols, graph, history, memory) + pgvector + blob store (GCS on our cloud; S3/GCS/Azure/local disk for self-host via `object_store`). |
| Retrieval | Agentic search (harness greps/reads the worktree at the right sha) + Knowledge MCP for what grep can't answer. |
| Trust boundary | Control plane holds all secrets; runner host / sandbox holds none. |
| Language | Rust for all Mage backend code (server, indexer, runner). The UI is React + TypeScript (shared with aho-cloud). External runtimes are not Mage code: the harnesses (omp, Claude Code), Playwright, SCIP indexers. |
| Browser repro | **Playwright** (Node runtime on the runner): repro scripts, video, trace. |
| Media in PRs and Linear | Runner converts recordings to **mp4** (≤ 10 MB) + **GIF** (before/after). PR body embeds GIFs and the mp4 via GitHub user-attachments uploads; the Linear card gets the mp4 via Linear's `fileUpload`. Fallback: link to the Mage artifact page. |
| Audience | Internal first; public product for other companies later. Public launch requires the v2 sandbox (third-party code) and multi-tenancy. |
| Fix path | Staged: (1) diagnosis + **fix brief** for the user's own LLM, (2) Mage fixes in a fork, gated, (3) Mage opens the PR. |
| Model credentials | Local: harness logins (omp / Claude Code). Production: Anthropic API key and Vertex. |
| Control plane hosting | GCP, same project/region as runners: server role on Cloud Run, Postgres on Cloud SQL (pgvector), GCS; indexer and runner roles on Compute Engine VMs (local disk for mirrors/workspaces). Local dev: all on the Mac with Postgres in Docker. |
| Binary | One binary `mage`, one role per process: `mage start server`, `mage start indexer`, `mage start runner`. |
| Public model credentials | Tenants bring their own key (BYOK) initially. |
| Code location | Inside the aho-cloud repo, folder `mage/`. aho is a git dependency (aho harness only). Mage never depends on aho-cloud crates; integrations are optional adapters (feature `ahocloud`). |
| Deployment modes | **Self-hosted** (no aho-cloud) and **on aho-cloud**, same binary. Self-hosters get the binary and a container image; deploy templates for several providers later. |
| Login | GitHub OAuth and Google (OIDC) in Mage; aho-cloud adds the same providers next to passkeys. On aho-cloud, Mage trusts the edge `X-Principal-JWT`. |
| Frontend | React + TypeScript with aho-cloud's design system (`packages/ui`). Mage pages (`mage/web`) mount in the aho-cloud console and in a standalone Mage shell (`mage/web-app`). |

## Planes

- **Control plane** (trusted, long-lived, holds secrets): UI, API, Linear/Slack intake, triage,
  Knowledge MCP, run manager, gates, artifact store, resolution memory.
- **Data plane** (untrusted, disposable, no secrets): v1 runner host; v2 Firecracker microVMs
  (`shared/vmm` on runner hosts, or aho-cloud runtime-agent).

## User flow

1. Linear webhook (+ linked Slack thread) → ticket normalized, including the customer's deployed sha.
2. Triage runs immediately → ranked likely causes, each with `file:line@sha` evidence.
3. Support opens the ticket in Mage, clicks **Run diagnostic** on a cause.
4. Run manager forks a clean environment at the customer's sha; harness seeds data and writes a repro.
5. Harness submits a result; the repro gate re-runs only the script in a second clean fork and
   records video there.
6. Verdict + artifacts shown in Mage, summarized to Linear and Slack.
7. On resolve: root cause, fix PR, repro script saved to resolution memory.

## Interfaces that make v1 → v2 a swap

**Harness adapter**: `start(run_spec)`, `events()`, `steer(text)`, `abort()`, structured `result`
via a `submit_result` tool. aho's rpc (`session.prompt/steer/subscribe/abort`) already matches.

**Harness integration** (both harnesses get the same Mage tools through MCP):

| | oh-my-pi | Claude Code |
|---|---|---|
| Launch | `omp --mode rpc --no-ui --session-dir <run dir>` | `claude -p --input-format stream-json --output-format stream-json --verbose` |
| Protocol | JSONL over stdio: `ready`, `prompt`, `steer`, `abort`, events, `prompt_result`, `session_settled` | stream-json over stdio |
| Mage tools | Knowledge MCP + Run MCP (host tools via `set_host_tools` also possible) | Knowledge MCP + Run MCP via `--mcp-config` |
| Built-in tools, v1 | Allowed; cwd = run workspace; process runs as the `mage-agent` OS user, cleared env | Allowed via `--allowedTools`; same user/cwd/env rules |
| Built-in tools, v2 | `--no-tools` / `--tools`; all execution through Run MCP into the VM | `--disallowedTools` for built-ins; all execution through Run MCP |
| Steer / abort | `steer` / `abort` commands | Extra user message on stdin (interrupt semantics to verify) / signal the process |
| Done | `prompt_result`, then `session_settled` | `result` message |
| Result | `submit_result` on Run MCP | `submit_result` on Run MCP |
| Model credentials | omp `auth-gateway` on the control plane; runner holds only a gateway bearer token | API key or Vertex (`CLAUDE_CODE_USE_VERTEX=1`); `ANTHROPIC_BASE_URL` → gateway untested |
| Subagents | `set_subagent_subscription` for progress/events | Task tool events in the stream |

**Run MCP** (served by the runner role, one instance per run, scoped to one fork): `exec`, `read`,
`write`, `list`, `playwright_run` (Mage controls video/trace output), `http_request`, `logs`,
`db_query` (test DB only), `submit_result`. The v2 aho executor sits behind the same tools.

**Executor**:

| Op | v1 host | v2 aho |
|---|---|---|
| `prepare(repo, sha, recipe)` | Prepared workspace: clone at sha, deps installed, boot verified | Learn VM → frozen base image |
| `fork(base)` | CoW copy (APFS `cp -c` / reflink / overlayfs) + Postgres `TEMPLATE` DB + assigned ports | qcow2 overlay (L1) / memory restore (L2) |
| `exec` / `read` / `write` | `mage-agent` OS user, cleared env, process group | Guest agent protocol |
| `browser` / `http` | Playwright on host against run ports | Playwright in guest |
| `record` | Playwright video + trace | RFB recording from outside the VM + trace |
| `destroy` | Kill process group, drop DB, delete workspace | Delete overlay + aho-net session |

**Recipe** `.mage/env.yaml`: build, services, health checks, fixtures, test user, fake external
services, named ports. Workspace-relative, no host-specific paths — same file drives v1 and v2.

## Gates (in the run manager, harness-independent)

- **Evidence gate** (triage): every cited `file:line@sha` exists in the index.
- **Boot gate** (learn): recipe accepted only after a fresh fork boots and health checks pass.
- **Repro gate** (diagnose): "reproduced" only if the repro script alone fails the same way in a
  second clean fork. Neutralizes agent edits to app code; video comes from this run.

## v1 rules (no sandbox)

- Runner host is a separate machine from the control plane.
- Agent commands run as a separate OS user with a cleared environment.
- Fake-keys env only; egress firewall on the runner VM (GCP VPC: deny all, allow registries).
- One diagnostic at a time per repo until fork isolation is proven.
- Trusted repos only; third-party repos wait for v2.

## Knowledge (Knowledge MCP)

"Learning" = indexes + summaries + memory, never model training. Indexed shas: default branch head,
release tags, deployed shas, on-demand ticket shas. Incremental by git blob hash; layers become
available progressively and each tool reports which layer answered.

| Layer | Content |
|---|---|
| 1 Files + text | Paths, language, blob hash; grep on worktree |
| 2 Symbols + refs | tree-sitter definitions; SCIP precise refs where available |
| 3 Error/log strings | Throw/log/HTTP-status literals and i18n catalogs → templated patterns → site → enclosing fn → routes |
| 4 App map | `Page → Component → Endpoint → Handler → Table/Job/Flag` via framework extractors |
| 5 Semantic | Symbol-level chunks, embeddings, bottom-up summaries, glossary (lower priority) |
| 6 History | Diffs between shas, blame, PRs, Linear links |
| + | Codebase brief (generated), recipe, CI, docs, resolution memory |

MCP rules: read-only, per-run token scoped to repo/sha, every response carries `sha`, citations,
and `layer`; bounded, paginated. Tools: `repo_overview`, `search_code`, `read_file`, `find_symbol`,
`references`, `call_graph`, `lookup_error`, `app_map`, `route_info`, `explain`, `glossary`,
`changes_between`, `blame`, `similar_tickets`, `run_recipe`.

Triage retrieval order: exact error match → name/route/glossary match → semantic → graph expansion
→ history boost (changes since last-good sha) → similar tickets → model rerank → evidence gate.

Quality: recall@k of true root-cause file on resolved tickets; `lookup_error` hit rate on real log
lines; app-map coverage; push-to-ready latency per layer.

## Fork (v2, Firecracker via `shared/vmm`)

- **L1 image fork**: `prepare` produces a base OCI image (repo@sha + recipe build, via the runner
  or aho-cloud's builder); each fork is a new microVM whose disk is created from that image.
  Per-fork network and egress allowlist; the VM manager owns cleanup.
- **L2 snapshot fork**: Firecracker snapshot of the running prepared VM (memory + device state +
  disk), restored per fork. Snapshots are tied to Firecracker version, kernel, CPU template, and
  memory size. After restore: clock resync, entropy reseed, new MAC/IP, new exec session.
  Not in aho-cloud today; added to `shared/vmm`.
- Previous plan (aho `world-vm` with qcow2 overlays) is superseded; aho stays only as the future
  aho harness.

## Rust stack

Cargo workspace in `aho-cloud/mage/`, every crate ports-and-adapters: `mage-core` (pure domain,
shared ports, wire DTOs), `mage-infra` (shared driven adapters), role crates `mage-server`,
`mage-indexer`, `mage-runner`, and `mage-app` — the **one binary `mage`** (details in
[internals/README.md](internals/README.md)). Shared with aho-cloud: `shared/{vmm,auth,tsgen}` and
`packages/ui` ([internals/deployment.md](internals/deployment.md)). Roles chosen at start
(`mage start all` runs every role in one process for local dev):

| Command | Role |
|---|---|
| `mage start server` | Control plane: API, UI, Linear/Slack webhooks, triage, run manager + gates, Knowledge MCP |
| `mage start indexer` | Indexing worker, pulls jobs from the Postgres jobs table |
| `mage start runner` | Runner host daemon: host executor, Run MCP, launches harnesses; talks to the server over HTTP/JSON + SSE (mTLS) |

One binary, separate secrets: no secrets are compiled in; each role loads only its own config.
The runner role receives the server URL, its mTLS certificate, and a gateway token — never
database credentials, Linear/Slack tokens, or tenant model keys.

| Concern | Choice | Note |
|---|---|---|
| Runtime / HTTP / webhooks | tokio, axum, tower; `hmac` + `sha2` for signatures | |
| MCP server | `rmcp` (official Rust SDK), streamable HTTP | |
| Database | `sqlx` + `pgvector` crate | |
| Job queue | Postgres jobs table, `FOR UPDATE SKIP LOCKED` | No extra infra |
| Blob store | `object_store` | Local FS in dev, GCS in prod |
| Git | `gix`; `git` CLI subprocess where gix lacks (worktrees) | Blob hashes from gix |
| Parsing | `tree-sitter` + grammar crates | Clojure/CLJS grammar needs checking if indexing hoop |
| Precise refs | `scip` crate to read SCIP; language indexers run as subprocesses | Indexers are Node/Go/etc. binaries |
| Text search | ripgrep libraries (`grep-searcher`, `grep-regex`, `ignore`) in-process | Zoekt (Go) or tantivy later |
| LLM client | Triage runs as a read-only harness run (Knowledge MCP only), so v1 needs no Rust LLM client. Later, if direct calls are needed: thin client on `reqwest` or aho's `llm-anthropic-messages` crate | No official Anthropic Rust SDK |
| Linear / Slack | GraphQL via `cynic` or `graphql_client`; `slack-morphism` | |
| Host executor | `reflink-copy` (APFS clonefile / Linux reflink), `nix` (uid, process groups, signals), `tokio::process` | |
| Browser repro | Playwright (Node) on the runner, invoked through `playwright_run` | Video + trace viewer |
| Video | `ffmpeg` subprocess (Playwright webm → mp4 + GIF) | Firecracker VMs have no display; Playwright records inside the VM |
| UI | React + TypeScript (`mage/web`, `mage/web-app`), aho-cloud design system (`packages/ui`); types generated from `mage_core::wire::api` with ts-rs | Playwright trace viewer served as a static asset |
| Recipe format | Prefer `.mage/env.toml` (`toml` crate); `serde_yaml` is unmaintained | |
| Observability | `tracing`, OpenTelemetry export | |
| Quality gate | Borrow aho's: fmt, clippy `-D warnings`, layer guard between crates | |

v2 benefit: `shared/vmm` is the same code aho-cloud's runtime-agent runs, so self-hosted runners
and aho-cloud sandboxes behave the same.

## Fix path

| Stage | What Mage does | Gate | Needs |
|---|---|---|---|
| F1 Fix brief | After a reproduced diagnosis, generates a prompt for the user's LLM: root cause, `file:line@sha` evidence, repro script, suggested approach, acceptance = "repro script passes + project tests pass" | Evidence gate | Nothing new |
| F2 Mage fixes | Harness edits code in a fresh fork using the brief; patch shown in the UI | **Fix gate**: repro script now passes in a clean fork with the patch applied, and the project's tests still pass | Recipe test command |
| F3 Open PR | Opens a PR with the patch + repro script as a regression test, links Linear ticket; human review always required | Fix gate | GitHub App write scopes (contents, pull requests), requested only when F3 is enabled |

## Public product requirements (before launch)

- v2 sandbox (Firecracker) for all third-party code; v1 host runner stays internal-only
  (self-hosters decide for their own repos).
- Tenancy: `org_id` on every row, per-tenant encryption of code and artifacts, per-tenant
  retention settings.
- Integrations become installable apps: GitHub App per installation, Linear and Slack OAuth apps
  per workspace (no pasted tokens).
- Tenants bring their own model keys (BYOK) initially: stored encrypted on the control plane,
  never sent to the runner or sandbox; cost caps per run and per ticket.

## Phases

| Version | Phase | Deliverable |
|---|---|---|
| v1 | P1 | Knowledge MCP + triage + Linear/Slack + UI (no execution) |
| v1 | P2 | Host executor (prepare/fork/exec/destroy), recipe + boot gate, on Mac |
| v1 | P3 | Diagnose: harnesses + repro gate + Playwright video + **F1 fix brief** |
| v1 | P4 | Runner on one GCP VM with egress firewall; control plane on GCP |
| v1 | P5 | **F2** Mage fixes in a fork + fix gate |
| v1 | P6 | **F3** Mage opens PRs (internal repos) |
| v2 | P7 | Extract `shared/vmm` from aho-cloud runtime-agent; `exec_vm` (L1) on Mac (Docker backend) → x86 GCP KVM host; L2 snapshots |
| v2 | P8 | Public launch: third-party repos, tenancy, installable integrations, aho-cloud mode (edge login, ledger, live repro links), self-host binary + container image, aho harness adapter, prod read-only data via hoop |

## Risks

1. Auto-booting arbitrary repos fails on complex multi-service apps → onboarding wizard required.
2. Data-dependent bugs don't reproduce on fixtures.
3. Without deployed-sha mapping, every repro may target the wrong code.
4. v1 has no kernel boundary; mitigated only by separate host, separate user, cleared env, firewall.
5. VM layer maturity: aho-cloud's Firecracker backend has run only via the Docker backend, not on
   KVM hardware; sandboxes are long-lived per slug and there is no snapshot/fork yet.
6. arm64 (Mac) vs x86_64 (GCP) can hide or create bugs; base images and snapshots are per arch.
7. BYOK keys must never reach the runner, so both harnesses need a model gateway on the control
   plane (per-run token in, tenant key injected). omp has `auth-gateway`; Claude Code through a
   gateway via `ANTHROPIC_BASE_URL` is unverified — prove it before public launch.
8. Cloud Run does not terminate client mTLS by itself; runner → server mTLS needs an external
   Application Load Balancer with mTLS, or use short-lived signed runner tokens instead. Long-lived
   SSE streams on Cloud Run hit the request timeout and must reconnect.

## Open decisions

None currently.

## Revisit later

- **Graph database.** Kept Postgres (recursive CTEs, per-sha versioning via blob manifests,
  fixed MCP tools). Revisit if real queries often need variable-length paths >3 hops, CTE latency
  becomes a problem, or the agent should run ad-hoc graph queries. Keep the graph behind a
  `GraphStore` interface. Candidates: Neo4j (safe default), Memgraph (in-memory, BSL),
  FalkorDB (graph + vector, SSPL — license issue if hosted), Apache AGE (Cypher in Postgres,
  status unchecked). Avoid Kuzu (archived Oct 2025).
- **Text index.** Start with ripgrep libraries on worktrees; move to Zoekt (Go service) or tantivy
  with an ngram tokenizer (Rust) when repo/sha count grows.
- **Embeddings.** Keep only if they raise triage recall@k; market trend for code is away from them
  (Sourcegraph, Claude Code) except Cursor with a custom model.
- **aho harness adapter** as the third harness.
- **Firecracker snapshots (L2)** — not on the Mac (Docker backend); KVM hosts only.
- **Multi-host scheduler** for sandbox hosts once diagnostics queue up.
- **Prod read-only data via hoop** for data-dependent bugs.
- **Third-party repos** once the v2 sandbox is in place.
- **Licensing** of Mage (self-hosters get binaries/images only for now).
- **Deploy templates** for self-host on several providers.
