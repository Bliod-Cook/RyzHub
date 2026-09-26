# The preparation script resolves this tag to a digest before building.
ARG BASE
FROM ${BASE} AS build
WORKDIR /opt/ryzhub-claude
COPY package.json package-lock.json tsconfig.json adapter.ts ./
RUN npm ci && npm run build && npm prune --omit=dev && npm cache clean --force
FROM ${BASE}
COPY --from=build /opt/ryzhub-claude /opt/ryzhub-claude
RUN mkdir -p /workspace /home/agent /project
