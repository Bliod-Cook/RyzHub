#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
export CARGO_INCREMENTAL=0 SQLX_OFFLINE=true
export RYZHUB_TEST_SIGNING_KEY=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
export RYZHUB_TEST_VAULT_KEY=AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE
export RYZHUB_TEST_UPSTREAM_KEY=local-fixture-key
cargo test -p ryzhub-server --test m3 -- --ignored --test-threads=1 --nocapture
