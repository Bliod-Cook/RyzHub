-- Images a Chat model viewed with `view_image`. Model context keeps only a reference, so
-- snapshots and forks stay small; the bytes are inlined when a request is sent. Decoded and
-- scaled inside the sandbox, at most a few MiB each.
CREATE TABLE chat_images (
 org_id uuid NOT NULL, conversation_id uuid NOT NULL, sha256 text NOT NULL,
 media_type text NOT NULL, data bytea NOT NULL, width int NOT NULL, height int NOT NULL,
 created_at timestamptz NOT NULL DEFAULT now(),
 PRIMARY KEY(org_id,conversation_id,sha256),
 FOREIGN KEY(org_id,conversation_id) REFERENCES conversations(org_id,id)
);
ALTER TABLE chat_images ENABLE ROW LEVEL SECURITY;
ALTER TABLE chat_images FORCE ROW LEVEL SECURITY;
CREATE POLICY org_isolation ON chat_images
 USING (org_id=nullif(current_setting('app.org_id',true),'')::uuid)
 WITH CHECK (org_id=nullif(current_setting('app.org_id',true),'')::uuid);
GRANT SELECT,INSERT,DELETE ON chat_images TO ryzhub_app;
