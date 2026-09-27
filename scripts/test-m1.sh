#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
export SQLX_OFFLINE=true
# Deliberately public, test-only secrets. Never used for deployment credentials.
export RYZHUB_TEST_VAULT_KEY=AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE
export RYZHUB_TEST_UPSTREAM_KEY=local-fixture-key
docker compose up -d --wait
./scripts/start-execution-node.sh
cargo test -p ryzhub-server --test m1 -- --ignored --test-threads=1
