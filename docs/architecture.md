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
| `api` | control-plane routes and bodies; typed client (`client` feature). |
| `ledger` | per-project release ledger on `aho-log` (hash chain, fold, invariants). |
| `control-plane` | the API server: auth (device flow, passkeys, device keys), orgs, projects, releases, approvals, domains, secrets, workspaces. |
| `builder` | source → OCI image (Dockerfile / buildpack / command) via BuildKit and `pack`. |
| `runtime-agent` | per-host daemon: microVM lifecycle, cgroup ceilings, health checks, maintenance/sleep; `cloud-guest` PID 1 inside each VM. `--backend docker` runs replicas as containers for local development (no isolation, no egress enforcement). |
| `domains` | domain bindings: TXT tokens, DNS checks, grace/tombstone, ACME, certificate store, RDAP expiry. |
| `edge` | reverse proxy: route modes, edge login (password / magic link / OIDC), principal JWT minting, JWKS, webhook signature verifiers, tombstone pages. |
| `session-host` | Entry A: runs `ahoc` per workspace and bridges aho rpc (NDJSON) to a WebSocket; serves the workspace's file tree, files and deploy package to the control plane. |
| `draft` | aho session log → manifest draft with per-field evidence. |
| `importers` | repo files (Dockerfile, Procfile, compose, fly.toml, render.yaml, app.yaml, package.json) → draft. |
| `workdir` | a project directory as the cloud sees it: the deterministic source archive, the file tree and file reads (same walk: `.gitignore` honored, `.git`/`.aho` pruned, symlinks never followed or escaping), the attesting aho session log, and the draft (`deploy.json`'s declared fields, then repo files, then the session, merged by strength). Shared by `ahocloud` and `session-host`. |
| `cap-deploy` | aho capability: `cloud_draft`, `cloud_propose` tools; `Deploy/*` entries. |
| `cap-edge-contract` | aho capability: prompt section for the principal contract, `edge_routes`, `edge_verifier` tools. |
| `bundle-cloud` | `cloud()` composition = aho-lab + the caps; `ahoc` binary (aho-sdk rpc). |
| `ahocloud` | the customer CLI: `login`, `manifest init|check`, `deploy`, `approve`, `releases`, `rollback`, `domains`. |
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

## Not built

- **Pre-release conformance crawler** (checking that the edge enforces the declared route modes
  against a staging deployment): not implemented. The edge's own integration tests cover header
  stripping, every route mode and webhook verification.
- **`edge.toml` in the surface diff:** route-mode changes are validated but not classified as
  widening/narrowing.
- **`git push` remote:** CI proposes through the API instead.
