#!/usr/bin/env bash
# Client checks: widget tests render with the kernel built for this machine.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build -q -p ryzhub-bridge --release
python3 scripts/gen-tokens.py --check
cd client
flutter analyze
flutter test "$@"
