#!/usr/bin/env bash
# Local aho cloud.
#
#   scripts/dev.sh                 # control plane, session host, console
#   scripts/dev.sh --with-deploy   # + builder, runtime (Docker backend), edge, mail catcher, sandboxes
#   scripts/dev.sh --down          # remove the deploy containers (images and volumes stay)
#
# Without --with-deploy, builder and runtime are not started, so releases stop at `build failed`;
# passkeys, device login, orgs, projects, domains, secrets, manifests, drafts, the approval card
# and workspaces with a live aho session all work.
#
# With --with-deploy (needs Docker: OrbStack, Docker Desktop or colima), releases are built by
# BuildKit into a local registry, run as containers, and served by the edge on
# http://<project>.preview.localhost:8443 behind magic-link login (mail lands in the catcher).
# Containers stand in for microVMs: there is no VM isolation and egress allowlists are not
# enforced. The same mode builds the sandbox image (scripts/build-sandbox-image.sh; the first
# build compiles aho for Linux, a few minutes) and enables sandboxes: VMs (here containers) with
# aho, reached from the console's Terminal and Chat tabs or `ahocloud ssh`.
#
# Env: AHO_CLOUD_DEV_DIR (state, default ./.dev), CP_PORT (8080), SH_PORT (8790), WEB_PORT (5173),
#      EDGE_PORT (8443), REGISTRY_PORT (5055; macOS AirPlay holds 5000).
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
dev="${AHO_CLOUD_DEV_DIR:-$root/.dev}"
cp_port="${CP_PORT:-8080}"
sh_port="${SH_PORT:-8790}"
web_port="${WEB_PORT:-5173}"
edge_port="${EDGE_PORT:-8443}"
registry_port="${REGISTRY_PORT:-5055}"
builder_port=7420
runtime_port=7400
buildkit_port=1234
target="${CARGO_TARGET_DIR:-$root/target}"

buildkit_version="v0.33.0"
# sha256 of buildkit-<version>.darwin-<arch>.tar.gz, from the GitHub release assets.
buildctl_sha256_arm64="730ac4ffd6f4a88dc404fc675aeaf4cfee414915036042847f0a60861cc8790c"
buildctl_sha256_amd64="3c51f59d731c2882226f564c56ff3fe2a726b9bded6c4626c6868862c986a08c"
network="aho-cloud"
containers=(aho-cloud-registry aho-cloud-buildkit aho-cloud-mailpit)

mode=""
case "${1:-}" in
  "") ;;
  --with-deploy) mode=deploy ;;
  --down) mode=down ;;
  *) echo "usage: $0 [--with-deploy | --down]" >&2; exit 2 ;;
esac

remove_apps() {
  local ids
  ids=$(docker ps -aq --filter label=aho-cloud.managed=1 2>/dev/null || true)
  if [[ -n "$ids" ]]; then
    # shellcheck disable=SC2086
    docker rm -f $ids >/dev/null
  fi
}

if [[ "$mode" == down ]]; then
  remove_apps
  docker rm -f "${containers[@]}" >/dev/null 2>&1 || true
  echo "removed deploy containers (volumes aho-cloud-registry and aho-cloud-buildkit kept)"
  exit 0
fi

mkdir -p "$dev/control-plane" "$dev/sessions"
# Keys live with the state so a restart keeps workspaces readable.
keyfile="$dev/keys.env"
if [[ ! -f "$keyfile" ]]; then
  (
    umask 077
    {
      echo "AHO_CLOUD_MASTER_KEY=$(openssl rand -hex 32)"
      echo "AHO_CLOUD_SERVICE_TOKEN=$(openssl rand -hex 32)"
      echo "AHO_CLOUD_SESSION_SECRET=$(openssl rand -hex 32)"
    } >"$keyfile"
  )
fi
set -a
# shellcheck disable=SC1090
source "$keyfile"
set +a

# Starts container $1 with `docker run` arguments $2…, or reuses it if it already exists.
ensure_container() {
  local name="$1"
  shift
  if [[ "$(docker inspect --type container -f '{{.State.Running}}' "$name" 2>/dev/null || true)" == "true" ]]; then
    return
  fi
  if docker inspect --type container "$name" >/dev/null 2>&1; then
    docker start "$name" >/dev/null
  else
    docker run -d --name "$name" "$@" >/dev/null
  fi
}

