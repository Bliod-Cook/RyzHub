#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
export CARGO_INCREMENTAL=0 SQLX_OFFLINE=true
export RYZHUB_TEST_VAULT_KEY=AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE
export RYZHUB_TEST_UPSTREAM_KEY=local-fixture-key
# The MinIO service's deliberately public, test-only credentials (compose.yaml).
export RYZHUB_TEST_S3_ACCESS=local-s3-access RYZHUB_TEST_S3_SECRET=local-s3-secret-key
docker compose --profile test up -d --wait
./scripts/prepare-runtimes.sh
cargo test -p ryzhub-server --test work -- --ignored --test-threads=1
TEST_RUNTIME=runsc cargo test -p ryzhub-server --test work actual_codex -- --ignored --test-threads=1
