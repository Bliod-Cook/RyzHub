#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
export CARGO_INCREMENTAL=0
node=ryzhub-execution
ctr=(docker exec "$node" ctr --address /run/ryzhub-containerd.sock --namespace ryzhub)
mkdir -p var/execution
# Resolve and record an immutable base, then pin the resulting containerd image digest.
if [[ ! -s var/execution/harness-base.txt ]]; then
 docker pull node:22-alpine > var/execution/harness-base-pull.log 2>&1
 docker image inspect node:22-alpine --format '{{index .RepoDigests 0}}' > var/execution/harness-base.txt
fi
docker build --build-arg "BASE=$(cat var/execution/harness-base.txt)" -t ryzhub-harness:local harness/claude > var/execution/harness-image-build.log 2>&1
docker save ryzhub-harness:local -o var/execution/harness-image.tar
"${ctr[@]}" images import /state/harness-image.tar > var/execution/harness-image-import.log 2>&1
digest=$("${ctr[@]}" images list | awk '$1=="docker.io/library/ryzhub-harness:local" {print $3}')
test -n "$digest"
image="docker.io/library/ryzhub-harness@$digest"
"${ctr[@]}" images tag --force docker.io/library/ryzhub-harness:local "$image" >/dev/null
printf '%s\n' "$image" > var/execution/harness-image.txt
rm var/execution/harness-image.tar
printf 'Harness image: %s\n' "$image"
