#!/usr/bin/env bash
# Builds the sandbox VM image (images/sandbox/Dockerfile), pushes it, and prints the pinned
# reference the control plane takes as `--sandbox-image`: `aho.sandbox@sha256:<hex>`, relative
# to the runtime's `--image-repo` template.
#
#   scripts/build-sandbox-image.sh [REPO_PREFIX]
#
# REPO_PREFIX is where the runtime pulls images from, without `{project}`: the default
# `localhost:${REGISTRY_PORT:-5055}/aho-cloud` matches scripts/dev.sh. Needs Docker with buildx
# (OrbStack, Docker Desktop, colima). Progress goes to stderr; only the reference to stdout.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
aho="${AHO_SRC:-$(cd "$root/../aho" && pwd)}"
prefix="${1:-localhost:${REGISTRY_PORT:-5055}/aho-cloud}"
repo="aho.sandbox"
image="$prefix/$repo"

if [[ ! -f "$aho/Cargo.toml" ]]; then
  echo "no aho checkout at $aho (set AHO_SRC)" >&2
  exit 1
fi

docker buildx build \
  --progress "${BUILDKIT_PROGRESS:-auto}" \
  --build-context "aho-src=$aho" \
  -f "$root/images/sandbox/Dockerfile" \
  --load \
  -t "$image:latest" \
  "$root" >&2
docker push -q "$image:latest" >&2

pinned=$(docker inspect --format '{{range .RepoDigests}}{{println .}}{{end}}' "$image:latest" | grep -m1 "^$image@sha256:" || true)
if [[ -z "$pinned" ]]; then
  echo "pushed $image:latest but found no digest for it" >&2
  exit 1
fi
echo "$repo@${pinned#*@}"
