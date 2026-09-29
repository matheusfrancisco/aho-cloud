#!/usr/bin/env bash
# Local development without the deploy half: control plane (in-memory), session host running
# `ahoc` per workspace (aho's host world), and the web console. Builder and runtime are not
# started, so proposed releases end as `build_failed`; everything before that works: passkeys,
# device login, orgs, projects, domains, secrets, manifests, drafts, the approval card, and
# Entry A workspaces talking to a real aho session.
#
#   scripts/dev.sh            # console on http://localhost:5173
#
# Env: AHO_CLOUD_DEV_DIR (state, default ./.dev), CP_PORT (8080), SH_PORT (8790), WEB_PORT (5173).
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
dev="${AHO_CLOUD_DEV_DIR:-$root/.dev}"
cp_port="${CP_PORT:-8080}"
sh_port="${SH_PORT:-8790}"
web_port="${WEB_PORT:-5173}"
target="${CARGO_TARGET_DIR:-$root/target}"

mkdir -p "$dev/control-plane" "$dev/sessions"
# Keys live with the state so a restart keeps browser sessions' workspaces readable.
keyfile="$dev/keys.env"
if [[ ! -f "$keyfile" ]]; then
  umask 077
  {
    echo "AHO_CLOUD_MASTER_KEY=$(openssl rand -hex 32)"
    echo "AHO_CLOUD_SERVICE_TOKEN=$(openssl rand -hex 32)"
    echo "AHO_CLOUD_SESSION_SECRET=$(openssl rand -hex 32)"
  } >"$keyfile"
fi
set -a
# shellcheck disable=SC1090
source "$keyfile"
set +a

echo "building binaries…"
(cd "$root" && CARGO_TARGET_DIR="$target" cargo build -q -p cloud-control-plane -p cloud-session-host -p bundle-cloud -p ahocloud)
if [[ ! -d "$root/web/node_modules" ]]; then
  (cd "$root/web" && npm install --no-audit --no-fund)
fi

pids=()
cleanup() {
  for pid in ${pids[@]+"${pids[@]}"}; do kill "$pid" 2>/dev/null || true; done
  wait 2>/dev/null || true
}
trap cleanup EXIT INT TERM

# Nothing listens on port 9: builds and deploys fail fast with a clear error.
"$target/debug/cloud-control-plane" \
  --ephemeral \
  --bind "127.0.0.1:$cp_port" \
  --data-dir "$dev/control-plane" \
  --public-url "http://127.0.0.1:$cp_port" \
  --rp-id localhost \
  --rp-origin "http://localhost:$web_port" \
  --builder-url http://127.0.0.1:9 \
  --runtime-url http://127.0.0.1:9 \
  --session-host-url "http://127.0.0.1:$sh_port" \
  --edge-cname edge.localhost \
  --preview-domain preview.localhost &
pids+=($!)

"$target/debug/cloud-session-host" \
  --listen "127.0.0.1:$sh_port" \
  --data-root "$dev/sessions" \
  --public-url "ws://127.0.0.1:$sh_port" \
  --preview-url "http://{project}.preview.localhost" \
  --command "$target/debug/ahoc" &
pids+=($!)

(cd "$root/web" && AHO_CLOUD_API="http://127.0.0.1:$cp_port" exec npx vite --port "$web_port" --strictPort) &
pids+=($!)

echo
echo "console:        http://localhost:$web_port"
echo "control plane:  http://127.0.0.1:$cp_port   (ahocloud --api-url http://127.0.0.1:$cp_port)"
echo "session host:   ws://127.0.0.1:$sh_port"
echo "state:          $dev   (in-memory control plane: accounts vanish on restart)"
echo "Ctrl-C stops everything."
# `wait -n` needs bash 4.3; stock macOS has 3.2. Ctrl-C runs cleanup.
wait
