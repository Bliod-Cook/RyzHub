FROM groonga/pgroonga:4.0.4-alpine-18
ARG PGVECTOR_VERSION=0.8.2
RUN apk add --no-cache --virtual .vector-build build-base clang19 llvm19-dev git \
    && git clone --branch v${PGVECTOR_VERSION} --depth 1 https://github.com/pgvector/pgvector.git /tmp/pgvector \
    && make -C /tmp/pgvector OPTFLAGS='' \
    && make -C /tmp/pgvector install \
    && rm -rf /tmp/pgvector \
    && apk del .vector-build
COPY --chmod=755 init-roles.sh /docker-entrypoint-initdb.d/
