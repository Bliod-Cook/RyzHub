#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
export CARGO_INCREMENTAL=0
mkdir -p var/m3
# Resolve and record an immutable base, then pin the resulting containerd image digest.
if [[ ! -s var/m3/base-image.txt ]]; then
 docker pull node:22-alpine > var/m3/base-pull.log 2>&1
 docker image inspect node:22-alpine --format '{{index .RepoDigests 0}}' > var/m3/base-image.txt
fi
docker build --build-arg "BASE=$(cat var/m3/base-image.txt)" -t ryzhub-m3-base:local harness/claude > var/m3/image-build.log 2>&1
docker save ryzhub-m3-base:local -o var/execution/m3-image.tar
docker exec ryzhub-m1-execution ctr --address /run/ryzhub-containerd.sock --namespace ryzhub images import /state/m3-image.tar > var/m3/image-import.log 2>&1
m3_digest=$(docker exec ryzhub-m1-execution ctr --address /run/ryzhub-containerd.sock --namespace ryzhub images list | awk '$1=="docker.io/library/ryzhub-m3-base:local" {print $3}')
test -n "$m3_digest"
m3_image="docker.io/library/ryzhub-m3-base@$m3_digest"
docker exec ryzhub-m1-execution ctr --address /run/ryzhub-containerd.sock --namespace ryzhub images tag --force docker.io/library/ryzhub-m3-base:local "$m3_image" >/dev/null
printf '%s\n' "$m3_image" > var/m3/image.txt
rm var/execution/m3-image.tar
printf 'M3 sandbox image: %s\n' "$m3_image"
