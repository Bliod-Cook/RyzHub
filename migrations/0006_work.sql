ALTER TABLE model_credentials DROP CONSTRAINT model_credentials_provider_check;
ALTER TABLE model_credentials ADD CONSTRAINT model_credentials_provider_check CHECK(provider IN ('anthropic','openai'));
ALTER TABLE usage_records ADD COLUMN provider text NOT NULL DEFAULT 'anthropic';
-- Expiry fences execution independently of the conversation Owner lease.
ALTER TABLE sandboxes ADD COLUMN node_lease_until timestamptz;
ALTER TABLE sandboxes ADD COLUMN persistent boolean NOT NULL DEFAULT false;
ALTER TABLE sandboxes ADD COLUMN native_thread_id text;
ALTER TABLE virtual_keys ADD COLUMN superseded_at timestamptz;
ALTER TABLE runs ADD COLUMN native_turn_id text;
ALTER TABLE runs ADD COLUMN sandbox_id uuid;
ALTER TABLE runs ADD CONSTRAINT run_sandbox_tenant FOREIGN KEY(org_id,sandbox_id) REFERENCES sandboxes(org_id,id);
CREATE UNIQUE INDEX one_work_writer ON sandboxes(org_id,conversation_id) WHERE persistent AND state IN ('running','provisioning','restoring');
CREATE TABLE native_requests (
 org_id uuid NOT NULL, sandbox_id uuid NOT NULL, request_id text NOT NULL,
 run_id uuid NOT NULL, method text NOT NULL, params jsonb NOT NULL,
 action_id uuid, answer jsonb, delivered boolean NOT NULL DEFAULT false,
 PRIMARY KEY(sandbox_id,request_id),
 FOREIGN KEY(org_id,sandbox_id) REFERENCES sandboxes(org_id,id),
 FOREIGN KEY(org_id,run_id) REFERENCES runs(org_id,id),
 FOREIGN KEY(org_id,action_id) REFERENCES pending_actions(org_id,id)
);
CREATE TABLE native_items (
 org_id uuid NOT NULL, sandbox_id uuid NOT NULL, native_id text NOT NULL, item_id uuid NOT NULL,
 PRIMARY KEY(sandbox_id,native_id),
 FOREIGN KEY(org_id,sandbox_id) REFERENCES sandboxes(org_id,id),
 FOREIGN KEY(org_id,item_id) REFERENCES items(org_id,id)
);
CREATE TABLE checkpoints (
 id uuid PRIMARY KEY, org_id uuid NOT NULL, conversation_id uuid NOT NULL, sandbox_id uuid NOT NULL,
 manifest jsonb NOT NULL, channel_seq bigint NOT NULL, gseq bigint NOT NULL,
 state text NOT NULL DEFAULT 'committed' CHECK(state IN ('committed','tombstoned','deleted')),
 created_at timestamptz NOT NULL DEFAULT now(), delete_after timestamptz,
 UNIQUE(org_id,id), FOREIGN KEY(org_id,conversation_id) REFERENCES conversations(org_id,id),
 FOREIGN KEY(org_id,sandbox_id) REFERENCES sandboxes(org_id,id)
);
CREATE TABLE workspace_heads (
 org_id uuid NOT NULL, conversation_id uuid NOT NULL, checkpoint_id uuid,
 generation bigint NOT NULL DEFAULT 1, PRIMARY KEY(org_id,conversation_id),
 FOREIGN KEY(org_id,conversation_id) REFERENCES conversations(org_id,id),
 FOREIGN KEY(org_id,checkpoint_id) REFERENCES checkpoints(org_id,id)
);
CREATE TABLE restore_pins (
 id uuid PRIMARY KEY, org_id uuid NOT NULL, checkpoint_id uuid NOT NULL, created_at timestamptz NOT NULL DEFAULT now(),
 FOREIGN KEY(org_id,checkpoint_id) REFERENCES checkpoints(org_id,id)
);
CREATE TABLE workspace_jobs (
 id uuid PRIMARY KEY, org_id uuid NOT NULL, conversation_id uuid NOT NULL,
 kind text NOT NULL CHECK(kind IN ('checkpoint','archive','restore')), state text NOT NULL DEFAULT 'queued',
 result jsonb, created_at timestamptz NOT NULL DEFAULT now(),
 FOREIGN KEY(org_id,conversation_id) REFERENCES conversations(org_id,id)
);
CREATE UNIQUE INDEX one_workspace_job ON workspace_jobs(org_id,conversation_id) WHERE state IN ('queued','running');
CREATE TABLE mcp_calls (
 id uuid PRIMARY KEY, org_id uuid NOT NULL, run_id uuid NOT NULL, sandbox_id uuid NOT NULL, lease_epoch bigint NOT NULL,
 request_key text NOT NULL, fingerprint text NOT NULL, tool text NOT NULL, arguments jsonb NOT NULL,
 action_id uuid NOT NULL, state text NOT NULL DEFAULT 'pending', result jsonb, expires_at timestamptz NOT NULL,
 UNIQUE(sandbox_id,request_key),
 FOREIGN KEY(org_id,run_id) REFERENCES runs(org_id,id),
 FOREIGN KEY(org_id,sandbox_id) REFERENCES sandboxes(org_id,id),
 FOREIGN KEY(org_id,action_id) REFERENCES pending_actions(org_id,id)
);
DO $$ DECLARE t text; BEGIN
 FOREACH t IN ARRAY ARRAY['native_requests','native_items','checkpoints','workspace_heads','restore_pins','workspace_jobs','mcp_calls'] LOOP
  EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY',t);
  EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY',t);
  EXECUTE format('CREATE POLICY org_isolation ON %I USING (org_id=nullif(current_setting(''app.org_id'',true),'''')::uuid) WITH CHECK (org_id=nullif(current_setting(''app.org_id'',true),'''')::uuid)',t);
  EXECUTE format('GRANT SELECT,INSERT,UPDATE,DELETE ON %I TO ryzhub_app',t);
 END LOOP;
END $$;
GRANT SELECT ON workspace_jobs TO ryzhub_worker;
ALTER TABLE guest_events ADD COLUMN channel_seq bigint;
