#!/usr/bin/env bash
# Build a sandbox dependency layer and load it into the execution node's containerd.
#
#   scripts/build-deps.sh DIR                          the layer is DIR's tree
#   scripts/build-deps.sh --dockerfile FILE [CONTEXT]  the layer is the built image's tree
#
# Prints the digest-pinned reference for `[sandbox] deps` or a profile's `deps`. The layer is
# mounted read-only over the base image, so it may hold only what it adds (package caches,
# tools under /usr/local, ...). Build it from trusted sources only: every organization's
# sandboxes read it.
set -euo pipefail
cd "$(dirname "$0")/.."
node="${RYZHUB_EXECUTION_NODE:-ryzhub-m1-execution}"
name="ryzhub.local/deps:build"
ctr=(docker exec -i "$node" ctr --address /run/ryzhub-containerd.sock --namespace ryzhub)

if [[ "${1:-}" == "--dockerfile" ]]; then
  docker build --quiet -t "$name" -f "$2" "${3:-.}" >/dev/null
elif [[ -d "${1:-}" ]]; then
  tar -C "$1" -c . | docker import - "$name" >/dev/null
else
  echo "usage: $0 DIR | --dockerfile FILE [CONTEXT]" >&2
  exit 2
fi
docker save "$name" | "${ctr[@]}" images import - >/dev/null
digest=$("${ctr[@]}" images list | awk -v n="$name" '$1==n {print $3}')
test -n "$digest"
reference="ryzhub.local/deps@$digest"
"${ctr[@]}" images tag --force "$name" "$reference" >/dev/null
printf '%s\n' "$reference"
