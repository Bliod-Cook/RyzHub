#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
: "${DATABASE_URL:?Set DATABASE_URL to a migrated PostgreSQL 18 database}"
# SQLx writes metadata for the critical fencing and sequence-allocation queries.
SQLX_OFFLINE=false SQLX_OFFLINE_DIR="$PWD/.sqlx" cargo check -p ryzhub-server
