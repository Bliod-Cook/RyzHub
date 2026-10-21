#!/usr/bin/env bash
# Build a sandbox dependency layer and load it into the execution node's containerd.
#
#   scripts/build-deps.sh DIR                          the layer is DIR's tree
#   scripts/build-deps.sh --dockerfile FILE [CONTEXT]  the layer is the built image's tree
#
# Prints the digest-pinned reference for `[sandbox] deps`. The layer is mounted read-only over
# the base image, so it may hold only what it adds (package caches, tools under /usr/local, ...).
# Build it from trusted sources only: every organization's sandboxes read it.
# RYZHUB_NODE names the node's container.
set -euo pipefail
cd "$(dirname "$0")/.."
name="ryzhub.local/deps:build"

if [[ "${1:-}" == "--dockerfile" ]]; then
  docker build --quiet --provenance=false -t "$name" -f "$2" "${3:-.}" >/dev/null
elif [[ -d "${1:-}" ]]; then
  tar -C "$1" -c . | docker import - "$name" >/dev/null
else
  echo "usage: $0 DIR | --dockerfile FILE [CONTEXT]" >&2
  exit 2
fi
scripts/load-image.sh "${RYZHUB_NODE:-ryzhub-execution}" "$name"
