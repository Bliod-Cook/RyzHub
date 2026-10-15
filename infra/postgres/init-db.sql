-- Development credentials only. Production credentials are supplied out of band.
CREATE ROLE ryzhub_app LOGIN PASSWORD 'local-app-only' NOSUPERUSER NOBYPASSRLS;
CREATE ROLE ryzhub_worker LOGIN PASSWORD 'local-worker-only' NOSUPERUSER BYPASSRLS;
GRANT CONNECT ON DATABASE ryzhub TO ryzhub_app, ryzhub_worker;
ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO ryzhub_app;
ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT SELECT ON TABLES TO ryzhub_worker;

