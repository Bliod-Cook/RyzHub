-- Envelope-encrypted secrets. Each value is encrypted with its own data key, and that key is
-- wrapped by the vault key `kek_id` names, so rotating the vault key rewraps keys only.
-- Other tables store a secret_id, never ciphertext.
CREATE TABLE secrets (
    id uuid PRIMARY KEY,
    org_id uuid NOT NULL REFERENCES organizations,
    ciphertext bytea NOT NULL,
    wrapped_dek bytea NOT NULL,
    kek_id smallint NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (org_id, id)
);
CREATE INDEX secrets_kek ON secrets(kek_id);

-- Credentials move to secrets; `ryzhub-server migrate-secrets` converts existing ones. A
-- revoked credential's secret is deleted, so it holds neither.
ALTER TABLE model_credentials
    ADD COLUMN secret_id uuid,
    ALTER COLUMN encrypted_key DROP NOT NULL,
    ADD CONSTRAINT model_credentials_secret_tenant
        FOREIGN KEY (org_id, secret_id) REFERENCES secrets(org_id, id),
    ADD CONSTRAINT model_credentials_one_key
        CHECK (num_nonnulls(encrypted_key, secret_id) = 1 OR revoked_at IS NOT NULL);

-- Durable background jobs, enqueued in the transaction whose commit should cause them. This
-- also carries external side effects (notifications, webhooks): a separate outbox would have
-- the same semantics.
CREATE TABLE jobs (
    id uuid PRIMARY KEY,
    org_id uuid NOT NULL REFERENCES organizations,
    kind text NOT NULL,
    payload jsonb NOT NULL DEFAULT '{}',
    priority smallint NOT NULL DEFAULT 0,
    state text NOT NULL DEFAULT 'queued' CHECK (state IN ('queued', 'running', 'completed', 'dead')),
    run_at timestamptz NOT NULL DEFAULT now(),
    attempts integer NOT NULL DEFAULT 0,
    max_attempts integer NOT NULL DEFAULT 8 CHECK (max_attempts > 0),
    lease_until timestamptz,
    claim_token uuid,
    dedupe_key text,
    error text,
    created_at timestamptz NOT NULL DEFAULT now(),
    finished_at timestamptz,
    UNIQUE (org_id, id)
);
-- At most one queued job per key; a running one does not absorb later triggers.
CREATE UNIQUE INDEX jobs_dedupe ON jobs(org_id, kind, dedupe_key)
    WHERE dedupe_key IS NOT NULL AND state = 'queued';
CREATE INDEX jobs_due ON jobs(org_id, kind, priority DESC, run_at) WHERE state IN ('queued', 'running');
CREATE INDEX jobs_finished ON jobs(finished_at) WHERE state IN ('completed', 'dead');

DO $$ DECLARE t text; BEGIN FOREACH t IN ARRAY ARRAY['secrets', 'jobs'] LOOP
    EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
    EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY', t);
    EXECUTE format('CREATE POLICY org_isolation ON %I USING (org_id=nullif(current_setting(''app.org_id'',true),'''')::uuid) WITH CHECK (org_id=nullif(current_setting(''app.org_id'',true),'''')::uuid)', t);
    EXECUTE format('GRANT SELECT,INSERT,UPDATE,DELETE ON %I TO ryzhub_app', t);
END LOOP; END $$;
-- The worker role finds due jobs across organizations; it never reads secrets.
GRANT SELECT ON jobs TO ryzhub_worker;
REVOKE ALL ON secrets FROM ryzhub_worker;
