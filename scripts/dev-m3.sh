#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
export CARGO_INCREMENTAL=0 SQLX_OFFLINE=true
docker compose up -d --wait
mkdir -p var/m3 var/m1
chmod 700 var/m1
if [[ ! -f var/m1/secrets.env ]]; then
 (umask 077; python3 - <<'KEYS' > var/m1/secrets.env
import secrets,base64
for name in ['RYZHUB_VAULT_KEY']:
 print(name+'='+base64.urlsafe_b64encode(secrets.token_bytes(32)).decode().rstrip('='))
KEYS
 )
fi
[[ -S var/execution/host.sock ]] || ./scripts/prepare-m2.sh
[[ -s var/m3/image.txt ]] || ./scripts/prepare-m3.sh
set -a
source .env.example
source var/m1/secrets.env
set +a
# A separate local development database preserves the existing M0-M2 data.
if ! docker exec ryzhub-m0-postgres-1 psql -U postgres -Atc "SELECT 1 FROM pg_database WHERE datname='ryzhub_m3'" | rg -q 1; then
 docker exec ryzhub-m0-postgres-1 psql -U postgres -c 'CREATE DATABASE ryzhub_m3'
fi
export DATABASE_URL="${DATABASE_URL%/*}/ryzhub_m3"
export WORKER_DATABASE_URL="${WORKER_DATABASE_URL%/*}/ryzhub_m3"
export MIGRATION_DATABASE_URL="${MIGRATION_DATABASE_URL%/*}/ryzhub_m3"
export OPENAI_API_KEY=local-responses-key ANTHROPIC_API_KEY=local-fixture-key
export RYZHUB_FIXTURE_PORT=9181 RYZHUB_RESPONSES_FIXTURE_PORT=9182
export RYZHUB_CONFIG="$PWD/var/m3/config.toml"
python3 - <<'PY'
import pathlib,json,os
p=pathlib.Path.cwd();q=json.dumps
pathlib.Path(os.environ['RYZHUB_CONFIG']).write_text(f'''listen = "127.0.0.1:8280"
allowed_origins = ["http://localhost:3000", "http://127.0.0.1:3000"]
[policy]
models = ["mock", "fixture-model", "gpt-5.4", "gpt-5.2", "claude-sonnet-4-6", "claude-opus-4-6"]
harnesses = ["mock", "chat", "codex", "claude"]
max_sandboxes = 8
allow_full_auto = false
[llm]
enabled = true
listen = "127.0.0.1:8281"
gateway_url = "http://127.0.0.1:8281"
unix_socket = {q(str(p/'var/execution/gateway/llm-m3.sock'))}
upstream_url = "http://127.0.0.1:9181"
responses_url = "http://127.0.0.1:9182"
model = "fixture-model"
responses_model = "gpt-5.4"
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
image = {q((p/'var/m3/image.txt').read_text().strip())}
runtime = "runc"
memory_bytes = 1073741824
pids_limit = 256
[work]
enabled = true
model = "gpt-5.4"
claude_model = "claude-sonnet-4-6"
storage_root = {q(str(p/'var/m3/blobs'))}
secondary_root = {q(str(p/'var/m3/secondary'))}
''')
PY
cargo build -p ryzhub-server
python3 scripts/anthropic-fixture.py > var/m3/anthropic-fixture.log 2>&1 &
m3_anthropic_pid=$!
python3 scripts/responses-fixture.py > var/m3/responses-fixture.log 2>&1 &
m3_responses_pid=$!
trap 'kill "$m3_anthropic_pid" "$m3_responses_pid" 2>/dev/null || true' EXIT
target/debug/ryzhub-server migrate
target/debug/ryzhub-server all
