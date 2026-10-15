#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
# Real providers for the checks that use them (see the suite); without them those checks skip.
env_file="${RYZHUB_ENV_FILE:-.env}"
if [[ -f "$env_file" ]]; then set -a; source "$env_file"; set +a; fi
export SQLX_OFFLINE=true
# Deliberately public, test-only secrets. Never used for deployment credentials.
export RYZHUB_TEST_VAULT_KEY=AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE
export RYZHUB_TEST_UPSTREAM_KEY=local-fixture-key
docker compose up -d --wait
./scripts/start-execution-node.sh
cargo test -p ryzhub-server --test platform -- --ignored --test-threads=1 --nocapture
