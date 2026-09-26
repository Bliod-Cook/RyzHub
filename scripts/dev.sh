#!/usr/bin/env bash
set -euo pipefail
export SQLX_OFFLINE=true
cd "$(dirname "$0")/.."
set -a
source "${RYZHUB_ENV_FILE:-.env.example}"
set +a
docker compose up -d --wait
cargo run -p ryzhub-server -- migrate
exec cargo run -p ryzhub-server -- all
