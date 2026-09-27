#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
# Requires Flutter 3.47.5, frb_codegen 2.11.1, wasm-pack and the pinned nightly below.
pushd client >/dev/null
flutter pub get
popd >/dev/null
flutter_rust_bridge_codegen generate
cargo fmt -p ryzhub-bridge
cd client
flutter_rust_bridge_codegen build-web --release --rust-root ../crates/bridge \
  --wasm-pack-rustup-toolchain nightly-2026-09-25 --output "$PWD/web"
flutter build web --release --no-web-resources-cdn --no-wasm-dry-run "$@"
