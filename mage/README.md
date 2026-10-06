# ahomage (Mage) v1

Support tool: learns a GitHub codebase, triages Linear tickets into likely causes with verified
`file:line@sha` evidence, reproduces them in a fork (Playwright video), writes a fix brief, fixes,
and opens a PR. Design: [docs/design.md](docs/design.md); internals: [docs/internals/](docs/internals/README.md).

v1 = phases P1–P6 (self-host mode, host executor, omp + Claude Code harnesses). Not in v1:
Firecracker VMs (`shared/vmm`), aho-cloud adapters (`ahocloud` feature), Build extension,
knowledge layer 5 (embeddings).

## Layout

| Path | What |
|---|---|
| `crates/mage-core` | Pure domain, shared ports + in-memory fakes + contract suites, wire DTOs, token codec, TS generation |
| `crates/mage-infra` | Postgres (all migrations, JobQueue, RepoStore, KnowledgeStore), blob store, GitHub App client, config, telemetry |
| `crates/mage-server` | Server role: HTTP/API, auth, intake (GitHub/Linear/Slack), Knowledge MCP, triage, runs + gates + runner protocol, fix + PRs, model gateway |
| `crates/mage-indexer` | Indexer role: git mirrors, tree-sitter, error sites, app map, history, SCIP ingest, source bundles, `/internal/v1` |
| `crates/mage-runner` | Runner role: host executor, SQLite state, omp/Claude Code harnesses, Run MCP, Playwright, verify runs, media, lease loop |
| `crates/mage-app` | The `mage` binary (CLI + composition) and the layer-guard test |
| `web/`, `web-app/` | `@mage/web` pages and the standalone shell (React; shared design system in `../packages/ui`) |
| `../shared/auth` | GitHub OAuth + Google OIDC (own Cargo workspace in `../shared`) |
| `config/mage.example.toml` | Documented local-dev config for all three roles |

## Prerequisites

- Rust 1.85+ (edition 2024), Node 22+, `git`.
- **Postgres 16** (migrations use `NULLS NOT DISTINCT`), e.g.
  `docker run -d --name mage-pg -e POSTGRES_HOST_AUTH_METHOD=trust -p 55432:5432 postgres:16`.
- Runner extras: `omp` and/or `claude` on PATH (each harness is enabled only if its binary is
  found), Node + Playwright browsers, `ffmpeg`/`ffprobe` (media), Docker (repos whose recipe uses
  compose), a local Postgres for recipe databases.

## Tests

```sh
# Rust: whole workspace (Postgres-backed tests skip without DATABASE_URL)
DATABASE_URL=postgres://postgres@localhost:55432/postgres cargo test --workspace --all-features
cd ../shared && cargo test -p shared-auth && cd -          # login providers
UPDATE_GENERATED=1 cargo test -p mage-core generated_ts_is_current   # regenerate web/src/gen/mage.ts

# Web
(cd ../packages/ui && npm install && npx tsc --noEmit && npx vitest run)
(cd web && npm install && npx tsc --noEmit && npx vitest run)        # install web before web-app
(cd web-app && npm install && npx tsc --noEmit && npx vitest run && npm run build)
```

Opt-in tests (real binaries / OS features), off by default:

| Env | Enables |
|---|---|
| `MAGE_TEST_OMP=1`, `MAGE_TEST_CLAUDE=1` | Harness acceptance tests against the real CLIs (spend credits) |
| `MAGE_TEST_PLAYWRIGHT=1` | Playwright adapter against a real browser |
| `MAGE_TEST_FFMPEG=1` | mp4/GIF encoding |
| `MAGE_TEST_RUNNER_PG_URL=postgres://…` | Executor database (TEMPLATE) contract |
| `MAGE_TEST_ENFORCED=1` | Enforced runner isolation (Linux only) |

Last full run: core 326, infra 51 + 25 integration, indexer 160, server 581 + compose, runner 333 +
smoke, app 10 + layers 2, shared-auth 32; web: ui 15, mage/web 62, web-app 12, console 92 — all green.

