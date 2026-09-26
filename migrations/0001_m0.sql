CREATE TABLE users (
    id uuid PRIMARY KEY, email text NOT NULL UNIQUE, display_name text NOT NULL,
    password_hash text NOT NULL, status text NOT NULL DEFAULT 'active', is_instance_admin boolean NOT NULL DEFAULT false
);
CREATE TABLE user_identities (
    user_id uuid NOT NULL REFERENCES users, provider text NOT NULL, subject text NOT NULL,
    PRIMARY KEY (provider, subject)
);
CREATE TABLE auth_sessions (
    id uuid PRIMARY KEY, user_id uuid NOT NULL REFERENCES users, token_hash bytea NOT NULL UNIQUE,
    expires_at timestamptz NOT NULL, revoked_at timestamptz
);
CREATE TABLE ws_tickets (
    token_hash bytea PRIMARY KEY, session_id uuid NOT NULL REFERENCES auth_sessions,
    expires_at timestamptz NOT NULL
);
CREATE TABLE organizations (
    id uuid PRIMARY KEY, kind text NOT NULL CHECK (kind IN ('personal','team')),
    name text NOT NULL, state text NOT NULL DEFAULT 'active'
);
CREATE TABLE memberships (
    org_id uuid NOT NULL REFERENCES organizations, user_id uuid NOT NULL REFERENCES users,
    role text NOT NULL CHECK (role IN ('owner','admin','member')), PRIMARY KEY (org_id,user_id)
);
CREATE TABLE org_policies (
    org_id uuid NOT NULL REFERENCES organizations, version bigint NOT NULL, policy jsonb NOT NULL,
    PRIMARY KEY (org_id,version)
);
CREATE TABLE instance_settings (key text PRIMARY KEY, value jsonb NOT NULL);
CREATE TABLE conversations (
    id uuid PRIMARY KEY, org_id uuid NOT NULL REFERENCES organizations, creator_id uuid NOT NULL REFERENCES users,
    mode text NOT NULL CHECK (mode IN ('chat','work')), title text NOT NULL,
    state text NOT NULL DEFAULT 'active' CHECK (state IN ('active','archived','deletion_pending','deleted')),
    visibility text NOT NULL DEFAULT 'private' CHECK (visibility = 'private'), temporary boolean NOT NULL DEFAULT false,
    owner_epoch bigint NOT NULL DEFAULT 0, archived_at timestamptz, expires_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(), UNIQUE(org_id,id)
);
CREATE TABLE channels (
    id text PRIMARY KEY, org_id uuid NOT NULL REFERENCES organizations,
    kind text NOT NULL CHECK (kind IN ('conv','user','org')), conversation_id uuid, user_id uuid REFERENCES users,
    last_seq bigint NOT NULL DEFAULT 0 CHECK(last_seq >= 0), published_seq bigint NOT NULL DEFAULT 0,
    writer_epoch bigint, owner_id uuid, lease_until timestamptz,
    UNIQUE(org_id,id), FOREIGN KEY(org_id,conversation_id) REFERENCES conversations(org_id,id),
    CHECK(published_seq BETWEEN 0 AND last_seq),
    CHECK ((kind = 'conv' AND conversation_id IS NOT NULL AND writer_epoch IS NOT NULL)
        OR (kind = 'user' AND user_id IS NOT NULL AND writer_epoch IS NULL)
        OR (kind = 'org' AND writer_epoch IS NULL))
);
CREATE TABLE runs (
    id uuid PRIMARY KEY, org_id uuid NOT NULL, conversation_id uuid NOT NULL,
    harness text NOT NULL DEFAULT 'mock', model text NOT NULL DEFAULT 'mock', exec_epoch bigint NOT NULL,
    state text NOT NULL CHECK(state IN ('running','requires_action','completed','cancelled')),
    response_text text NOT NULL, assistant_item_id uuid NOT NULL, cause_cid uuid NOT NULL,
    started_at timestamptz NOT NULL DEFAULT now(), ended_at timestamptz,
    UNIQUE(org_id,id), FOREIGN KEY(org_id,conversation_id) REFERENCES conversations(org_id,id)
);
CREATE UNIQUE INDEX one_active_run ON runs(conversation_id) WHERE state IN ('running','requires_action');
CREATE TABLE items (
    id uuid PRIMARY KEY, org_id uuid NOT NULL, conversation_id uuid NOT NULL, run_id uuid,
    revision bigint NOT NULL, kind text NOT NULL, state text NOT NULL, text text NOT NULL,
    first_seq bigint NOT NULL, last_seq bigint NOT NULL, UNIQUE(org_id,id),
    FOREIGN KEY(org_id,conversation_id) REFERENCES conversations(org_id,id),
    FOREIGN KEY(org_id,run_id) REFERENCES runs(org_id,id)
);
CREATE INDEX items_history ON items(org_id,conversation_id,first_seq);
CREATE TABLE events (
    channel_id text NOT NULL, seq bigint NOT NULL CHECK(seq > 0), org_id uuid NOT NULL,
    run_id uuid, item_id uuid, kind text NOT NULL, cause_cid uuid, payload bytea NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(), PRIMARY KEY(channel_id,seq,created_at),
    FOREIGN KEY(org_id,channel_id) REFERENCES channels(org_id,id),
    FOREIGN KEY(org_id,run_id) REFERENCES runs(org_id,id),
    FOREIGN KEY(org_id,item_id) REFERENCES items(org_id,id)
) PARTITION BY RANGE(created_at);
CREATE INDEX events_replay ON events(org_id,channel_id,seq);
-- Provision the current window and two future weeks. Maintenance extends this window.
DO $$ DECLARE start_at timestamptz; partition_name text; BEGIN
    FOR i IN -5..2 LOOP
        start_at := date_trunc('week',now()) + i * interval '1 week';
        partition_name := 'events_' || to_char(start_at,'IYYY_IW');
        EXECUTE format('CREATE TABLE %I PARTITION OF events FOR VALUES FROM (%L) TO (%L)', partition_name,start_at,start_at+interval '1 week');
    END LOOP;
