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
if [[ ! -f var/execution/bin/runsc ]] || ! var/execution/bin/runsc --version | rg -q 'release-20260921.0'; then
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
cat > var/execution/runsc.toml <<'EOF'
binary_name = "/state/bin/runsc-containerd"
[runsc_config]
  overlay2 = "none"
  platform = "systrap"
  host-uds = "all"
  network = "host"
EOF
./scripts/start-execution-node.sh
# Scope these binaries to the dedicated execution node.
docker exec ryzhub-execution sh -c 'for b in runsc containerd-shim-runsc-v1; do if ! cmp -s "/state/bin/$b" "/usr/local/bin/$b"; then cp "/state/bin/$b" "/usr/local/bin/$b.new"; mv "/usr/local/bin/$b.new" "/usr/local/bin/$b"; fi; done; if [ ! -d /usr/local/bin/gvisor-bin ]; then cp -a /state/bin/gvisor-bin /usr/local/bin/; fi' 