## Run locally (`./run.sh`)

`run.sh` does everything below: starts Postgres 16 in Docker (`mage-pg`, port 55432), builds the
UI and the binary, creates the vault key, migrates, stores the GitHub OAuth secret, creates the org,
issues the runner bootstrap token, then runs `mage start all`. Re-runs skip finished setup steps.

```sh
export MAGE_GITHUB_APP_PRIVATE_KEY_FILE=~/Downloads/ahomage.private-key.pem
export MAGE_GITHUB_CLIENT_ID=Iv1.xxxxxxxxxxxxxxxx MAGE_GITHUB_APP_SLUG=ahomage
export MAGE_GITHUB_OAUTH_CLIENT_SECRET=…   MAGE_OWNER=<your-github-login>   # first run only
./run.sh                 # UI: http://localhost:8080, Ctrl-C stops all roles
./run.sh --reset         # drop the database and runner credentials, set up again
./run.sh --rebuild-web   # rebuild the embedded UI;  --no-web skips it (API only)
```

`./run.sh --help` lists the optional variables (org, container, port, database name).

### Manual steps (what `run.sh` does)

From `mage/`, with Postgres up and `MAGE_DATABASE_URL`, `MAGE_GITHUB_APP_PRIVATE_KEY` exported
(see the header of `config/mage.example.toml`, which is authoritative):

```sh
(cd web && npm ci) && (cd web-app && npm ci && npm run build)   # embed the UI (else API only)
cargo build -p mage-app && export PATH="$PWD/target/debug:$PATH"
mkdir -p ~/.config/mage && head -c 32 /dev/urandom > ~/.config/mage/dev.key && chmod 600 ~/.config/mage/dev.key
C=config/mage.example.toml
mage --config $C migrate
mage --config $C admin secret put platform github/oauth_client_secret --stdin
mage --config $C admin org create hoop --name Hoop --owner <github-login>
(umask 077 && mage --config $C admin runner-token create dev-runner > ~/.config/mage/runner-bootstrap)
mage --config $C start all        # http://localhost:8080
```

`[server.intake] mode = "webhook"` (default) requires the GitHub, Linear and Slack webhook secrets in
the vault; `mode = "poll"` needs none and mounts no `/hooks`. Production runs one role per process
with one config file each (`mage start server|indexer|runner`); `start all` refuses
`environment = "production"`.

## Validation checklist (v1 phases)

1. **P1 knowledge + triage**: connect a repo (Repos → Connect) → indexer layers reach Ready →
   a Linear issue with the configured label becomes a ticket → triage run (omp) → causes with
   verified citations → Linear comment.
2. **P2 executor**: onboarding wizard → Learn run proposes `.mage/env.toml` → Boot gate passes.
3. **P3 diagnose**: Run diagnostic on a cause → repro script → Repro gate on a fresh fork →
   video/GIF/trace on the Run page → fix brief.
4. **P4 GCP runner**: same runner config with `isolation = "enforced"` on Linux, egress firewall.
5. **P5 fix**: Let Mage fix → Fix gate (script passes, signature absent, tests pass) → patch.
6. **P6 PR**: Open PR (Fix App configured) → draft PR with before/after GIFs and mp4.

## Deviations from the docs

- Git access uses the `git` CLI everywhere (no gix); tokens only via `GIT_CONFIG_*` env, never argv.
- Artifact names are flat lowercase (`[a-z0-9][a-z0-9._-]{0,127}`): `explore-3.video.webm`,
  SCIP indexes as kind `scip_index` named `<language>.scip`.
- "Index a version" from the UI accepts only the default branch or a tag: `VersionReason` has no
  ticket-less on-demand variant yet.
- Unverified external behavior (marked `[unverified]` in docs, guarded by config): Claude Code
  through the model gateway (`[server.harness.claude_code] gateway_verified`), GitHub
  user-attachments upload with a GitHub App token (PRs fall back to Mage artifact links),
  Vertex `count_tokens` (404).
- Each area's remaining deviations are listed in its module docs and in the internals docs'
  "Open questions".
