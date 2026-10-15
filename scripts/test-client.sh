#!/usr/bin/env bash
# Client checks: widget tests render with the kernel built for this machine.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build -q -p ryzhub-bridge --release
cd client
dart format --output=none --set-exit-if-changed lib test
flutter analyze
flutter test "$@"
