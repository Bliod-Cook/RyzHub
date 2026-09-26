ALTER TABLE runs DROP CONSTRAINT runs_state_check;
ALTER TABLE runs ADD CONSTRAINT runs_state_check CHECK(state IN ('running','requires_action','completed','cancelled','failed'));
ALTER TABLE runs ADD COLUMN credential_id uuid;
ALTER TABLE runs ADD COLUMN parent_run_id uuid;
ALTER TABLE runs ADD COLUMN read_only boolean NOT NULL DEFAULT false;
ALTER TABLE runs ADD COLUMN error_code text;
ALTER TABLE runs ADD COLUMN seed_messages jsonb NOT NULL DEFAULT '[]';
ALTER TABLE runs ADD CONSTRAINT parent_run_tenant FOREIGN KEY(org_id,parent_run_id) REFERENCES runs(org_id,id);
DROP INDEX one_active_run;
CREATE UNIQUE INDEX one_active_run ON runs(conversation_id) WHERE state IN ('running','requires_action') AND NOT read_only;

CREATE TABLE model_credentials (
    id uuid PRIMARY KEY, org_id uuid NOT NULL REFERENCES organizations,
    owner_user_id uuid REFERENCES users, name text NOT NULL, provider text NOT NULL CHECK(provider='anthropic'),
    encrypted_key bytea NOT NULL, revoked_at timestamptz, created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE(org_id,id)
);
ALTER TABLE runs ADD CONSTRAINT run_credential_tenant FOREIGN KEY(org_id,credential_id) REFERENCES model_credentials(org_id,id);
CREATE TABLE pending_inputs (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, org_id uuid NOT NULL,
    run_id uuid NOT NULL, text text NOT NULL, kind text NOT NULL CHECK(kind IN ('message','steer')),
    item_id uuid NOT NULL, consumed_step integer, created_at timestamptz NOT NULL DEFAULT now(),
    FOREIGN KEY(org_id,run_id) REFERENCES runs(org_id,id),
    FOREIGN KEY(org_id,item_id) REFERENCES items(org_id,id)
);
CREATE TABLE loop_steps (
    org_id uuid NOT NULL, run_id uuid NOT NULL, step_no integer NOT NULL,
    request_messages jsonb NOT NULL, native_response jsonb,
    consumed_input_seq bigint NOT NULL DEFAULT 0, status text NOT NULL CHECK(status IN ('planned','completed')),
    attempt integer NOT NULL DEFAULT 1, created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY(run_id,step_no), FOREIGN KEY(org_id,run_id) REFERENCES runs(org_id,id)
);
CREATE TABLE sandboxes (
    id uuid PRIMARY KEY, org_id uuid NOT NULL, conversation_id uuid NOT NULL,
    run_id uuid NOT NULL, lease_epoch bigint NOT NULL, exec_epoch bigint NOT NULL,
    state text NOT NULL, handle jsonb NOT NULL, acked_gseq bigint NOT NULL DEFAULT 0,
    created_at timestamptz NOT NULL DEFAULT now(), UNIQUE(org_id,id),
    FOREIGN KEY(org_id,conversation_id) REFERENCES conversations(org_id,id),
    FOREIGN KEY(org_id,run_id) REFERENCES runs(org_id,id)
);
CREATE TABLE tool_calls (
    org_id uuid NOT NULL, run_id uuid NOT NULL, call_id text NOT NULL, step_no integer NOT NULL,
    name text NOT NULL, arguments jsonb NOT NULL, result jsonb, item_id uuid NOT NULL,
    status text NOT NULL CHECK(status IN ('planned','started','completed','outcome_unknown')),
    sandbox_id uuid, action_id uuid, started_at timestamptz, completed_at timestamptz,
    PRIMARY KEY(run_id,call_id), FOREIGN KEY(org_id,run_id) REFERENCES runs(org_id,id),
    FOREIGN KEY(org_id,item_id) REFERENCES items(org_id,id),
    FOREIGN KEY(org_id,sandbox_id) REFERENCES sandboxes(org_id,id),
    FOREIGN KEY(org_id,action_id) REFERENCES pending_actions(org_id,id)
);
CREATE TABLE guest_events (
    org_id uuid NOT NULL, sandbox_id uuid NOT NULL, gseq bigint NOT NULL,
    payload jsonb NOT NULL, created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY(sandbox_id,gseq), FOREIGN KEY(org_id,sandbox_id) REFERENCES sandboxes(org_id,id)
);
CREATE TABLE virtual_keys (
    id uuid PRIMARY KEY, org_id uuid NOT NULL, user_id uuid NOT NULL REFERENCES users,
    run_id uuid NOT NULL, sandbox_id uuid NOT NULL, lease_epoch bigint NOT NULL,
    models text[] NOT NULL, credential_id uuid, expires_at timestamptz NOT NULL, revoked_at timestamptz,
    FOREIGN KEY(org_id,run_id) REFERENCES runs(org_id,id),
    FOREIGN KEY(org_id,sandbox_id) REFERENCES sandboxes(org_id,id),
    FOREIGN KEY(org_id,credential_id) REFERENCES model_credentials(org_id,id)
);
CREATE TABLE usage_records (
    id uuid PRIMARY KEY, org_id uuid NOT NULL, user_id uuid NOT NULL REFERENCES users,
    run_id uuid NOT NULL, virtual_key_id uuid, model text NOT NULL,
    credential_id uuid, input_tokens bigint NOT NULL DEFAULT 0, output_tokens bigint NOT NULL DEFAULT 0,
    raw_usage jsonb NOT NULL DEFAULT '{}', estimated boolean NOT NULL DEFAULT true,
    status text NOT NULL DEFAULT 'started', upstream_status integer, session_id text, agent_id text, parent_agent_id text,
    created_at timestamptz NOT NULL DEFAULT now(), finished_at timestamptz,
    FOREIGN KEY(org_id,run_id) REFERENCES runs(org_id,id),
    FOREIGN KEY(org_id,credential_id) REFERENCES model_credentials(org_id,id)
);
CREATE TABLE model_rate_limits (
    org_id uuid NOT NULL REFERENCES organizations, scope text NOT NULL, minute bigint NOT NULL,
    requests integer NOT NULL, PRIMARY KEY(org_id,scope,minute)
);
DO $$ DECLARE t text; BEGIN
    FOREACH t IN ARRAY ARRAY['model_credentials','pending_inputs','loop_steps','sandboxes','tool_calls','guest_events','virtual_keys','usage_records','model_rate_limits'] LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY',t);
        EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY',t);
        EXECUTE format('CREATE POLICY org_isolation ON %I USING (org_id=nullif(current_setting(''app.org_id'',true),'''')::uuid) WITH CHECK (org_id=nullif(current_setting(''app.org_id'',true),'''')::uuid)',t);
        EXECUTE format('GRANT SELECT,INSERT,UPDATE,DELETE ON %I TO ryzhub_app',t);
    END LOOP;
END $$;
GRANT USAGE,SELECT ON SEQUENCE pending_inputs_id_seq TO ryzhub_app;
GRANT SELECT ON sandboxes TO ryzhub_worker;