# Downloads the pinned buildctl client into $dev/bin, checking its checksum.
ensure_buildctl() {
  local bin="$dev/bin/buildctl"
  [[ -x "$bin" ]] && return
  local arch sha
  case "$(uname -m)" in
    arm64 | aarch64) arch=arm64 sha="$buildctl_sha256_arm64" ;;
    x86_64) arch=amd64 sha="$buildctl_sha256_amd64" ;;
    *) echo "no pinned buildctl for $(uname -m)" >&2; exit 1 ;;
  esac
  if [[ "$(uname -s)" != Darwin ]]; then
    echo "--with-deploy pins the macOS buildctl; on Linux install buildctl and put it at $bin" >&2
    exit 1
  fi
  local tarball="$dev/bin/buildkit-$buildkit_version.tar.gz"
  mkdir -p "$dev/bin"
  echo "downloading buildctl ${buildkit_version}…"
  curl -fsSL -o "$tarball" "https://github.com/moby/buildkit/releases/download/$buildkit_version/buildkit-$buildkit_version.darwin-$arch.tar.gz"
  if [[ "$(shasum -a 256 "$tarball" | awk '{print $1}')" != "$sha" ]]; then
    rm -f "$tarball"
    echo "buildctl checksum mismatch; refusing to use it" >&2
    exit 1
  fi
  tar -xzf "$tarball" -C "$dev/bin" --strip-components 1 bin/buildctl
  rm -f "$tarball"
}

start_infra() {
  if ! docker info >/dev/null 2>&1; then
    echo "--with-deploy needs a running Docker daemon (OrbStack, Docker Desktop or colima)" >&2
    exit 1
  fi
  ensure_buildctl
  docker network inspect "$network" >/dev/null 2>&1 || docker network create "$network" >/dev/null
  # BuildKit pushes to the registry by its network alias; the host (and the Docker daemon
  # pulling for the runtime) reaches the same registry on localhost.
  ensure_container aho-cloud-registry --network "$network" --network-alias aho-registry \
    -p "127.0.0.1:$registry_port:5000" -v aho-cloud-registry:/var/lib/registry registry:2
  printf '[registry."aho-registry:5000"]\n  http = true\n  insecure = true\n' >"$dev/buildkitd.toml"
  ensure_container aho-cloud-buildkit --privileged --network "$network" \
    -p "127.0.0.1:$buildkit_port:1234" -v "$dev/buildkitd.toml:/etc/buildkit/buildkitd.toml:ro" \
    -v aho-cloud-buildkit:/var/lib/buildkit "moby/buildkit:$buildkit_version" \
    --addr tcp://0.0.0.0:1234 --config /etc/buildkit/buildkitd.toml
  # Magic-link mail for edge logins: SMTP on 1025, inbox on http://localhost:8025.
  ensure_container aho-cloud-mailpit -p 127.0.0.1:1025:1025 -p 127.0.0.1:8025:8025 axllent/mailpit
  # Apps left running by an earlier run belong to nobody now.
  remove_apps
}

echo "building binaries…"
crates=(-p cloud-control-plane -p cloud-session-host -p bundle-cloud -p ahocloud)
if [[ "$mode" == deploy ]]; then
  crates+=(-p cloud-builder -p cloud-runtime -p cloud-edge)
fi
if ! (cd "$root" && CARGO_TARGET_DIR="$target" cargo build -q "${crates[@]}") 2>"$dev/build.log"; then
  cat "$dev/build.log" >&2
  exit 1
fi
if [[ ! -d "$root/web/node_modules" ]]; then
  (cd "$root/web" && npm install --no-audit --no-fund)
fi

if [[ "$mode" == deploy ]]; then
  start_infra
  builder_url="http://127.0.0.1:$builder_port"
  runtime_url="http://127.0.0.1:$runtime_port"
  preview_url="http://{project}.preview.localhost:$edge_port"
  echo "building the sandbox image (log: $dev/sandbox-image.log)…"
  if ! sandbox_image=$(REGISTRY_PORT="$registry_port" BUILDKIT_PROGRESS=plain \
    "$root/scripts/build-sandbox-image.sh" "localhost:$registry_port/aho-cloud" 2>"$dev/sandbox-image.log"); then
    tail -n 30 "$dev/sandbox-image.log" >&2
    exit 1
  fi
  # Sandboxes need the runtime: without these flags they answer 503 `sandboxes_disabled`.
  cp_sandbox=(--sandbox-image "$sandbox_image")
  sh_sandbox=(--runtime-url "$runtime_url")
