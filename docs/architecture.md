# aho cloud — architecture

Customers build with aho (in our web UI, or locally with the aho CLI) or with anything else, then
hand us one document — the deploy manifest — and we run the project under their domain. The
contract is stack-agnostic; aho integration is additive (drafting, provenance, in-session tools).

```
React console ──► control-plane (axum) ──► ledger (aho-log)     per-project release ledger
     │                 │   │   │
     │ aho rpc / WS    │   │   └──► builder ──► BuildKit / pack  → OCI digest
     ▼                 │   └──────► runtime-agent ──► Firecracker microVMs (+ cloud-guest init)
session-host ──► ahoc (aho-sdk + bundle-cloud: aho-lab + cap-deploy + cap-edge-contract)
                       └──────────► domains (TXT verify, ACME, certs, tombstones)
internet ──► edge (TLS, routes, auth, X-Principal-JWT, webhook verifiers) ──► runtime
```

## Crates

| crate | role |
|---|---|
| `contract` | manifest v1, validation (typed refusals), surface diff, release policy, standing orders, `edge.toml`, principal claims, ledger bodies, drafts. No I/O, no aho. |
| `api` | control-plane routes and bodies; typed client (`client` feature); the exec frame protocol (`exec`) shared by terminals, the runtime agent and `cloud-guest`. |
| `ledger` | per-project release ledger on `aho-log` (hash chain, fold, invariants). |
| `control-plane` | the API server: auth (device flow, passkeys, device keys), orgs, projects, releases, approvals, domains, secrets, workspaces, sandboxes (records, write-only env vars, auto-stop, public-port sites). |
| `builder` | source → OCI image (Dockerfile / buildpack / command) via BuildKit and `pack`. |
| `runtime-agent` | per-host daemon: microVM lifecycle, cgroup ceilings, health checks, maintenance/sleep; `cloud-guest` PID 1 inside each VM, which also serves exec sessions (PTY or pipes) and lists/reads the workspace. Sandboxes are a second kind of desired state next to deployments. `--backend docker` runs replicas as containers for local development (no isolation, no egress enforcement). |
| `domains` | domain bindings: TXT tokens, DNS checks, grace/tombstone, ACME, certificate store, RDAP expiry. |
| `edge` | reverse proxy: route modes, edge login (password / magic link / OIDC), principal JWT minting, JWKS, webhook signature verifiers, tombstone pages. |
| `session-host` | Entry A: runs `ahoc` per workspace and bridges aho rpc (NDJSON) to a WebSocket; serves the workspace's file tree, files and deploy package to the control plane. For sandboxes: the public terminal socket (single-use tokens) and sandbox chat, both relayed to the runtime agent's exec socket. |
| `draft` | aho session log → manifest draft with per-field evidence. |
| `importers` | repo files (Dockerfile, Procfile, compose, fly.toml, render.yaml, app.yaml, package.json) → draft. |
| `workdir` | a project directory as the cloud sees it: the deterministic source archive, the file tree and file reads (same walk: `.gitignore` honored, `.git`/`.aho` pruned, symlinks never followed or escaping), the attesting aho session log, and the draft (`deploy.json`'s declared fields, then repo files, then the session, merged by strength). Shared by `ahocloud` and `session-host`. |
| `cap-deploy` | aho capability: `cloud_draft`, `cloud_propose` tools; `Deploy/*` entries. |
| `cap-edge-contract` | aho capability: prompt section for the principal contract, `edge_routes`, `edge_verifier` tools. |
| `bundle-cloud` | `cloud()` composition = aho-lab + the caps; `ahoc` binary (aho-sdk rpc). |
| `ahocloud` | the customer CLI: `login`, `manifest init|check`, `deploy`, `approve`, `releases`, `rollback`, `domains`, `sandbox`, `ssh`. |
| `web/` | React console (Vite, TS); types generated from `contract` and `api`, aho `protocol.ts` for sessions. |

## Rules

- **The model can propose, never approve.** Agents and CI append `release_proposed`; activation needs
  signed human approvals meeting the org `ReleasePolicy` for the surface diff against live. Every
  quorum is capped at the org's member count (recorded in each project ledger as
  `members_changed` at project creation and on every membership change; never below 1); an
  owner is still required where the policy says so, so a one-person org's owner signs once per stage.
- **The cloud rebuilds.** Local builds are never trusted; the manifest names source, the builder
  produces the digest.
