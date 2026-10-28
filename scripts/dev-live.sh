#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
export CARGO_INCREMENTAL=0 SQLX_OFFLINE=true

# Keep real upstream configuration and credentials outside the tracked files.
dev_config="${RYZHUB_DEV_CONFIG:-$PWD/var/dev/config.toml}"
dev_secrets="${RYZHUB_DEV_SECRETS:-$PWD/var/dev/secrets.env}"
dev_database="${RYZHUB_DEV_DATABASE:-ryzhub_dev}"
[[ -f "$dev_config" ]] || { echo "Missing instance configuration: $dev_config" >&2; exit 1; }
[[ -f "$dev_secrets" ]] || { echo "Missing provider environment: $dev_secrets" >&2; exit 1; }
[[ -f var/vault.env ]] || { echo "Missing existing vault key: var/vault.env" >&2; exit 1; }

docker compose up -d --wait
[[ -S var/execution/host.sock ]] || ./scripts/prepare-runtimes.sh
set -a
source "${RYZHUB_ENV_FILE:-.env.example}"
source var/vault.env
source "$dev_secrets"
set +a

# The development database already holds the migrations and user data.
export DATABASE_URL="${DATABASE_URL%/*}/$dev_database"
export WORKER_DATABASE_URL="${WORKER_DATABASE_URL%/*}/$dev_database"
export MIGRATION_DATABASE_URL="${MIGRATION_DATABASE_URL%/*}/$dev_database"
export RYZHUB_CONFIG="$dev_config"
cargo build -p ryzhub-server
target/debug/ryzhub-server migrate
# The admin panel restarts the server by having it exit with 75; stopping this script stops it.
trap 'kill "$server" 2>/dev/null; wait "$server"; exit' TERM INT
while true; do
  target/debug/ryzhub-server all &
  server=$!
  status=0
  wait "$server" || status=$?
  [[ $status -eq 75 ]] || exit "$status"
done
