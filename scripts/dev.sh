#!/usr/bin/env bash
set -euo pipefail
export SQLX_OFFLINE=true
cd "$(dirname "$0")/.."
[[ -f var/vault.env ]] || (umask 077; mkdir -p var; python3 -c 'import base64,secrets; print("RYZHUB_VAULT_KEY="+base64.urlsafe_b64encode(secrets.token_bytes(32)).decode().rstrip("="))' > var/vault.env)
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