END $$;
CREATE TABLE commands (
    id uuid PRIMARY KEY, user_id uuid NOT NULL REFERENCES users, cid uuid NOT NULL,
    org_id uuid NOT NULL, conversation_id uuid NOT NULL, device_id text NOT NULL,
    payload bytea NOT NULL, status text NOT NULL DEFAULT 'queued' CHECK(status IN ('queued','done')),
    result bytea, created_at timestamptz NOT NULL DEFAULT now(), UNIQUE(user_id,cid), UNIQUE(org_id,id),
    FOREIGN KEY(org_id,conversation_id) REFERENCES conversations(org_id,id)
);
CREATE INDEX queued_commands ON commands(conversation_id,created_at,id) WHERE status = 'queued';
CREATE TABLE pending_actions (
    id uuid PRIMARY KEY, org_id uuid NOT NULL, run_id uuid NOT NULL, run_epoch bigint NOT NULL,
    kind text NOT NULL DEFAULT 'approval', summary text NOT NULL, options text[] NOT NULL,
    status text NOT NULL DEFAULT 'pending', resolved_by uuid REFERENCES users, resolved_device text NOT NULL DEFAULT '',
    expires_at timestamptz NOT NULL, UNIQUE(org_id,id), FOREIGN KEY(org_id,run_id) REFERENCES runs(org_id,id)
);
CREATE TABLE read_states (
    org_id uuid NOT NULL, user_id uuid NOT NULL REFERENCES users, conversation_id uuid NOT NULL, read_seq bigint NOT NULL,
    PRIMARY KEY(user_id,conversation_id), FOREIGN KEY(org_id,conversation_id) REFERENCES conversations(org_id,id)
);
CREATE TABLE audit_logs (
    id uuid NOT NULL, org_id uuid NOT NULL REFERENCES organizations, actor_id uuid NOT NULL,
    actor_kind text NOT NULL, action text NOT NULL, resource text NOT NULL, result text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(), PRIMARY KEY(id,created_at)
) PARTITION BY RANGE(created_at);
DO $$ DECLARE start_at timestamptz; partition_name text; BEGIN
    FOR i IN -1..2 LOOP
        start_at := date_trunc('month',now()) + i * interval '1 month';
        partition_name := 'audit_logs_' || to_char(start_at,'YYYY_MM');
        EXECUTE format('CREATE TABLE %I PARTITION OF audit_logs FOR VALUES FROM (%L) TO (%L)',partition_name,start_at,start_at+interval '1 month');
    END LOOP;
END $$;

DO $$ DECLARE table_name text; BEGIN
    FOREACH table_name IN ARRAY ARRAY['memberships','org_policies','conversations','channels','runs','items','events','commands','pending_actions','read_states','audit_logs'] LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY',table_name);
        EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY',table_name);
        EXECUTE format('CREATE POLICY org_isolation ON %I USING (org_id = nullif(current_setting(''app.org_id'',true),'''')::uuid) WITH CHECK (org_id = nullif(current_setting(''app.org_id'',true),'''')::uuid)',table_name);
    END LOOP;
END $$;
-- Do not grant access to leaf partitions: parent-table RLS is the access path.
REVOKE ALL ON ALL TABLES IN SCHEMA public FROM ryzhub_app, ryzhub_worker;
GRANT SELECT,INSERT,UPDATE,DELETE ON users,user_identities,auth_sessions,ws_tickets,organizations,memberships,org_policies,conversations,channels,runs,items,events,commands,pending_actions,read_states,audit_logs TO ryzhub_app;
GRANT SELECT ON instance_settings TO ryzhub_app;
GRANT SELECT ON organizations,memberships,channels,runs,commands,conversations TO ryzhub_worker;
