# The Work sandbox base: the Claude Agent SDK adapter, and OpenCode for the `opencode` ACP
# harness. BASE is pinned by digest, so a rebuild starts from the same Node.js and Alpine.
ARG BASE=node:22-alpine@sha256:0a7108bf6c7bf5de370ffb1a3ed6be93d405b43ff159f681a8d18c0e2bc2e402
FROM ${BASE} AS build
WORKDIR /opt/ryzhub-claude
COPY package.json package-lock.json tsconfig.json adapter.ts ./
RUN npm ci && npm run build && npm prune --omit=dev && npm cache clean --force
# Pinned with the platform's OpenCode profile (`version`); npm checks the package's integrity.
ARG OPENCODE_VERSION=1.19.0
WORKDIR /opt/opencode
RUN npm pack "opencode-linux-x64-musl@${OPENCODE_VERSION}" \
 && tar xzf opencode-linux-x64-musl-*.tgz \
 && install -m 0755 package/bin/opencode /opt/opencode/opencode \
 && rm -rf package ./*.tgz && npm cache clean --force
FROM ${BASE}
COPY --from=build /opt/ryzhub-claude /opt/ryzhub-claude
COPY --from=build /opt/opencode/opencode /usr/local/bin/opencode
# OpenCode searches with ripgrep, which it would otherwise download on first use.
RUN apk add --no-cache ripgrep && mkdir -p /workspace /home/agent /project
