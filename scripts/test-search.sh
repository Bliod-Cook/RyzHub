#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
export SQLX_OFFLINE=true CARGO_INCREMENTAL=0
export RYZHUB_TEST_EMBEDDING_KEY=local-embedding-fixture
cargo test -p ryzhub-server --test search -- --ignored --test-threads=1 --nocapture
