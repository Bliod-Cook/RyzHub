#!/bin/sh
# The roles the migrations grant to, made once when the data directory is new. The server
# runs as ryzhub_app (row-level security applies); owners read across organizations as
# ryzhub_worker.
set -eu
psql -v ON_ERROR_STOP=1 --username "$POSTGRES_USER" --dbname "$POSTGRES_DB" \
  -v app="$RYZHUB_APP_PASSWORD" -v worker="$RYZHUB_WORKER_PASSWORD" -v db="$POSTGRES_DB" <<'SQL'
CREATE ROLE ryzhub_app LOGIN PASSWORD :'app' NOSUPERUSER NOBYPASSRLS;
CREATE ROLE ryzhub_worker LOGIN PASSWORD :'worker' NOSUPERUSER BYPASSRLS;
GRANT CONNECT ON DATABASE :"db" TO ryzhub_app, ryzhub_worker;
ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO ryzhub_app;
ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT SELECT ON TABLES TO ryzhub_worker;
SQL
