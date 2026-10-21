#!/usr/bin/env bash
# Build the sandbox image (harness/claude) and load it into an execution node. Prints the
# digest-pinned reference for `[sandbox] image`; RYZHUB_NODE names the node's container.
set -euo pipefail
cd "$(dirname "$0")/.."
image=docker.io/library/ryzhub-harness:local
docker build --provenance=false -t "$image" harness/claude >&2
scripts/load-image.sh "${RYZHUB_NODE:-ryzhub-execution}" "$image"
