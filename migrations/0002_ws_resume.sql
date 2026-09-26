CREATE TABLE ws_resumes (
    token_hash bytea PRIMARY KEY,
    session_id uuid NOT NULL REFERENCES auth_sessions,
    device_id text NOT NULL,
    subscriptions jsonb NOT NULL DEFAULT '{}',
    expires_at timestamptz NOT NULL
);
REVOKE ALL ON ws_resumes FROM ryzhub_worker;
GRANT SELECT,INSERT,UPDATE,DELETE ON ws_resumes TO ryzhub_app;
