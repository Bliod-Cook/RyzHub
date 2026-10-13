-- A knowledge file read from a document keeps the document, by content address in the
-- organization's file store (shared with library files of the same content).
ALTER TABLE project_files ADD COLUMN source_sha256 text, ADD COLUMN source_type text,
  ADD COLUMN source_size bigint, ADD COLUMN truncated boolean NOT NULL DEFAULT false;
CREATE INDEX project_files_source ON project_files(org_id,source_sha256) WHERE source_sha256 IS NOT NULL;

-- A library file sent in parts. `direct` uploads go straight to an S3 bucket as one
-- multipart upload (`s3_upload_id`); the others come through the gateway, a part at a time.
-- Kept a day after completion, so a repeated completion returns the same file.
CREATE TABLE uploads (
 id uuid PRIMARY KEY, org_id uuid NOT NULL REFERENCES organizations, user_id uuid NOT NULL REFERENCES users,
 name text NOT NULL, size bigint NOT NULL CHECK (size > 0), part_size bigint NOT NULL CHECK (part_size > 0),
 direct boolean NOT NULL, s3_upload_id text,
 state text NOT NULL DEFAULT 'open' CHECK (state IN ('open','completing','completed','aborted')),
 attachment_id uuid, created_at timestamptz NOT NULL DEFAULT now(), expires_at timestamptz NOT NULL,
 UNIQUE(org_id,id)
);
CREATE INDEX uploads_expiry ON uploads(expires_at);
-- A part received: its digest when it came through the gateway, its entity tag when it went
-- to the bucket.
CREATE TABLE upload_parts (
 org_id uuid NOT NULL, upload_id uuid NOT NULL, n int NOT NULL CHECK (n > 0), size bigint NOT NULL,
 sha256 text, etag text, PRIMARY KEY(upload_id,n),
 FOREIGN KEY(org_id,upload_id) REFERENCES uploads(org_id,id) ON DELETE CASCADE
);
DO $$ DECLARE t text; BEGIN
 FOREACH t IN ARRAY ARRAY['uploads','upload_parts'] LOOP
  EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY',t);
  EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY',t);
  EXECUTE format('CREATE POLICY org_isolation ON %I USING (org_id=nullif(current_setting(''app.org_id'',true),'''')::uuid) WITH CHECK (org_id=nullif(current_setting(''app.org_id'',true),'''')::uuid)',t);
  EXECUTE format('GRANT SELECT,INSERT,UPDATE,DELETE ON %I TO ryzhub_app',t);
  EXECUTE format('GRANT SELECT ON %I TO ryzhub_worker',t);
 END LOOP;
END $$;

-- Android devices that receive notifications, each tied to the sign-in that registered it:
-- signing out stops them. `device` is the client's own id (its Hello); a device with a live
-- connection (`online_until`) is not notified, it already shows the change.
CREATE TABLE push_devices (
 id uuid PRIMARY KEY, user_id uuid NOT NULL REFERENCES users,
 sign_in uuid NOT NULL REFERENCES auth_refresh ON DELETE CASCADE,
 device text NOT NULL, platform text NOT NULL CHECK (platform IN ('android')),
 token text NOT NULL UNIQUE, locale text NOT NULL DEFAULT 'zh',
 online_until timestamptz, created_at timestamptz NOT NULL DEFAULT now(),
 UNIQUE(user_id,device)
);
GRANT SELECT,INSERT,UPDATE,DELETE ON push_devices TO ryzhub_app;
