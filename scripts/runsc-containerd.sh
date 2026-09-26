#!/bin/sh
# runsc sidecars can retain the CLI stdout pipe. create has no stdout protocol.
for arg do
  if [ "$arg" = create ]; then
    log=$(mktemp /state/runsc-create.XXXXXX)
    /usr/local/bin/runsc "$@" > "$log" 2>&1
    status=$?
    if [ "$status" -ne 0 ]; then cat "$log" >&2; fi
    exit "$status"
  fi
done
exec /usr/local/bin/runsc "$@"
