#!/usr/bin/env bash
set -euo pipefail
export SQLX_OFFLINE=true
cd "$(dirname "$0")/.."
[[ -f var/vault.env ]] || (umask 077; mkdir -p var; printf 'RYZHUB_VAULT_KEY=%s\n' "$(head -c 32 /dev/urandom | base64 | tr '+/' '-_' | tr -d '=')" > var/vault.env)
set -a
source "${RYZHUB_ENV_FILE:-.env.example}"
source var/vault.env
set +a
# Chat answers come from the local Anthropic protocol fixture, never from a model.
export ANTHROPIC_API_KEY=local-fixture-key
docker compose up -d --wait
cargo build -p ryzhub-server
python3 scripts/anthropic-fixture.py &
fixture=$!
trap 'kill "$fixture" 2>/dev/null || true' EXIT
target/debug/ryzhub-server migrate
target/debug/ryzhub-server all
