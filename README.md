# aho cloud

Build a project with aho (in the web console or locally) or with anything else, hand over one
document — the deploy manifest (`deploy.aho.cloud/v1`) — and aho cloud builds it, gets it approved
by humans, and runs it under your domain behind an edge that owns TLS and login.

Design and crate map: [docs/architecture.md](docs/architecture.md).

## Layout

```
crates/contract        manifest, validation, surface diff, policy, edge.toml, principal claims (no aho, no I/O)
crates/api             control-plane routes + bodies, internal service bodies, typed client
crates/ledger          per-project release ledger on aho-log
crates/control-plane   the API server                         bin: cloud-control-plane
crates/builder         source → OCI image (BuildKit, pack)    bin: cloud-builder
crates/runtime-agent   Firecracker microVMs, egress proxy     bins: cloud-runtime-agent, cloud-guest
crates/domains         domain bindings, DNS, ACME, certs, RDAP
crates/edge            TLS reverse proxy, edge login, JWT     bin: cloud-edge
crates/session-host    browser ↔ aho rpc bridge               bin: cloud-session-host
crates/draft           aho session log → manifest draft
crates/importers       Dockerfile/Procfile/compose/fly/render/app.yaml/package.json → draft
crates/cap-deploy      aho capability: cloud_draft, cloud_propose
crates/cap-edge-contract  aho capability: edge contract prompt, edge_routes, edge_verifier
crates/bundle-cloud    aho-lab + the caps                     bin: ahoc
crates/ahocloud        customer CLI                           bin: ahocloud
web/                   React console
schema/manifest.v1.json  published JSON Schema (generated)
```

aho itself is a path dependency (`../aho`) and is never modified: cloud features are capability
crates composed in `bundle-cloud`.

## Commands

```
cargo test --workspace                              # all Rust tests
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
UPDATE_GENERATED=1 cargo test -p cloud-contract -p cloud-api   # rewrite web/src/gen/*.ts + schema
cd web && npm install && npx tsc --noEmit && npx vitest run && npx vite build
```

`PgStore` tests run only with `DATABASE_URL` set; without it they print a skip reason.

## Run locally (no deploy)

```
scripts/dev.sh        # console http://localhost:5173 · control plane :8080 · session host :8790
```

Starts the control plane (in memory: accounts vanish on restart), the session host running `ahoc`
per workspace in aho's host world, and the console. Builder and runtime are not started, so
releases stop at `build failed`. Everything before that works: passkey accounts (open the console
at `localhost`, not `127.0.0.1` — it is the WebAuthn origin), orgs, projects, domains and their DNS
records, secrets, manifests, the approval card, workspaces with a live aho session, and the CLI:

```
ahocloud --api-url http://127.0.0.1:8080 login          # approve the code at /device
ahocloud --api-url http://127.0.0.1:8080 -p shop deploy --dir path/to/app
```

Prompts in a workspace use your aho provider login (`~/.aho`) and spend its credits.

## External programs (production hosts)

Tests never need these; they run against in-process fakes and fake CLI scripts.

| component | needs |
|---|---|
| builder | `git`, `buildctl` (BuildKit), `pack`, an OCI registry |
| runtime-agent | Linux + KVM, `firecracker`, kernel image, `skopeo`, `umoci`, `mkfs.ext4`, `ip`, `nft`, cgroup v2 |
| control-plane | Postgres (or `--ephemeral` for development) |
| edge | ACME CA (Let's Encrypt), SMTP for magic links, OIDC client ids |
| session-host | `ahoc` (from `bundle-cloud`) and aho's VM world prerequisites |
