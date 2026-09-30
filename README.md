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
crates/session-host    browser ↔ aho rpc, workspace files     bin: cloud-session-host
crates/draft           aho session log → manifest draft
crates/importers       Dockerfile/Procfile/compose/fly/render/app.yaml/package.json → draft
crates/workdir         project dir → source archive, file tree, session log, draft (CLI + session host)
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

## Run locally

```
scripts/dev.sh                 # control plane :8080 · session host :8790 · console http://localhost:5173
scripts/dev.sh --with-deploy   # + BuildKit, registry, builder, runtime (Docker), edge :8443, mail catcher
scripts/dev.sh --down          # remove the deploy containers (registry and BuildKit volumes stay)
```

Both modes run the control plane in memory (accounts vanish on restart), the session host with
`ahoc` per workspace in aho's host world (forced with `--set seams.world=host`, whatever
`~/.aho/config.toml` says: the Code tab and deploys read the workspace directory on this machine;
the host world is policy-only, not a sandbox), and the console. Open the console at `localhost`,
not `127.0.0.1`: it is the WebAuthn origin for passkeys.

Without `--with-deploy`, releases stop at `build failed`; passkey accounts, orgs, projects, domains
and their DNS records, secrets, manifests, the approval card and workspaces all work.

With `--with-deploy` (needs Docker: OrbStack, Docker Desktop or colima; first start pulls
`registry:2`, `moby/buildkit` and `axllent/mailpit` and downloads a checksummed `buildctl` into the
state dir), releases are built by BuildKit into `localhost:5055`, run as containers, and served at
`http://<project>.preview.localhost:8443`.

From the console:

1. Create an organization (sidebar), then **Create → New project**.
2. In the project, chat with aho; the **Code** tab shows the workspace files as a deploy would ship
   them.
3. **Deploy to preview** packs the workspace files. With a `deploy.json` it proposes right away;
   without one it shows a draft from the files and the aho session: confirm the inferred fields and
   answer what nothing in the code decides (who can open the app, health check, owner, budget).
   Preview activates once built. Preview and staging admit only org members; with the email-link
   login, the link lands in the mail catcher at <http://localhost:8025>.
4. **Go live** adds or picks a domain (shows its DNS records), re-proposes the preview's source with
   that domain if its manifest lacks it, then asks for passkey approvals for staging and live.
   Quorums never exceed the org's member count, so a one-person org's owner approves alone. Locally
   the domain cannot verify, so live stays "waiting for DNS".

From a terminal, the same works for any directory with a `Dockerfile`:

```
ahocloud --api-url http://127.0.0.1:8080 login                       # approve the code at /device
ahocloud --api-url http://127.0.0.1:8080 -p shop deploy --dir path/to/app
ahocloud --api-url http://127.0.0.1:8080 -p shop approve <release> --stage staging   # then promote
```

Containers stand in for microVMs: no VM isolation, no egress allowlist, Docker's own memory/CPU/pids
limits. Buildpack builds need `pack`, which the script does not install. Prompts in a workspace use
your aho provider login (`~/.aho`) and spend its credits.

## External programs (production hosts)

Tests never need these; they run against in-process fakes and fake CLI scripts.

| component | needs |
|---|---|
| builder | `git`, `buildctl` (BuildKit), `pack`, an OCI registry |
| runtime-agent | Linux + KVM, `firecracker`, kernel image, `skopeo`, `umoci`, `mkfs.ext4`, `ip`, `nft`, cgroup v2 (or `--backend docker` for development) |
| control-plane | Postgres (or `--ephemeral` for development) |
| edge | ACME CA (Let's Encrypt), SMTP for magic links, OIDC client ids |
| session-host | `ahoc` (from `bundle-cloud`) and aho's VM world prerequisites |
