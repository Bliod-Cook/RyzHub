#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p var/execution/bin
codex_path="${RYZHUB_CODEX_BINARY:-$(command -v codex)}"
test "$("$codex_path" --version)" = 'codex-cli 0.154.0'
if [[ ! -f var/execution/bin/codex ]] || ! cmp -s "$codex_path" var/execution/bin/codex; then
 cp --reflink=auto "$codex_path" var/execution/bin/codex.new
 mv var/execution/bin/codex.new var/execution/bin/codex
fi
if [[ ! -f var/execution/bin/runsc ]] || ! var/execution/bin/runsc --version | grep -q 'release-20260921.0'; then
  source_url=https://storage.googleapis.com/gvisor/releases/release/20260921.0/x86_64
  curl -fsSL "$source_url/gvisor.tar.bz2" -o var/execution/gvisor.tar.bz2
  curl -fsSL "$source_url/gvisor.tar.bz2.sha512" -o var/execution/gvisor.tar.bz2.sha512
  python3 - <<'PY'
import pathlib,hashlib,tarfile
p=pathlib.Path('var/execution/gvisor.tar.bz2')
assert hashlib.sha512(p.read_bytes()).hexdigest()==p.with_suffix('.bz2.sha512').read_text().split()[0]
with tarfile.open(p) as t:t.extractall('var/execution/bin',filter='data')
PY
fi
cp scripts/runsc-containerd.sh var/execution/bin/runsc-containerd
chmod +x var/execution/bin/runsc-containerd
cp scripts/runsc.toml var/execution/runsc.toml
./scripts/start-execution-node.sh
# Install gVisor in the running node with the entrypoint's own install step.
docker exec -i ryzhub-execution sh -s install-gvisor < scripts/execution-node-entrypoint.sh
