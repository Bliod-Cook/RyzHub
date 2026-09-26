#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
export CARGO_INCREMENTAL=0
mkdir -p var/m1 var/m2
if [[ ! -f var/m1/secrets.env ]]; then
 (umask 077; python3 - <<'PYKEY' > var/m1/secrets.env
import secrets,base64
for name in ['RYZHUB_SIGNING_KEY','RYZHUB_VAULT_KEY']:
 print(name+'='+base64.urlsafe_b64encode(secrets.token_bytes(32)).decode().rstrip('='))
PYKEY
 )
fi
docker compose up -d --wait
./scripts/prepare-m2.sh
cargo build -p ryzhub-server
set -a
source "${RYZHUB_ENV_FILE:-.env.example}"
source var/m1/secrets.env
set +a
export OPENAI_API_KEY=local-responses-key
export ANTHROPIC_API_KEY=local-fixture-key
export RYZHUB_CONFIG="$PWD/var/m2/config.toml"
mkdir -p var/m2
python3 - <<'PY'
import json,pathlib,os
p=pathlib.Path.cwd();q=json.dumps
pathlib.Path(os.environ['RYZHUB_CONFIG']).write_text(f'''listen = "127.0.0.1:8180"
allowed_origins = ["http://localhost:3000", "http://127.0.0.1:3000"]
[policy]
models = ["mock", "fixture-model", "gpt-5.4"]
harnesses = ["mock", "chat", "codex"]
max_sandboxes = 8
[llm]
enabled = true
listen = "127.0.0.1:8181"
gateway_url = "http://127.0.0.1:8181"
unix_socket = {q(str(p/'var/execution/gateway/llm-m2.sock'))}
upstream_url = "http://127.0.0.1:9081"
responses_url = "http://127.0.0.1:9082"
model = "fixture-model"
[sandbox]
enabled = true
host_socket = {q(str(p/'var/execution/host.sock'))}
image = {q((p/'var/execution/image.txt').read_text().strip())}
runtime = "runc"
memory_bytes = 1073741824
pids_limit = 256
[work]
enabled = true
model = "gpt-5.4"
storage_root = {q(str(p/'var/m2/blobs'))}
secondary_root = {q(str(p/'var/m2/secondary'))}
''')
PY
python3 scripts/responses-fixture.py > var/m2/fixture.log 2>&1 &
fixture_pid=$!
python3 scripts/anthropic-fixture.py > var/m2/anthropic-fixture.log 2>&1 &
anthropic_pid=$!
trap 'kill "$fixture_pid" "$anthropic_pid" 2>/dev/null || true' EXIT
target/debug/ryzhub-server migrate
target/debug/ryzhub-server all
