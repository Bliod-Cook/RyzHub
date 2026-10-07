-- A member's file library. Files belong to their uploader, not to a conversation; any of the
-- uploader's conversations may refer to one. Deleting a file removes it everywhere; deleting a
-- conversation removes only its references. Content lives in the BlobStore under
-- `<org>/attachments/<sha256>`, shared between entries with the same content. A deleted
-- file stays as a `reclaimed` tombstone once its content is gone, so references answer
-- "deleted" rather than "unknown".
CREATE TABLE attachments (
 id uuid PRIMARY KEY, org_id uuid NOT NULL REFERENCES organizations, user_id uuid NOT NULL REFERENCES users,
 -- Uploads now; files a sandbox hands back to the user later.
 direction text NOT NULL DEFAULT 'upload' CHECK (direction IN ('upload','output')),
 state text NOT NULL CHECK (state IN ('uploading','ready','deleted','reclaimed')),
 name text NOT NULL, media_type text NOT NULL,
 kind text NOT NULL CHECK (kind IN ('image','pdf','text','file')),
 size bigint NOT NULL, sha256 text NOT NULL, width int, height int,
 -- An image as models see it (scaled, at most a few MiB); `model_sha = sha256` when the
 -- original passed through unchanged.
 model_sha text, model_media_type text, model_data bytea,
 -- A small UTF-8 text file, inlined into model context.
 text_content text,
 created_at timestamptz NOT NULL DEFAULT now(), deleted_at timestamptz,
 UNIQUE (org_id,id)
);
CREATE INDEX attachments_library ON attachments(org_id,user_id,created_at DESC) WHERE state='ready';
CREATE INDEX attachments_content ON attachments(org_id,sha256) WHERE state IN ('uploading','ready');
CREATE INDEX attachments_reclaim ON attachments(state,created_at) WHERE state IN ('uploading','deleted');
-- A conversation's references: who may see the file there, its name under /attachments, and
-- what the conversation's sandboxes hold.
CREATE TABLE conversation_files (
 org_id uuid NOT NULL, conversation_id uuid NOT NULL, attachment_id uuid NOT NULL,
 path_name text NOT NULL, created_at timestamptz NOT NULL DEFAULT now(),
 PRIMARY KEY (conversation_id,attachment_id), UNIQUE (conversation_id,path_name),
 FOREIGN KEY (org_id,conversation_id) REFERENCES conversations(org_id,id),
 FOREIGN KEY (org_id,attachment_id) REFERENCES attachments(org_id,id)
);
CREATE INDEX conversation_files_attachment ON conversation_files(org_id,attachment_id);
-- The files each input carried, in order.
ALTER TABLE pending_inputs ADD COLUMN attachment_ids uuid[] NOT NULL DEFAULT '{}';
DO $$ DECLARE t text; BEGIN
 FOREACH t IN ARRAY ARRAY['attachments','conversation_files'] LOOP
  EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY',t);
  EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY',t);
  EXECUTE format('CREATE POLICY org_isolation ON %I USING (org_id=nullif(current_setting(''app.org_id'',true),'''')::uuid) WITH CHECK (org_id=nullif(current_setting(''app.org_id'',true),'''')::uuid)',t);
  EXECUTE format('GRANT SELECT,INSERT,UPDATE,DELETE ON %I TO ryzhub_app',t);
  EXECUTE format('GRANT SELECT ON %I TO ryzhub_worker',t);
 END LOOP;
END $$;
