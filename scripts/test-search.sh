#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
# Real providers for the checks that use them (see the suite); without them those checks skip.
env_file="${RYZHUB_ENV_FILE:-.env}"
if [[ -f "$env_file" ]]; then set -a; source "$env_file"; set +a; fi
export SQLX_OFFLINE=true CARGO_INCREMENTAL=0
# Deliberately public, test-only secret for the fixture model upstream.
export RYZHUB_TEST_UPSTREAM_KEY=local-fixture-key
cargo test -p ryzhub-server --test search -- --ignored --test-threads=1 --nocapture
