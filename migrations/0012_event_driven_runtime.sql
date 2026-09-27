-- 0012: event-driven runtime, opaque gateway keys, bounded storage and explicit lifecycles.

------------------------------------------------------------------------------------------
-- Revocation notices. Gateways LISTEN on ryzhub_revocation and re-validate long-lived
-- subscriptions and model streams when authorization-relevant rows change.
------------------------------------------------------------------------------------------
CREATE FUNCTION ryzhub_notice(source text, org uuid, member uuid, conv uuid, run uuid,
                              channel text DEFAULT NULL, epoch bigint DEFAULT NULL)
RETURNS void LANGUAGE sql AS $$
    SELECT pg_notify('ryzhub_revocation', jsonb_strip_nulls(jsonb_build_object(
        'table', source, 'org_id', org, 'user_id', member, 'conversation_id', conv,
        'run_id', run, 'channel', channel, 'writer_epoch', epoch))::text)
$$;

CREATE FUNCTION ryzhub_notice_membership() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN PERFORM ryzhub_notice(TG_TABLE_NAME, OLD.org_id, OLD.user_id, NULL, NULL); RETURN NULL; END $$;
CREATE FUNCTION ryzhub_notice_user() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN PERFORM ryzhub_notice(TG_TABLE_NAME, NULL, NEW.id, NULL, NULL); RETURN NULL; END $$;
CREATE FUNCTION ryzhub_notice_session() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN PERFORM ryzhub_notice(TG_TABLE_NAME, NULL, NEW.user_id, NULL, NULL); RETURN NULL; END $$;
CREATE FUNCTION ryzhub_notice_organization() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN PERFORM ryzhub_notice(TG_TABLE_NAME, NEW.id, NULL, NULL, NULL); RETURN NULL; END $$;
CREATE FUNCTION ryzhub_notice_policy() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN PERFORM ryzhub_notice(TG_TABLE_NAME, NEW.org_id, NULL, NULL, NULL); RETURN NULL; END $$;
CREATE FUNCTION ryzhub_notice_credential() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN PERFORM ryzhub_notice(TG_TABLE_NAME, NEW.org_id, NEW.owner_user_id, NULL, NULL); RETURN NULL; END $$;
CREATE FUNCTION ryzhub_notice_key() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN PERFORM ryzhub_notice(TG_TABLE_NAME, NEW.org_id, NEW.user_id, NULL, NEW.run_id); RETURN NULL; END $$;
CREATE FUNCTION ryzhub_notice_run() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN PERFORM ryzhub_notice(TG_TABLE_NAME, NEW.org_id, NULL, NEW.conversation_id, NEW.id); RETURN NULL; END $$;
CREATE FUNCTION ryzhub_notice_sandbox() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN PERFORM ryzhub_notice(TG_TABLE_NAME, NEW.org_id, NULL, NEW.conversation_id, NEW.run_id); RETURN NULL; END $$;
CREATE FUNCTION ryzhub_notice_conversation() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN PERFORM ryzhub_notice(TG_TABLE_NAME, NEW.org_id, NEW.creator_id, NEW.id, NULL); RETURN NULL; END $$;
CREATE FUNCTION ryzhub_notice_channel() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN PERFORM ryzhub_notice(TG_TABLE_NAME, NEW.org_id, NULL, NEW.conversation_id, NULL, NEW.id, NEW.writer_epoch); RETURN NULL; END $$;

CREATE TRIGGER revocation AFTER DELETE OR UPDATE ON memberships
    FOR EACH ROW EXECUTE FUNCTION ryzhub_notice_membership();
CREATE TRIGGER revocation AFTER UPDATE OF status, is_instance_admin ON users
    FOR EACH ROW EXECUTE FUNCTION ryzhub_notice_user();
CREATE TRIGGER revocation AFTER UPDATE OF revoked_at ON auth_sessions
    FOR EACH ROW EXECUTE FUNCTION ryzhub_notice_session();
CREATE TRIGGER revocation AFTER UPDATE OF state ON organizations
    FOR EACH ROW EXECUTE FUNCTION ryzhub_notice_organization();
CREATE TRIGGER revocation AFTER INSERT OR UPDATE ON org_policies
    FOR EACH ROW EXECUTE FUNCTION ryzhub_notice_policy();
CREATE TRIGGER revocation AFTER UPDATE OF revoked_at ON model_credentials
    FOR EACH ROW EXECUTE FUNCTION ryzhub_notice_credential();
CREATE TRIGGER revocation AFTER UPDATE OF revoked_at, superseded_at ON virtual_keys
    FOR EACH ROW EXECUTE FUNCTION ryzhub_notice_key();
CREATE TRIGGER revocation AFTER UPDATE OF state, exec_epoch ON runs
    FOR EACH ROW EXECUTE FUNCTION ryzhub_notice_run();
