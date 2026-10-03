# aho cloud console (`web/`)

React + TypeScript console for aho cloud: passkey sign-in, device approval, organizations and
their projects, chatting with aho in a project's workspace, its code, deploying the workspace to
a preview, going live on the project's domain, release approval cards, domains, secrets, the
release ledger, and sandboxes (VMs with aho for coding without deploying: terminal, chat, files). Plain CSS (`src/styles.css`, CSS variables), no UI framework.

Node 22 or newer.

```sh
npm install
npm run dev          # Vite dev server; proxies /v1 to $AHO_CLOUD_API (default http://127.0.0.1:8080)
npm run typecheck    # tsc --noEmit
npm test             # vitest run (jsdom)
npm run build        # vite build → dist/
npm run preview      # serve dist/
```

`VITE_AHO_CLOUD_API` (build time) points the console at a control plane on another origin; by
default it calls the API on its own origin. Passkeys need the console origin to match the control
plane's configured WebAuthn origin and RP id.

## What the user sees

- **Shell** — top bar with the account menu (email, "Approve a device", log out); a sidebar with
  the org switcher (the user's orgs plus an inline "New organization" form), the **Create** menu
  ("New project", "New sandbox"), and the org's pages: Projects, Sandboxes, Team, Release policy.
- **Projects** (`/orgs/:org`, the org's home) — search (`/` focuses it) and status chips with
  counts. A project is *Live* when a release is active at live, *Preview* when one runs at
  preview or staging only, *Building* when nothing runs and its newest release is building, and
  *Idle* otherwise. Beside the list: the `ahocloud login` / `ahocloud -p <project> deploy`
  snippet, the org's sandboxes (count, state of the first five, link to all), and the org's ten
  newest ledger entries.
- **Team** / **Release policy** — members (owners add members by email) and the org's policy,
  read-only.
- **Project** (`/projects/:p`) — the aho chat on the left (workspace rpc: transcript, prompt,
  abort, tool-approval cards; a newer window taking the workspace over closes this socket with
  4000 and it does not reconnect), and tabs on the right:
  - *Code* — the files a deploy would ship (`WORKSPACE_FILES`) as a collapsible tree and a
    line-numbered viewer (`WORKSPACE_FILE`; binary and truncated files are marked). Refreshes on
    demand, 0.8 s after a burst of tool results in the chat, and every 10 s while the tab shows.
  - *Releases*, *Domains*, *Secrets*, *Activity* (the ledger).
- **Deploy to preview** — `POST WORKSPACE_DEPLOY {}`. `proposed`: the Releases tab follows the
  release (building → activating → active at preview) and links the preview URL.
  `needs_manifest`: the draft opens in the confirm screen; confirming sends `{manifest}` back;
  refusals show in place.
- **Go live** (enabled while a release is active at preview) — pick or add the domain and see its
  DNS records; if the preview release does not declare the hostname, a copy with the same
  `source` and `provenance` and the domain appended is proposed and followed to preview; then
  "Approve & go live" signs and promotes staging, then live (one passkey approval per stage; steps
  already done or not needed are skipped, so a failed run resumes). An unverified domain is shown
  as waiting for DNS.
- **Sandboxes** (`/orgs/:org/sandboxes`) — the org's sandboxes with state (starting, running,
  stopped, failed), creator, auto-stop time and public URL; re-read while any is starting. The
  empty state explains what a sandbox is. **New sandbox** asks for a slug (one namespace with
  projects), the egress list (prefilled from `DEFAULT_SANDBOX_EGRESS`, mirrored in
  `src/lib/sandbox.ts`), an optional public port and auto-stop hours (default 8, at most 72).
- **Sandbox** (`/sandboxes/:s`) — state badge, Start / Stop, Delete (confirmed), the public URL;
  tabs (Terminal and Chat stay connected while other tabs show; all but Settings offer Start
  while the sandbox is not running):
  - *Terminal* — xterm.js, loaded only by this page. `POST SANDBOX_TERMINAL {cols, rows}` with the
    fitted size, then the returned URL `?token=` as a binary WebSocket carrying exec frames
    (`src/lib/exec.ts`): keystrokes → Stdin, fit on window resize → Resize, Stdout/Stderr →
    terminal. Exit shows `[process exited with code N]`, an Error frame its message, a drop the
    close code; each offers Reconnect, and nothing reconnects on its own.
  - *Chat* — the project chat, connected through `SANDBOX_CHAT` (same takeover rule: 4000 closes
    it for good); no manifest-draft confirmation.
  - *Files* — the code panel over `SANDBOX_FILES` / `SANDBOX_FILE`.
  - *Settings* — egress, port, auto-stop (`PUT SANDBOX_SETTINGS`; a running sandbox restarts) and
    environment variables: names and fingerprints listed, values write-only (password field,
    never shown again), deletable; they apply at the next start (`ANTHROPIC_API_KEY` powers the
    aho inside).
- The release page (`/projects/:p/releases/:r`), login and device pages keep their routes.

The console remembers per user in `localStorage` the org last worked in (home redirect, sidebar
scope) and the preview URLs workspace starts reported; a URL whose host starts with the project
slug also serves as the `{project}.` template for other projects' preview links.

## Layout

- `src/gen/` — generated by `UPDATE_GENERATED=1 cargo test -p cloud-contract -p cloud-api`. Do not edit.
- `src/aho.ts` — type-only re-export of aho's `sdk/ts/src/protocol.ts` (the rpc wire types).
- `src/lib/api.ts` — typed client over `routes`/`fill`; errors are `ApiError` with the server's `code`.
- `src/lib/approval.ts` — `approvalMessage` (identical bytes to Rust `approval_message`), the
  SHA-256 passkey challenge, and canonical JSON + `blake3:` hashing for standing orders.
- `src/lib/webauthn.ts` — base64url ⇄ ArrayBuffer conversion of WebAuthn options and credentials;
  register, login, and approval assertions.
- `src/lib/rpc.ts` — `RpcClient`: `hello` handshake, request ids, event dispatch, reconnect with
  `session.subscribe{from}`.
- `src/lib/exec.ts` — the exec frame codec, byte for byte `crates/api/src/exec.rs` (kind byte +
  payload; Resize = cols, rows u16 BE; Exit = i32 BE).
- `src/lib/sandbox.ts` — sandbox defaults (mirrors of the Rust constants), egress host and env
  name rules, the settings form's check.
- `src/lib/deploy.ts` — following a release to preview (`waitForPreview`).
- `src/lib/golive.ts` — the go-live steps: bind the domain, declare it, approve and promote.
- `src/lib/status.ts`, `src/lib/filetree.ts`, `src/lib/slug.ts` — dashboard status/filter/feed,
  the file tree from `FileTree` paths, the slug rule.
- `src/lib/draft.ts`, `src/components/DraftConfirm.tsx` — draft confirmation (weak fields must be
  confirmed or corrected before submit).
- `src/components/` — shell, chat, code panel, releases, go-live modal, create-project and
  create-sandbox modals, sandbox terminal and settings, approval card, domains, secrets, ledger,
  transcript, standing orders.
- `src/pages/` — routed screens; `src/ui/` — shared bits (errors, badges, modal, dropdown, icons).
