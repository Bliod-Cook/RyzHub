#!/usr/bin/env bash
set -euo pipefail
export SQLX_OFFLINE=true CARGO_INCREMENTAL=0
# Public test-only signing key used by the virtual-key rotation regression.
export RYZHUB_TEST_SIGNING_KEY=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
cd "$(dirname "$0")/.."
docker compose up -d --wait
cargo test -p ryzhub-server --test consistency -- --ignored --test-threads=1
