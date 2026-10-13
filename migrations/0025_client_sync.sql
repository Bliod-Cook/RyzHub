-- Commands the Gateway runs itself on a user channel (CreateRun before its conversation
-- exists is queued under it; UndoMemoryUpdate names none).
ALTER TABLE commands ALTER COLUMN conversation_id DROP NOT NULL;

-- The conversation channel's sequence when its last run ended: unread while it is ahead of
-- the user's read position.
ALTER TABLE conversations ADD COLUMN activity_seq bigint NOT NULL DEFAULT 0;

-- Unsent messages, one per user and conversation, synced across the user's devices. The
-- latest write wins.
CREATE TABLE drafts (
 org_id uuid NOT NULL,
 user_id uuid NOT NULL REFERENCES users ON DELETE CASCADE,
 conversation_id uuid NOT NULL,
 text text NOT NULL,
 attachment_ids uuid[] NOT NULL DEFAULT '{}',
 device_id text NOT NULL DEFAULT '',
 updated_at timestamptz NOT NULL DEFAULT now(),
 PRIMARY KEY (user_id,conversation_id),
 FOREIGN KEY (org_id,conversation_id) REFERENCES conversations(org_id,id) ON DELETE CASCADE
);
DO $$ DECLARE t text; BEGIN
 FOREACH t IN ARRAY ARRAY['drafts'] LOOP
  EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY',t);
  EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY',t);
  EXECUTE format('CREATE POLICY org_isolation ON %I USING (org_id=nullif(current_setting(''app.org_id'',true),'''')::uuid) WITH CHECK (org_id=nullif(current_setting(''app.org_id'',true),'''')::uuid)',t);
  EXECUTE format('GRANT SELECT,INSERT,UPDATE,DELETE ON %I TO ryzhub_app',t);
  EXECUTE format('GRANT SELECT ON %I TO ryzhub_worker',t);
 END LOOP;
END $$;
