#!/usr/bin/env bash
# Run Mage locally: Postgres 16 in Docker, build UI + binary, migrate, first-run setup, then
# `mage start all` (server + indexer + runner in one process, development only).
#
# Usage: ./run.sh [--reset] [--rebuild-web] [--no-web]
#   --reset        drop the database and the runner's saved credentials, then set up from scratch
#   --rebuild-web  rebuild the embedded UI even if web-app/dist exists
#   --no-web       skip the UI build (the server then serves the API only)
#
# Required:
#   MAGE_GITHUB_APP_PRIVATE_KEY_FILE   path to the GitHub App private key (.pem)
#   MAGE_GITHUB_CLIENT_ID              the GitHub App's client id (Iv1.…), unless set in the config
#   MAGE_GITHUB_APP_SLUG               the app's slug (github.com/apps/<slug>), unless set in the config
# Required on first run (or after --reset):
#   MAGE_GITHUB_OAUTH_CLIENT_SECRET    the GitHub App's OAuth client secret (stored in the vault)
#   MAGE_OWNER                         GitHub login of the first org owner
# Optional (defaults in brackets):
#   MAGE_CONFIG [config/mage.example.toml]   MAGE_ORG [hoop]   MAGE_ORG_NAME [Hoop]
#   MAGE_PG_CONTAINER [mage-pg]   MAGE_PG_PORT [55432]   MAGE_DB [mage]
#   MAGE_PORT [8080]   Mage server port (UI, API, runner protocol). The GitHub App's callback URL
#                      must be http://localhost:$MAGE_PORT/auth/github/callback
#   MAGE_INDEXER_PORT [8081]   indexer internal API port
# Any config key can be overridden as MAGE_<SECTION>__<KEY>=value (see the config header); this
# script honors MAGE_SERVER__VAULT__LOCAL_KEY_FILE, MAGE_RUNNER__DATA_DIR and
# MAGE_RUNNER__LINK__BOOTSTRAP_TOKEN (file:…) for its own paths.
set -euo pipefail

cd "$(dirname "$0")"
ROOT=$PWD

RESET=0 WEB=1 REBUILD_WEB=0
for arg in "$@"; do
  case "$arg" in
    --reset) RESET=1 ;;
    --no-web) WEB=0 ;;
    --rebuild-web) REBUILD_WEB=1 ;;
    -h|--help) sed -n '2,25p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $arg (see --help)" >&2; exit 2 ;;
  esac
done

