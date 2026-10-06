#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
export CARGO_INCREMENTAL=0 SQLX_OFFLINE=true

# Keep real upstream configuration and credentials outside the tracked files.
m4_config="${RYZHUB_M4_CONFIG:-$PWD/var/m4/config.toml}"
m4_secrets="${RYZHUB_M4_SECRETS:-$PWD/var/m4/secrets.env}"
m4_database="${RYZHUB_M4_DATABASE:-ryzhub_m3}"
[[ -f "$m4_config" ]] || { echo "Missing M4 configuration: $m4_config" >&2; exit 1; }
[[ -f "$m4_secrets" ]] || { echo "Missing provider environment: $m4_secrets" >&2; exit 1; }
[[ -f var/m1/secrets.env ]] || { echo "Missing existing vault key: var/m1/secrets.env" >&2; exit 1; }

docker compose up -d --wait
[[ -S var/execution/host.sock ]] || ./scripts/prepare-m2.sh
set -a
source "${RYZHUB_ENV_FILE:-.env.example}"
source var/m1/secrets.env
source "$m4_secrets"
set +a

# The existing preview database already contains the M4 migration and user data.
export DATABASE_URL="${DATABASE_URL%/*}/$m4_database"
export WORKER_DATABASE_URL="${WORKER_DATABASE_URL%/*}/$m4_database"
export MIGRATION_DATABASE_URL="${MIGRATION_DATABASE_URL%/*}/$m4_database"
export RYZHUB_CONFIG="$m4_config"
cargo build -p ryzhub-server
target/debug/ryzhub-server migrate
exec target/debug/ryzhub-server all
