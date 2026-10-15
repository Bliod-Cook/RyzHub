#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
export CARGO_INCREMENTAL=0 SQLX_OFFLINE=true
docker compose up -d --wait
mkdir -p var/dev
chmod 700 var/dev
[[ -f var/vault.env ]] || (umask 077; mkdir -p var; printf 'RYZHUB_VAULT_KEY=%s\n' "$(head -c 32 /dev/urandom | base64 | tr '+/' '-_' | tr -d '=')" > var/vault.env)
[[ -S var/execution/host.sock ]] || ./scripts/prepare-runtimes.sh
[[ -s var/execution/harness-image.txt ]] || ./scripts/build-harness-image.sh
set -a
source .env.example
source var/vault.env
set +a
# The development database, shared with dev-live.sh, is separate from the default one.
if ! docker compose exec -T postgres psql -U postgres -Atc "SELECT 1 FROM pg_database WHERE datname='ryzhub_dev'" | grep -q 1; then
 docker compose exec -T postgres psql -U postgres -c 'CREATE DATABASE ryzhub_dev'
fi
export DATABASE_URL="${DATABASE_URL%/*}/ryzhub_dev"
export WORKER_DATABASE_URL="${WORKER_DATABASE_URL%/*}/ryzhub_dev"
export MIGRATION_DATABASE_URL="${MIGRATION_DATABASE_URL%/*}/ryzhub_dev"
export OPENAI_API_KEY=local-responses-key ANTHROPIC_API_KEY=local-fixture-key
export RYZHUB_FIXTURE_PORT=9181 RYZHUB_RESPONSES_FIXTURE_PORT=9182
export RYZHUB_CONFIG="$PWD/var/dev/fixture.toml"
python3 - <<'PY'
import pathlib,json,os
p=pathlib.Path.cwd();q=json.dumps
pathlib.Path(os.environ['RYZHUB_CONFIG']).write_text(f'''listen = "127.0.0.1:8280"
allowed_origins = ["http://localhost:3000", "http://127.0.0.1:3000"]
[policy]
models = ["fixture-model", "gpt-5.4", "gpt-5.2", "claude-sonnet-4-6", "claude-opus-4-6"]
harnesses = ["chat", "codex", "claude"]
max_sandboxes = 8
allow_full_auto = false
[llm]
enabled = true
listen = "127.0.0.1:8281"
gateway_url = "http://127.0.0.1:8281"
unix_socket = {q(str(p/'var/execution/gateway/llm-dev.sock'))}
upstream_url = "http://127.0.0.1:9181"
responses_url = "http://127.0.0.1:9182"
model = "fixture-model"
[[llm.models]]
id = "gpt-5.4"
protocol = "responses"
harnesses = ["chat"]
[[llm.models]]
id = "gpt-5.2"
protocol = "responses"
harnesses = ["codex", "chat"]
[[llm.models]]
id = "claude-opus-4-6"
protocol = "anthropic"
harnesses = ["claude", "chat"]
[sandbox]
enabled = true
host_socket = {q(str(p/'var/execution/host.sock'))}
image = {q((p/'var/execution/harness-image.txt').read_text().strip())}
runtime = "runc"
memory_bytes = 1073741824
pids_limit = 256
[work]
enabled = true
model = "gpt-5.4"
claude_model = "claude-sonnet-4-6"
storage_root = {q(str(p/'var/dev/blobs'))}
secondary_root = {q(str(p/'var/dev/secondary'))}
''')
PY
cargo build -p ryzhub-server
python3 scripts/anthropic-fixture.py > var/dev/anthropic-fixture.log 2>&1 &
anthropic_pid=$!
python3 scripts/responses-fixture.py > var/dev/responses-fixture.log 2>&1 &
responses_pid=$!
trap 'kill "$anthropic_pid" "$responses_pid" 2>/dev/null || true' EXIT
target/debug/ryzhub-server migrate
target/debug/ryzhub-server all
