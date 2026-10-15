#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
export SQLX_OFFLINE=true
set -a
source "${RYZHUB_ENV_FILE:-.env.example}"
set +a
./scripts/start-execution-node.sh
mkdir -p var/chat
if [[ ! -f var/vault.env ]]; then
  (umask 077; python3 - <<'PY' > var/vault.env
import secrets,base64
for name in ['RYZHUB_VAULT_KEY']:
    print(name+'='+base64.urlsafe_b64encode(secrets.token_bytes(32)).decode().rstrip('='))
PY
  )
fi
set -a
source var/vault.env
set +a
if [[ "${1:-}" == --fixture ]]; then
  export ANTHROPIC_API_KEY=local-fixture-key
  export RYZHUB_MODEL=fixture-model
  export RYZHUB_UPSTREAM_URL="http://127.0.0.1:${RYZHUB_FIXTURE_PORT:-9081}"
  python3 scripts/anthropic-fixture.py &
  fixture_pid=$!
  trap 'kill "$fixture_pid" 2>/dev/null || true' EXIT
else
  : "${RYZHUB_MODEL:?set RYZHUB_MODEL to an available Anthropic model}"
fi
export RYZHUB_CONFIG="$PWD/var/chat/config.toml"
python3 - <<'PY'
import json,os,pathlib
root=pathlib.Path.cwd()
quote=json.dumps
model=os.environ['RYZHUB_MODEL']
text=f'''listen = {quote(os.environ.get('RYZHUB_LISTEN','127.0.0.1:8080'))}
allowed_origins = ["http://localhost:3000", "http://127.0.0.1:3000"]
[policy]
models = ["mock", {quote(model)}]
harnesses = ["mock", "chat"]
max_sandboxes = 4
[llm]
enabled = true
listen = {quote(os.environ.get('RYZHUB_LLM_LISTEN','127.0.0.1:8081'))}
gateway_url = {quote('http://'+os.environ.get('RYZHUB_LLM_LISTEN','127.0.0.1:8081'))}
unix_socket = {quote(str(root/'var/execution/gateway/llm.sock'))}
upstream_url = {quote(os.environ.get('RYZHUB_UPSTREAM_URL','https://api.anthropic.com'))}
model = {quote(model)}
[sandbox]
enabled = true
host_socket = {quote(str(root/'var/execution/host.sock'))}
image = {quote((root/'var/execution/image.txt').read_text().strip())}
runtime = "runc"
'''
pathlib.Path(os.environ['RYZHUB_CONFIG']).write_text(text)
PY
docker compose up -d --wait
cargo run -p ryzhub-server -- migrate
cargo run -p ryzhub-server -- all
