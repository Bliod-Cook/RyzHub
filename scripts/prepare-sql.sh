#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
: "${DATABASE_URL:?Set DATABASE_URL to a migrated PostgreSQL 18 database}"
# SQLx writes offline metadata for the compile-time-checked queries: lease claim/renew/fencing,
# sequence allocation, publishing, command idempotency, membership and gateway key lookup.
SQLX_OFFLINE=false SQLX_OFFLINE_DIR="$PWD/.sqlx" cargo check -p ryzhub-server