CREATE TRIGGER revocation AFTER UPDATE OF state, lease_epoch ON sandboxes
    FOR EACH ROW EXECUTE FUNCTION ryzhub_notice_sandbox();
CREATE TRIGGER revocation AFTER UPDATE OF state ON conversations
    FOR EACH ROW EXECUTE FUNCTION ryzhub_notice_conversation();
CREATE TRIGGER revocation AFTER UPDATE OF writer_epoch ON channels
    FOR EACH ROW WHEN (NEW.kind = 'conv') EXECUTE FUNCTION ryzhub_notice_channel();

------------------------------------------------------------------------------------------
-- Opaque gateway keys. Both Owner (loop) and sandbox callers present a random bearer token;
-- only its SHA-256 is stored. JWT-era keys can no longer be presented.
------------------------------------------------------------------------------------------
ALTER TABLE virtual_keys
    ADD COLUMN token_hash bytea,
    ADD COLUMN kind text NOT NULL DEFAULT 'sandbox' CHECK (kind IN ('loop', 'sandbox')),
    ADD COLUMN conversation_id uuid,
    ADD COLUMN exec_epoch bigint,
    ALTER COLUMN sandbox_id DROP NOT NULL,
    ALTER COLUMN lease_epoch DROP NOT NULL;
UPDATE virtual_keys SET revoked_at = coalesce(revoked_at, now()) WHERE token_hash IS NULL;
UPDATE virtual_keys v SET conversation_id = r.conversation_id
    FROM runs r WHERE r.org_id = v.org_id AND r.id = v.run_id;
ALTER TABLE virtual_keys
    ALTER COLUMN conversation_id SET NOT NULL,
    ADD CONSTRAINT key_tenant_conversation FOREIGN KEY (org_id, conversation_id)
        REFERENCES conversations(org_id, id),
    ADD CONSTRAINT key_shape CHECK (
        (kind = 'loop' AND exec_epoch IS NOT NULL)
        OR (kind = 'sandbox' AND sandbox_id IS NOT NULL AND lease_epoch IS NOT NULL));
CREATE UNIQUE INDEX virtual_keys_token ON virtual_keys(token_hash) WHERE token_hash IS NOT NULL;
CREATE INDEX virtual_keys_run ON virtual_keys(org_id, run_id);

-- A bearer token is the only input a gateway has before it knows the tenant.
CREATE FUNCTION resolve_gateway_key(token bytea) RETURNS TABLE(id uuid, org_id uuid)
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = pg_catalog, public AS $$
    SELECT v.id, v.org_id FROM public.virtual_keys v WHERE v.token_hash = token
$$;
REVOKE ALL ON FUNCTION resolve_gateway_key(bytea) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION resolve_gateway_key(bytea) TO ryzhub_app;

------------------------------------------------------------------------------------------
-- Typed timeline details and bounded item text.
------------------------------------------------------------------------------------------
ALTER TABLE items
    ADD COLUMN detail jsonb,
    ADD COLUMN truncated boolean NOT NULL DEFAULT false;

------------------------------------------------------------------------------------------
-- Model context as a chain of deltas with periodic snapshots instead of a full copy per step.
------------------------------------------------------------------------------------------
ALTER TABLE loop_steps
    ALTER COLUMN request_messages DROP NOT NULL,
    ADD COLUMN base_run_id uuid,
    ADD COLUMN base_step integer,
    ADD COLUMN input_delta jsonb,
    ADD COLUMN chain_length integer NOT NULL DEFAULT 0,
    ADD CONSTRAINT loop_step_base FOREIGN KEY (base_run_id, base_step) REFERENCES loop_steps(run_id, step_no),
    ADD CONSTRAINT loop_step_context CHECK (
        request_messages IS NOT NULL
        OR (base_run_id IS NOT NULL AND base_step IS NOT NULL AND input_delta IS NOT NULL));
ALTER TABLE runs
    ADD COLUMN seed_run_id uuid,
    ADD COLUMN seed_step integer,
    ADD CONSTRAINT run_seed_step FOREIGN KEY (seed_run_id, seed_step) REFERENCES loop_steps(run_id, step_no),
    ADD COLUMN native_interrupted boolean NOT NULL DEFAULT false;

------------------------------------------------------------------------------------------
-- Project content is snapshotted once per distinct revision and referenced by runs.
------------------------------------------------------------------------------------------
CREATE TABLE project_revisions (
    id uuid PRIMARY KEY, org_id uuid NOT NULL, project_id uuid NOT NULL, digest bytea NOT NULL,
    context text NOT NULL, files jsonb NOT NULL, created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (org_id, id), UNIQUE (project_id, digest),
    FOREIGN KEY (org_id, project_id) REFERENCES projects(org_id, id)
);
ALTER TABLE project_revisions ENABLE ROW LEVEL SECURITY;
ALTER TABLE project_revisions FORCE ROW LEVEL SECURITY;
CREATE POLICY org_isolation ON project_revisions
    USING (org_id = nullif(current_setting('app.org_id', true), '')::uuid)
    WITH CHECK (org_id = nullif(current_setting('app.org_id', true), '')::uuid);
