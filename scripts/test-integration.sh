#!/usr/bin/env bash
set -euo pipefail
export SQLX_OFFLINE=true CARGO_INCREMENTAL=0
# Deliberately public, test-only secret for the fixture model upstream.
export RYZHUB_TEST_UPSTREAM_KEY=local-fixture-key
cd "$(dirname "$0")/.."
docker compose up -d --wait
cargo test -p ryzhub-server --test consistency -- --ignored --test-threads=1
