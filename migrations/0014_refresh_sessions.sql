-- Signed-in devices. A browser keeps its refresh token in an HttpOnly cookie and trades it for
-- short-lived access tokens; access tokens still never reach disk. Each refresh rotates the
-- token. The previous one stays valid for a short grace so that tabs refreshing at once do not
-- sign each other out; presenting it later revokes the device as a replayed token.
CREATE TABLE auth_refresh (
    id uuid PRIMARY KEY,
    user_id uuid NOT NULL REFERENCES users,
    refresh_hash bytea NOT NULL UNIQUE,
    previous_hash bytea UNIQUE,
    rotated_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NOT NULL,
    revoked_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now()
);
-- The device whose refresh issued an access token; revoking the device revokes them too.
ALTER TABLE auth_sessions ADD COLUMN device_id uuid REFERENCES auth_refresh;
CREATE INDEX auth_sessions_device ON auth_sessions(device_id) WHERE device_id IS NOT NULL;
GRANT SELECT,INSERT,UPDATE,DELETE ON auth_refresh TO ryzhub_app;