else
  # Nothing listens on port 9: builds and deploys fail fast with a clear error.
  builder_url="http://127.0.0.1:9"
  runtime_url="http://127.0.0.1:9"
  preview_url="http://{project}.preview.localhost"
  cp_sandbox=()
  sh_sandbox=()
fi

pids=()
cleanup() {
  for pid in ${pids[@]+"${pids[@]}"}; do kill "$pid" 2>/dev/null || true; done
  wait 2>/dev/null || true
  if [[ "$mode" == deploy ]]; then
    remove_apps
  fi
}
trap cleanup EXIT INT TERM

"$target/debug/cloud-control-plane" \
  --ephemeral \
  --bind "127.0.0.1:$cp_port" \
  --data-dir "$dev/control-plane" \
  --public-url "http://127.0.0.1:$cp_port" \
  --rp-id localhost \
  --rp-origin "http://localhost:$web_port" \
  --builder-url "$builder_url" \
  --runtime-url "$runtime_url" \
  --session-host-url "http://127.0.0.1:$sh_port" \
  --edge-cname edge.localhost \
  --preview-domain preview.localhost \
  --preview-url "$preview_url" \
  ${cp_sandbox[@]+"${cp_sandbox[@]}"} &
pids+=($!)

# Workspaces run in aho's host world whatever ~/.aho/config.toml says: the Code tab and "Deploy to
# preview" read the files from the workspace directory on this machine, and a vm world keeps them
# on the VM's disk (and needs a lease). The host world is policy-only, not a sandbox.
"$target/debug/cloud-session-host" \
  --listen "127.0.0.1:$sh_port" \
  --data-root "$dev/sessions" \
  --public-url "ws://127.0.0.1:$sh_port" \
  --preview-url "$preview_url" \
  --command "$target/debug/ahoc" \
  --set seams.world=host \
  ${sh_sandbox[@]+"${sh_sandbox[@]}"} &
pids+=($!)

if [[ "$mode" == deploy ]]; then
  mkdir -p "$dev/builder" "$dev/runtime" "$dev/edge"
  "$target/debug/cloud-builder" \
    --listen "127.0.0.1:$builder_port" \
    --registry aho-registry:5000/aho-cloud \
    --work-dir "$dev/builder" \
    --buildctl "$dev/bin/buildctl" \
    --buildkit-addr "tcp://127.0.0.1:$buildkit_port" &
  pids+=($!)

  "$target/debug/cloud-runtime-agent" \
    --backend docker \
    --listen "127.0.0.1:$runtime_port" \
    --state-dir "$dev/runtime" \
    --image-repo "localhost:$registry_port/aho-cloud/{project}" \
    --pids-max 4096 \
    --reconcile-interval-secs 3 &
  pids+=($!)

  "$target/debug/cloud-edge" \
    --plain "127.0.0.1:$edge_port" \
    --https 127.0.0.1:9443 \
    --http 127.0.0.1:9080 \
    --control-plane "http://127.0.0.1:$cp_port" \
    --keys-dir "$dev/edge/keys" \
    --certs-dir "$dev/edge/certs" \
    --http01-dir "$dev/edge/http01" \
    --poll-secs 2 \
    --smtp-url smtp://127.0.0.1:1025 \
    --mail-from "aho cloud <login@localhost>" &
  pids+=($!)
fi

(cd "$root/web" && AHO_CLOUD_API="http://127.0.0.1:$cp_port" exec npx vite --port "$web_port" --strictPort) &
pids+=($!)

echo
echo "console:        http://localhost:$web_port"
echo "control plane:  http://127.0.0.1:$cp_port   (ahocloud --api-url http://127.0.0.1:$cp_port)"
echo "session host:   ws://127.0.0.1:$sh_port"
if [[ "$mode" == deploy ]]; then
  echo "previews:       http://<project>.preview.localhost:$edge_port   (login mail: http://localhost:8025)"
  echo "registry:       localhost:$registry_port   builder :$builder_port   runtime :$runtime_port"
  echo "sandboxes:      image $sandbox_image   (ahocloud ssh <sandbox>)"
fi
echo "state:          $dev   (in-memory control plane: accounts vanish on restart)"
echo "Ctrl-C stops everything."
# `wait -n` needs bash 4.3; stock macOS has 3.2. Ctrl-C runs cleanup.
wait