CONFIG=${MAGE_CONFIG:-config/mage.example.toml}
ORG=${MAGE_ORG:-hoop}
ORG_NAME=${MAGE_ORG_NAME:-Hoop}
PG=${MAGE_PG_CONTAINER:-mage-pg}
PG_PORT=${MAGE_PG_PORT:-55432}
DB=${MAGE_DB:-mage}
KEY_FILE=${MAGE_SERVER__VAULT__LOCAL_KEY_FILE:-$HOME/.config/mage/dev.key}
RUNNER_DIR=${MAGE_RUNNER__DATA_DIR:-$HOME/.mage/runner}
BOOTSTRAP_REF=${MAGE_RUNNER__LINK__BOOTSTRAP_TOKEN:-file:$HOME/.config/mage/runner-bootstrap}
BOOTSTRAP_FILE=${BOOTSTRAP_REF#file:}
RUNNER_CREDENTIALS=$RUNNER_DIR/credentials.json

# Ports: one value feeds every key that names them, so they cannot disagree.
PORT=${MAGE_PORT:-8080}
INDEXER_PORT=${MAGE_INDEXER_PORT:-8081}
export MAGE_SERVER__BIND=127.0.0.1:$PORT
export MAGE_SERVER__PUBLIC_URL=http://localhost:$PORT
export MAGE_RUNNER__SERVER_URL=http://localhost:$PORT
export MAGE_INDEXER__INTERNAL_AUTH__ISSUER=http://localhost:$PORT
export MAGE_INDEXER__INTERNAL_LISTEN=127.0.0.1:$INDEXER_PORT
export MAGE_SERVER__KNOWLEDGE__INDEXER_URL=http://127.0.0.1:$INDEXER_PORT

say() { printf '\033[1m==> %s\033[0m\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "$1 not found on PATH"; }

need docker
need cargo
[ "$WEB" = 0 ] || need npm
for h in omp claude; do
  command -v "$h" >/dev/null 2>&1 || echo "note: $h not on PATH; the runner will not offer that harness"
done

[ -n "${MAGE_GITHUB_APP_PRIVATE_KEY_FILE:-}" ] || die "set MAGE_GITHUB_APP_PRIVATE_KEY_FILE to the GitHub App private key (.pem)"
[ -r "$MAGE_GITHUB_APP_PRIVATE_KEY_FILE" ] || die "cannot read $MAGE_GITHUB_APP_PRIVATE_KEY_FILE"

# `localhost` may resolve to ::1 first, so a port is only free if nothing listens on it over IPv4
# or IPv6; otherwise browsers and the runner reach that other program instead of Mage.
port_owner() { { lsof -nP -iTCP:"$1" -sTCP:LISTEN 2>/dev/null || true; } | awk 'NR > 1 { print $1 " (pid " $2 ")"; exit }'; }
for p in "$PORT:MAGE_PORT" "$INDEXER_PORT:MAGE_INDEXER_PORT"; do
  owner=$(port_owner "${p%%:*}")
  [ -z "$owner" ] || die "port ${p%%:*} is used by $owner; stop it or pick another port, e.g. ${p#*:}=$(( ${p%%:*} + 10 )) ./run.sh"
done
[ -f "$CONFIG" ] || die "config not found: $CONFIG"

# --- Postgres 16 ---------------------------------------------------------------------------------
psql_q() { docker exec "$PG" psql -U postgres -d "${2:-postgres}" -v ON_ERROR_STOP=1 -tAc "$1"; }

# GitHub App identity: env wins over the config; the shipped placeholders are refused.
if [ -n "${MAGE_GITHUB_CLIENT_ID:-}" ]; then
  export MAGE_SERVER__GITHUB__CLIENT_ID=$MAGE_GITHUB_CLIENT_ID
  export MAGE_SERVER__LOGIN__GITHUB__CLIENT_ID=$MAGE_GITHUB_CLIENT_ID
  export MAGE_INDEXER__GITHUB__CLIENT_ID=$MAGE_GITHUB_CLIENT_ID
elif grep -q '^client_id *= *"Iv1\.0000000000000000"' "$CONFIG"; then
  die "set MAGE_GITHUB_CLIENT_ID (or replace the placeholder client_id in $CONFIG)"
fi
if [ -n "${MAGE_GITHUB_APP_SLUG:-}" ]; then
  export MAGE_SERVER__INTEGRATIONS__GITHUB_APP_SLUG=$MAGE_GITHUB_APP_SLUG
fi
if ! docker info >/dev/null 2>&1; then
  die "docker daemon is not running (start Docker/OrbStack)"
fi
if docker container inspect "$PG" >/dev/null 2>&1; then
  if [ "$(docker container inspect -f '{{.State.Running}}' "$PG")" != true ]; then
    say "starting Postgres container $PG"
    docker start "$PG" >/dev/null
  fi
else
  if lsof -nP -iTCP:"$PG_PORT" -sTCP:LISTEN >/dev/null 2>&1; then
    die "port $PG_PORT is already in use (another Postgres?); set MAGE_PG_PORT to a free port"
  fi
  say "creating Postgres 16 container $PG on port $PG_PORT"
  docker run -d --name "$PG" -e POSTGRES_HOST_AUTH_METHOD=trust -p "$PG_PORT:5432" postgres:16 >/dev/null
fi
# The container must publish 5432 on the host, or the URL below would reach a different server.
published=$( { docker port "$PG" 5432/tcp 2>/dev/null || true; } | sed -n 's/^0\.0\.0\.0:\([0-9]*\)$/\1/p' | head -1)
if [ -z "$published" ]; then
  die "container $PG does not publish port 5432 (its host port was probably taken when it was created).
  Fix: docker rm -f $PG   then re-run with a free port, e.g. MAGE_PG_PORT=55433 ./run.sh"
fi
if [ "$published" != "$PG_PORT" ]; then
  echo "note: $PG publishes port $published (not $PG_PORT); using $published"
  PG_PORT=$published
fi
for _ in $(seq 1 60); do
  docker exec "$PG" pg_isready -U postgres >/dev/null 2>&1 && break
  sleep 1
done
docker exec "$PG" pg_isready -U postgres >/dev/null 2>&1 || die "Postgres in $PG did not become ready"
major=$(psql_q "SHOW server_version_num")
[ "${major:0:2}" -ge 16 ] || die "Postgres 16+ required in $PG (found server_version_num $major)"

if [ "$RESET" = 1 ]; then
  say "reset: dropping database $DB and the runner credentials"
  psql_q "DROP DATABASE IF EXISTS \"$DB\" WITH (FORCE)" >/dev/null
  rm -f "$RUNNER_CREDENTIALS" "$BOOTSTRAP_FILE"
fi
if [ -z "$(psql_q "SELECT 1 FROM pg_database WHERE datname = '$DB'")" ]; then
  say "creating database $DB"
  psql_q "CREATE DATABASE \"$DB\"" >/dev/null
fi

export MAGE_DATABASE_URL=${MAGE_DATABASE_URL:-postgres://postgres@localhost:$PG_PORT/$DB}
MAGE_GITHUB_APP_PRIVATE_KEY=$(cat "$MAGE_GITHUB_APP_PRIVATE_KEY_FILE")
export MAGE_GITHUB_APP_PRIVATE_KEY

# --- build ---------------------------------------------------------------------------------------
if [ "$WEB" = 1 ] && { [ "$REBUILD_WEB" = 1 ] || [ ! -f web-app/dist/index.html ]; }; then
  say "building the UI (web, then web-app)"
  (cd ../packages/ui && npm ci --no-audit --no-fund)
  (cd web && npm ci --no-audit --no-fund)
  (cd web-app && npm ci --no-audit --no-fund && npm run build)
fi
say "building mage"
cargo build -p mage-app
MAGE_BIN=$ROOT/target/debug/mage
mage() { "$MAGE_BIN" --config "$CONFIG" "$@"; }

# --- first-run setup -----------------------------------------------------------------------------
if [ ! -f "$KEY_FILE" ]; then
  say "creating local vault key $KEY_FILE"
  mkdir -p "$(dirname "$KEY_FILE")"
  (umask 077 && head -c 32 /dev/urandom > "$KEY_FILE")
fi

say "applying migrations"
mage migrate

has_secret=$(psql_q "SELECT 1 FROM secrets WHERE org_id IS NULL AND name = 'github/oauth_client_secret' AND state = 'active' LIMIT 1" "$DB")
if [ -z "$has_secret" ]; then
  [ -n "${MAGE_GITHUB_OAUTH_CLIENT_SECRET:-}" ] || die "first run: set MAGE_GITHUB_OAUTH_CLIENT_SECRET (GitHub App OAuth client secret)"
  say "storing github/oauth_client_secret in the vault"
  printf '%s\n' "$MAGE_GITHUB_OAUTH_CLIENT_SECRET" | mage admin secret put platform github/oauth_client_secret --stdin
fi

if [ -z "$(psql_q "SELECT 1 FROM orgs WHERE slug = '$ORG'" "$DB")" ]; then
  [ -n "${MAGE_OWNER:-}" ] || die "first run: set MAGE_OWNER to the GitHub login of the org owner"
  say "creating org $ORG (owner $MAGE_OWNER)"
  mage admin org create "$ORG" --name "$ORG_NAME" --owner "$MAGE_OWNER"
fi

# Registered = the credential file holds a runner id; a failed first start leaves `"runner_id":null`.
registered=0
grep -Eq '"runner_id" *: *"' "$RUNNER_CREDENTIALS" 2>/dev/null && registered=1
if [ "$registered" = 0 ] && [ ! -s "$BOOTSTRAP_FILE" ]; then
  say "issuing a runner bootstrap token ($BOOTSTRAP_FILE)"
  mkdir -p "$(dirname "$BOOTSTRAP_FILE")"
  (umask 077 && mage admin runner-token create dev-runner > "$BOOTSTRAP_FILE")
fi

# --- run -----------------------------------------------------------------------------------------
say "starting mage (server + indexer + runner) — UI at $MAGE_SERVER__PUBLIC_URL; Ctrl-C to stop"
exec "$MAGE_BIN" --config "$CONFIG" start all