- **Secrets are names in documents, values only in the secret store and the runtime.** A value
  found in any uploaded session log taints the release until rotated.
- **Narrowing is free, widening is signed** — for releases and rollbacks (surface diff against live).
- **Authn at the edge, authz in the app.** Apps verify `X-Principal-JWT` against the project JWKS.
- Generated TypeScript (`web/src/gen/*.ts`) and the manifest JSON Schema (`schema/manifest.v1.json`)
  are checked by tests; `UPDATE_GENERATED=1 cargo test` rewrites them.

## Entry points

- **Entry A (web UI):** console → `POST /workspaces` → session-host starts `ahoc` in a VM world →
  browser speaks aho rpc over WebSocket. The agent calls `cloud_draft` / `cloud_propose`.
  "Deploy to preview" (`POST /v1/projects/{p}/workspace/deploy`) has the session host pack the
  caller's own workspace with its aho session log (the one the workspace resumes, else the
  newest; one that does not verify refuses the package); the control plane re-hashes the archive
  and re-verifies the log (stored and recorded exactly like uploads, so the release is
  `Attested`), forces `source` to the archive, and runs the `POST releases` pipeline with the
  request's manifest, else the workspace's `deploy.json`, else answers the draft to confirm and
  send back.
- **Entry B (local):** stock `aho` builds; `ahocloud login` binds a device key; `ahocloud deploy`
  drafts from `.aho/sessions/*.jsonl`, uploads source archive + session log, proposes.
- **No aho:** `ahocloud manifest init` (importers) or a hand-written `deploy.json`; CI calls
  `POST /v1/projects/{p}/releases` with `Unattested` provenance.

## Sandboxes

A sandbox is a VM with aho and common toolchains (`images/sandbox`), for coding without deploying.
It shares the project slug namespace, belongs to an org (every member may use it), and keeps one
persistent disk at `/workspace`; everything else resets on restart. The home directory is on that
disk (`/home/aho` → `/workspace/.aho/home`; `~/.aho` stays a real directory because aho's
credential store refuses a symlinked one). A new disk starts as a copy of the image's `/workspace`:
Docker named-volume semantics, which `cloud-guest` reproduces on microVMs.

```
browser xterm / ahocloud ssh ─WS─► session-host /terminal/{s}   (single-use token, 60 s, bound to the exec request)
console chat                 ─WS─► session-host /sandbox-rpc/{c} (aho rpc, same rules as workspace chat)
                                        │ WS + service token
                                        ▼
                     runtime-agent SANDBOX_EXEC ─► Vmm::exec ─► cloud-guest exec server
                                        (Firecracker: vsock port 53; Docker: `docker exec -i <c> cloud-guest exec`)
```

- **aho runs inside the VM.** Chat is `aho --project /workspace rpc` started as an exec session, so
  the agent process, its tools and its files are all inside the VM; the session host only relays
  NDJSON. The model key is a sandbox env var (`ANTHROPIC_API_KEY`), stored encrypted, never
  returned, and in the VM's environment; egress is limited to the sandbox's allowlist (default:
  model providers, package registries, GitHub).
- **Exec protocol** (`cloud_api::exec`): one request, then binary frames (stdin, resize, close-stdin
  in; stdout, stderr, exit, error out). Browsers never send the request: the control plane decides
  it (default `bash -l` in `/workspace`) and the session host binds it to the token.
- **Files** are `cloud-guest files|file` exec sessions in the VM, the same walk as workspaces.
- **Lifecycle:** create boots; stop keeps the disk; delete destroys it; settings changes restart a
  running VM; the control plane stops VMs `auto_stop_hours` after each start. A public port is an
  edge site at `<slug>.<preview domain>` behind email sign-in restricted to org members.

## Not built

- **Pre-release conformance crawler** (checking that the edge enforces the declared route modes
  against a staging deployment): not implemented. The edge's own integration tests cover header
  stripping, every route mode and webhook verification.
- **`edge.toml` in the surface diff:** route-mode changes are validated but not classified as
  widening/narrowing.
- **`git push` remote:** CI proposes through the API instead.
- **Sandbox model keys stay in the VM:** a prompt-injected agent can read its own key. The planned
  fix (placeholder values swapped by the egress proxy) is not built.
- **Firecracker sandboxes are untested on hardware:** the vsock exec listener compiles for Linux
  and the shared exec server is tested on macOS; only the Docker backend ran end to end.
