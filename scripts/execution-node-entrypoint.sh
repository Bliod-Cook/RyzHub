#!/bin/sh
set -eu
export RYZHUB_RUNSC_CONFIG=/state/runsc.toml
if [ -f /state/bin/runsc ]; then
 for binary in runsc containerd-shim-runsc-v1; do
  if ! cmp -s "/state/bin/$binary" "/usr/local/bin/$binary"; then
   cp "/state/bin/$binary" "/usr/local/bin/$binary.new"
   mv "/usr/local/bin/$binary.new" "/usr/local/bin/$binary"
  fi
 done
 if [ ! -d /usr/local/bin/gvisor-bin ]; then cp -a /state/bin/gvisor-bin /usr/local/bin/; fi
fi
# Delegate only this Docker container's private cgroup subtree to nested tasks.
if [ -f /sys/fs/cgroup/cgroup.controllers ]; then
  mkdir -p /sys/fs/cgroup/init
  for node_pid in $(cat /sys/fs/cgroup/cgroup.procs); do
    echo "$node_pid" > /sys/fs/cgroup/init/cgroup.procs 2>/dev/null || true
  done
  sed 's/ / +/g; s/^/+/' /sys/fs/cgroup/cgroup.controllers > /sys/fs/cgroup/cgroup.subtree_control
fi
containerd --address /run/ryzhub-containerd.sock --root /state/containerd-root --state /run/ryzhub-containerd > /state/containerd.log 2>&1 &
node_containerd_pid=$!
trap 'kill "$node_containerd_pid"; exit' INT TERM
while [ ! -S /run/ryzhub-containerd.sock ]; do sleep 0.1; done
# Keep containerd and its shims alive across host-agent restarts.
while kill -0 "$node_containerd_pid" 2>/dev/null; do
  /binaries/ryzhub-host &
  node_host_pid=$!
  echo "$node_host_pid" > /state/host.pid
  wait "$node_host_pid" || true
  sleep 0.2
done
