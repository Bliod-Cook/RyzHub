#!/usr/bin/env bash
# Load a Docker image into an execution node's containerd and print its digest-pinned
# reference, the form `[sandbox] image` and `deps` take.
#
#   scripts/load-image.sh NODE IMAGE    NODE: the node's container; IMAGE: a fully qualified
#                                       name with a tag (docker.io/library/name:tag)
set -euo pipefail
node=$1 image=$2
ctr=(docker exec -i "$node" ctr --address /run/ryzhub-containerd.sock --namespace ryzhub)
docker save "$image" | "${ctr[@]}" images import - >/dev/null
digest=$("${ctr[@]}" images list | awk -v name="$image" '$1==name {print $3}')
test -n "$digest"
reference="${image%:*}@$digest"
"${ctr[@]}" images tag --force "$image" "$reference" >/dev/null
printf '%s\n' "$reference"
