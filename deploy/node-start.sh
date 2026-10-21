#!/bin/sh
# Puts what ryzhub-host expects in its state directory (Codex, the gVisor wrapper and its
# configuration), then starts the node as the development stack does.
set -eu
mkdir -p /state/bin
for file in codex runsc-containerd; do
  # A new file beside the old one: running sandboxes keep what they started with.
  if ! cmp -s "/opt/ryzhub/$file" "/state/bin/$file"; then
    cp "/opt/ryzhub/$file" "/state/bin/$file.new"
    mv "/state/bin/$file.new" "/state/bin/$file"
  fi
done
cp /opt/ryzhub/runsc.toml /state/runsc.toml
exec /bin/sh /entrypoint.sh
