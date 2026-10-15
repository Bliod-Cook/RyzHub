-- Native protocol/config snapshots and private project boundaries.
CREATE TABLE projects (
 id uuid PRIMARY KEY, org_id uuid NOT NULL REFERENCES organizations,
 creator_id uuid NOT NULL REFERENCES users, name text NOT NULL,
 instructions text NOT NULL DEFAULT '', defaults jsonb NOT NULL DEFAULT '{}',
 memory_mode text NOT NULL CHECK(memory_mode IN ('default','project_only')),
 state text NOT NULL DEFAULT 'active' CHECK(state IN ('active','deletion_pending','deleted')),
 deleted_at timestamptz, created_at timestamptz NOT NULL DEFAULT now(), UNIQUE(org_id,id)
);
CREATE TABLE project_files (
 id uuid PRIMARY KEY, org_id uuid NOT NULL, project_id uuid NOT NULL,
 name text NOT NULL, content text NOT NULL, created_at timestamptz NOT NULL DEFAULT now(),
 UNIQUE(project_id,name), FOREIGN KEY(org_id,project_id) REFERENCES projects(org_id,id)
);
CREATE TABLE project_members (
 org_id uuid NOT NULL, project_id uuid NOT NULL, user_id uuid NOT NULL REFERENCES users,
 PRIMARY KEY(project_id,user_id), FOREIGN KEY(org_id,project_id) REFERENCES projects(org_id,id)
);
ALTER TABLE conversations ADD COLUMN project_id uuid;
ALTER TABLE conversations ADD CONSTRAINT conversation_project_tenant FOREIGN KEY(org_id,project_id) REFERENCES projects(org_id,id);
ALTER TABLE conversations ADD COLUMN run_config jsonb NOT NULL DEFAULT '{}';
ALTER TABLE conversations ADD COLUMN context_seed jsonb;
ALTER TABLE conversations ADD COLUMN project_deleted_state text;
ALTER TABLE runs ADD COLUMN protocol text NOT NULL DEFAULT 'anthropic';
ALTER TABLE runs ADD COLUMN run_config jsonb NOT NULL DEFAULT '{}';
ALTER TABLE runs ADD COLUMN project_id uuid;
ALTER TABLE runs ADD CONSTRAINT run_project_tenant FOREIGN KEY(org_id,project_id) REFERENCES projects(org_id,id);
ALTER TABLE loop_steps ADD COLUMN protocol text NOT NULL DEFAULT 'anthropic';
ALTER TABLE sandboxes ADD COLUMN capabilities jsonb NOT NULL DEFAULT '{}';
ALTER TABLE sandboxes ADD COLUMN commands jsonb NOT NULL DEFAULT '[]';
ALTER TABLE native_items ADD COLUMN native_message_id text;
CREATE TABLE conversation_operations (
 id uuid PRIMARY KEY, org_id uuid NOT NULL, conversation_id uuid NOT NULL,
 kind text NOT NULL CHECK(kind IN ('compact','fork')), payload jsonb NOT NULL DEFAULT '{}',
 state text NOT NULL DEFAULT 'queued', result jsonb, created_at timestamptz NOT NULL DEFAULT now(),
 FOREIGN KEY(org_id,conversation_id) REFERENCES conversations(org_id,id)
);
CREATE UNIQUE INDEX one_conversation_operation ON conversation_operations(org_id,conversation_id) WHERE state IN ('queued','running');
DO $$ DECLARE t text; BEGIN
 FOREACH t IN ARRAY ARRAY['projects','project_files','project_members','conversation_operations'] LOOP
  EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY',t);
  EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY',t);
  EXECUTE format('CREATE POLICY org_isolation ON %I USING (org_id=nullif(current_setting(''app.org_id'',true),'''')::uuid) WITH CHECK (org_id=nullif(current_setting(''app.org_id'',true),'''')::uuid)',t);
  EXECUTE format('GRANT SELECT,INSERT,UPDATE,DELETE ON %I TO ryzhub_app',t);
 END LOOP;
END $$;
GRANT SELECT ON conversation_operations TO ryzhub_worker;

ALTER TABLE runs ADD COLUMN operation text NOT NULL DEFAULT '';
ALTER TABLE conversations ADD COLUMN native_fork jsonb;
CREATE TABLE native_turns (
 org_id uuid NOT NULL, sandbox_id uuid NOT NULL, turn_id text NOT NULL, run_id uuid NOT NULL,
 PRIMARY KEY(sandbox_id,turn_id), FOREIGN KEY(org_id,sandbox_id) REFERENCES sandboxes(org_id,id),
 FOREIGN KEY(org_id,run_id) REFERENCES runs(org_id,id)
);
ALTER TABLE native_turns ENABLE ROW LEVEL SECURITY;
ALTER TABLE native_turns FORCE ROW LEVEL SECURITY;
CREATE POLICY org_isolation ON native_turns USING (org_id=nullif(current_setting('app.org_id',true),'')::uuid) WITH CHECK (org_id=nullif(current_setting('app.org_id',true),'')::uuid);
GRANT SELECT,INSERT,UPDATE,DELETE ON native_turns TO ryzhub_app;

ALTER TABLE loop_steps ADD COLUMN item_id uuid;

ALTER TABLE runs ADD COLUMN project_context text NOT NULL DEFAULT '';
ALTER TABLE runs ADD COLUMN project_files jsonb NOT NULL DEFAULT '{}';
