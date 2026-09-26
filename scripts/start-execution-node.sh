#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
node_name=ryzhub-m1-execution
node_root="$PWD/var/execution"
mkdir -p "$node_root/gateway"
chmod 700 "$node_root"
rustup target add x86_64-unknown-linux-musl
export CC_x86_64_unknown_linux_musl="${CC_x86_64_unknown_linux_musl:-musl-gcc}"
RUSTFLAGS='-C linker=rust-lld' cargo build -p ryzhub-execution --target x86_64-unknown-linux-musl
# Install outside Cargo's disposable target directory. A cargo clean must not
# leave a live node holding a bind mount to an unlinked build directory.
mkdir -p "$node_root/runtime"
for binary in ryzhub-host ryzhub-guest; do
  cp --reflink=auto "target/x86_64-unknown-linux-musl/debug/$binary" "$node_root/runtime/$binary.new"
  mv "$node_root/runtime/$binary.new" "$node_root/runtime/$binary"
done
if ! docker inspect "$node_name" >/dev/null 2>&1; then
  docker run -d --name "$node_name" --privileged \
    --mount "type=bind,src=$node_root,dst=/state" \
    --mount "type=bind,src=$node_root/runtime,dst=/binaries,readonly" \
    --mount "type=bind,src=$PWD/scripts/execution-node-entrypoint.sh,dst=/entrypoint.sh,readonly" \
    -e RYZHUB_EXECUTION_ROOT=/state -e CONTAINERD_ADDRESS=/run/ryzhub-containerd.sock \
    -e RYZHUB_GUEST_BINARY=/binaries/ryzhub-guest -e "RYZHUB_CONTROL_UID=$(id -u)" \
    --entrypoint /bin/sh docker:28-dind@sha256:2a232a42256f70d78e3cc5d2b5d6b3276710a0de0596c145f627ecfae90282ac /entrypoint.sh
else
  docker start "$node_name" >/dev/null
  docker exec "$node_name" sh -c 'umount -l /binaries; mkdir -p /binaries; mount --bind /state/runtime /binaries'
  docker exec "$node_name" sh -c 'kill "$(cat /state/host.pid)" 2>/dev/null || true'
fi
node_ready=false
for _ in {1..100}; do
  if [[ -S "$node_root/host.sock" ]] && python3 - "$node_root/host.sock" <<'PYPROBE'
import socket,sys
try:
 with socket.socket(socket.AF_UNIX) as s:
  s.settimeout(1);s.connect(sys.argv[1])
except OSError:sys.exit(1)
PYPROBE
  then node_ready=true; break; fi
  sleep 0.1
done
[[ "$node_ready" == true ]] || { echo "Execution node did not become ready" >&2; exit 1; }
docker exec "$node_name" ctr --address /run/ryzhub-containerd.sock --namespace ryzhub images pull docker.io/library/alpine:3.22 > "$node_root/image-pull.log" 2>&1
node_digest=$(docker exec "$node_name" ctr --address /run/ryzhub-containerd.sock --namespace ryzhub images list | awk '$1=="docker.io/library/alpine:3.22" {print $3}')
test -n "$node_digest"
node_image="docker.io/library/alpine@$node_digest"
docker exec "$node_name" ctr --address /run/ryzhub-containerd.sock --namespace ryzhub images tag --force docker.io/library/alpine:3.22 "$node_image" >/dev/null
printf '%s\n' "$node_image" > "$node_root/image.txt"
printf 'Execution socket: %s/host.sock\nImage: %s\n' "$node_root" "$node_image"
