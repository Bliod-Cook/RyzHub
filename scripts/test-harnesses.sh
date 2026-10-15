#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
export CARGO_INCREMENTAL=0 SQLX_OFFLINE=true
export RYZHUB_TEST_VAULT_KEY=AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE
export RYZHUB_TEST_UPSTREAM_KEY=local-fixture-key
docker compose up -d --wait
./scripts/prepare-runtimes.sh
[[ -s var/execution/harness-image.txt ]] || ./scripts/build-harness-image.sh
cargo test -p ryzhub-server --test harnesses -- --ignored --test-threads=1 --nocapture
