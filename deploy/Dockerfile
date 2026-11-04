# syntax=docker/dockerfile:1
# RyzHub's images, built from the repository root (compose.yaml here does that):
#   server  ryzhub-server, which plays every server role
#   node    an execution node: containerd, gVisor, ryzhub-host and Codex
#   web     Caddy with the web client; it also terminates TLS for the API

FROM rust:1.98-trixie AS toolchain
RUN apt-get update && apt-get install -y --no-install-recommends musl-tools \
 && rm -rf /var/lib/apt/lists/* \
 && rustup target add x86_64-unknown-linux-musl
WORKDIR /src

FROM toolchain AS rust
COPY Cargo.toml Cargo.lock ./
COPY .sqlx .sqlx
COPY crates crates
COPY migrations migrations
COPY proto proto
# Sandboxes run the node's binaries, so those are static (musl).
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    SQLX_OFFLINE=true cargo build --release --locked -p ryzhub-server \
 && CC_x86_64_unknown_linux_musl=musl-gcc RUSTFLAGS='-C linker=rust-lld' \
    cargo build --release --locked -p ryzhub-execution --bins --target x86_64-unknown-linux-musl \
 && mkdir /out \
 && cp target/release/ryzhub-server target/x86_64-unknown-linux-musl/release/ryzhub-host \
    target/x86_64-unknown-linux-musl/release/ryzhub-guest /out/

FROM debian:trixie-slim AS server
# git fetches plugins from repositories.
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates git \
 && rm -rf /var/lib/apt/lists/*
COPY --from=rust /out/ryzhub-server /usr/local/bin/
USER 65534
ENTRYPOINT ["ryzhub-server"]
CMD ["all"]

FROM debian:trixie-slim AS node-downloads
RUN apt-get update && apt-get install -y --no-install-recommends bzip2 ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*
# Bump together with scripts/prepare-runtimes.sh.
ARG GVISOR_RELEASE=20260921.0
ARG GVISOR_SHA512=7c899979bed334f0987888c41e545cede8e1f7e67a257978c8de244c3867e32bc30d08491e9af36720273c502acd01d8d86c9844ebcbe125790c43fe6bdf563e
# Checkpoints record the Codex version and restore only under the same one: bump together
# with CODEX_VERSION in crates/execution/src/native.rs and scripts/prepare-runtimes.sh.
ARG CODEX_VERSION=0.154.0
ARG CODEX_SHA256=d7e18b2597ae8f242f5f31ee9e90deef48dbc9edd634d9868fb6435d08c07f02
WORKDIR /out
RUN curl -fsSL "https://storage.googleapis.com/gvisor/releases/release/$GVISOR_RELEASE/x86_64/gvisor.tar.bz2" -o /tmp/gvisor.tar.bz2 \
 && echo "$GVISOR_SHA512  /tmp/gvisor.tar.bz2" | sha512sum -c - \
 && tar -xjf /tmp/gvisor.tar.bz2 -C /out \
 && curl -fsSL "https://github.com/openai/codex/releases/download/rust-v$CODEX_VERSION/codex-x86_64-unknown-linux-musl.tar.gz" -o /tmp/codex.tar.gz \
 && echo "$CODEX_SHA256  /tmp/codex.tar.gz" | sha256sum -c - \
 && tar -xzf /tmp/codex.tar.gz -C /tmp \
 && install -m 0755 /tmp/codex-x86_64-unknown-linux-musl /out/codex \
 && test "$(/out/codex --version)" = "codex-cli $CODEX_VERSION"

# The image CI and the development stack run their node in.
FROM docker:28-dind@sha256:2a232a42256f70d78e3cc5d2b5d6b3276710a0de0596c145f627ecfae90282ac AS node
COPY --from=node-downloads /out/runsc /out/containerd-shim-runsc-v1 /usr/local/bin/
COPY --from=node-downloads /out/gvisor-bin /usr/local/bin/gvisor-bin
COPY --from=node-downloads /out/codex /opt/ryzhub/codex
COPY --from=rust /out/ryzhub-host /out/ryzhub-guest /binaries/
COPY --chmod=755 scripts/runsc-containerd.sh /opt/ryzhub/runsc-containerd
COPY scripts/runsc.toml /opt/ryzhub/runsc.toml
COPY --chmod=755 scripts/execution-node-entrypoint.sh /entrypoint.sh
COPY --chmod=755 deploy/node-start.sh /opt/ryzhub/start.sh
ENV RYZHUB_EXECUTION_ROOT=/state \
    CONTAINERD_ADDRESS=/run/ryzhub-containerd.sock \
    RYZHUB_GUEST_BINARY=/binaries/ryzhub-guest
ENTRYPOINT ["/opt/ryzhub/start.sh"]

FROM toolchain AS web-build
RUN apt-get update && apt-get install -y --no-install-recommends unzip xz-utils zip \
 && rm -rf /var/lib/apt/lists/*
RUN git clone --depth 1 --branch 3.47.5 https://github.com/flutter/flutter.git /opt/flutter
# Flutter's archives carry uids that rootless builders cannot chown to.
ENV PATH=/opt/flutter/bin:$PATH \
    TAR_OPTIONS=--no-same-owner
RUN flutter config --no-analytics --no-cli-animations && flutter precache --web
RUN rustup component add rustfmt \
 && rustup toolchain install nightly-2026-09-25 --profile minimal --component rust-src \
    --target wasm32-unknown-unknown
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo install --locked flutter_rust_bridge_codegen@2.11.1 cargo-expand@1.0.126 wasm-pack@0.15.0
COPY Cargo.toml Cargo.lock flutter_rust_bridge.yaml ./
COPY .sqlx .sqlx
COPY crates crates
COPY migrations migrations
COPY proto proto
COPY scripts scripts
COPY client client
# Where the app finds the API; the web image is built for one instance.
ARG RYZHUB_SERVER_URL
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    --mount=type=cache,target=/root/.pub-cache \
    test -n "$RYZHUB_SERVER_URL" \
 && ./scripts/build-web.sh --dart-define=RYZHUB_SERVER_URL="$RYZHUB_SERVER_URL"

FROM caddy:2-alpine AS web
COPY --from=web-build /src/client/build/web /srv
COPY deploy/Caddyfile /etc/caddy/Caddyfile