GRANT SELECT, INSERT, UPDATE, DELETE ON project_revisions TO ryzhub_app;

ALTER TABLE runs ADD COLUMN project_revision_id uuid,
    ADD CONSTRAINT run_project_revision FOREIGN KEY (org_id, project_revision_id)
        REFERENCES project_revisions(org_id, id);
INSERT INTO project_revisions(id, org_id, project_id, digest, context, files)
SELECT DISTINCT ON (r.project_id, digest)
       uuidv7(), r.org_id, r.project_id, digest, r.project_context, r.project_files
FROM (SELECT org_id, project_id, project_context, project_files,
             sha256(convert_to(project_context || project_files::text, 'UTF8')) AS digest
      FROM runs WHERE project_id IS NOT NULL
        AND (project_context <> '' OR project_files <> '{}'::jsonb)) r
ORDER BY r.project_id, digest;
UPDATE runs r SET project_revision_id = p.id
FROM project_revisions p
WHERE p.org_id = r.org_id AND p.project_id = r.project_id
  AND p.digest = sha256(convert_to(r.project_context || r.project_files::text, 'UTF8'));
ALTER TABLE runs DROP COLUMN project_context, DROP COLUMN project_files;

------------------------------------------------------------------------------------------
-- Explicit lifecycles.
------------------------------------------------------------------------------------------
ALTER TABLE checkpoints DROP CONSTRAINT checkpoints_state_check,
    ADD CONSTRAINT checkpoints_state_check CHECK (state IN ('uploading', 'committed', 'tombstoned', 'deleted'));
ALTER TABLE workspace_jobs DROP CONSTRAINT workspace_jobs_kind_check,
    ADD CONSTRAINT workspace_jobs_kind_check CHECK (kind IN ('checkpoint', 'archive', 'restore', 'suspend')),
    ADD CONSTRAINT workspace_jobs_state_check CHECK (state IN ('queued', 'running', 'completed', 'failed'));
ALTER TABLE sandboxes ADD CONSTRAINT sandboxes_state_check CHECK (
    state IN ('provisioning', 'running', 'suspended', 'stopped', 'lost', 'archived', 'deleted'));
ALTER TABLE pending_actions ADD CONSTRAINT pending_actions_status_check CHECK (
    status IN ('pending', 'allow', 'deny', 'answered', 'invalidated')),
    ADD CONSTRAINT pending_actions_kind_check CHECK (kind IN ('approval', 'question'));
ALTER TABLE mcp_calls ADD CONSTRAINT mcp_calls_state_check CHECK (
    state IN ('pending', 'awaiting_approval', 'executing', 'completed', 'outcome_unknown'));
ALTER TABLE conversation_operations ADD CONSTRAINT conversation_operations_state_check CHECK (
    state IN ('queued', 'running', 'completed', 'failed'));

-- Credentials record which vault key encrypted them, so the key can be rotated.
ALTER TABLE model_credentials ADD COLUMN key_id smallint NOT NULL DEFAULT 0;

------------------------------------------------------------------------------------------
-- Partitions. A default partition keeps writes available if maintenance lapses; maintain
-- moves any rows out of it before attaching the missing range.
------------------------------------------------------------------------------------------
CREATE TABLE events_default PARTITION OF events DEFAULT;
CREATE TABLE audit_logs_default PARTITION OF audit_logs DEFAULT;
REVOKE ALL ON events_default, audit_logs_default FROM ryzhub_app, ryzhub_worker;

-- Server-side WebSocket resume duplicated the client's own cursors and is removed.
DROP TABLE ws_resumes;

------------------------------------------------------------------------------------------
-- Discovery touches only active rows; retention finds expired rows by time.
------------------------------------------------------------------------------------------
CREATE INDEX channels_unpublished ON channels(org_id, conversation_id) WHERE last_seq > published_seq;
CREATE INDEX runs_active ON runs(org_id, conversation_id) WHERE state IN ('running', 'requires_action');
CREATE INDEX sandboxes_live ON sandboxes(org_id, conversation_id) WHERE state IN ('running', 'provisioning');
CREATE INDEX commands_done ON commands(created_at) WHERE status = 'done';
CREATE INDEX guest_events_age ON guest_events(created_at);
CREATE INDEX model_rate_limits_age ON model_rate_limits(minute);
CREATE INDEX usage_records_org_time ON usage_records(org_id, created_at);
CREATE INDEX checkpoints_uploading ON checkpoints(created_at) WHERE state = 'uploading';
-- Owner discovery reads organization policies for their idle-suspension limit.
GRANT SELECT ON guest_events, model_rate_limits, virtual_keys, usage_records, pending_actions,
    conversation_operations, org_policies TO ryzhub_worker;
