#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
export CARGO_INCREMENTAL=0 SQLX_OFFLINE=true
export RYZHUB_TEST_VAULT_KEY=AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE
export RYZHUB_TEST_UPSTREAM_KEY=local-fixture-key
export RYZHUB_TEST_S3_ACCESS=local-s3-access RYZHUB_TEST_S3_SECRET=local-s3-secret-key
export RYZHUB_TEST_S3_PORT=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')
export RYZHUB_TEST_S3_URL="http://127.0.0.1:$RYZHUB_TEST_S3_PORT"
uv run --with botocore python scripts/s3-fixture.py &
s3_fixture_pid=$!
trap 'kill "$s3_fixture_pid" 2>/dev/null || true' EXIT
for _ in {1..100}; do
 if curl -s -o /dev/null "$RYZHUB_TEST_S3_URL"; then break; fi
 sleep 0.1
done
docker compose up -d --wait
./scripts/prepare-runtimes.sh
cargo test -p ryzhub-server --test work -- --ignored --test-threads=1
TEST_RUNTIME=runsc cargo test -p ryzhub-server --test work actual_codex -- --ignored --test-threads=1
