-- Plugins (M5). A plugin is fetched from its source into an immutable version; a member
-- installs a version and confirms what it asks for; it is then on for their conversations
-- unless a project or conversation turns it off. A run uses the plugins it started with.
CREATE TABLE plugins (
 id uuid PRIMARY KEY, org_id uuid NOT NULL REFERENCES organizations,
 name text NOT NULL,
 -- {kind: url|git|upload|catalog|mcp, url, ref, ...}
 source jsonb NOT NULL,
 created_by uuid NOT NULL REFERENCES users,
 created_at timestamptz NOT NULL DEFAULT now(),
 UNIQUE (org_id,id)
);
CREATE TABLE plugin_versions (
 id uuid PRIMARY KEY, org_id uuid NOT NULL, plugin_id uuid NOT NULL,
 version text NOT NULL DEFAULT '', content_hash text NOT NULL,
 trust text NOT NULL CHECK (trust IN ('signed','official','unverified')),
 -- The package as parsed (`plugins::package::Manifest`).
 manifest jsonb NOT NULL,
 -- The commit a git source was fetched at.
 commit text,
 created_at timestamptz NOT NULL DEFAULT now(),
 UNIQUE (org_id,id),
 FOREIGN KEY (org_id,plugin_id) REFERENCES plugins(org_id,id) ON DELETE CASCADE
);
CREATE TABLE plugin_installations (
 id uuid PRIMARY KEY, org_id uuid NOT NULL, plugin_id uuid NOT NULL,
 -- Member installations only in v1; organization-wide ones need its management UI.
 scope text NOT NULL DEFAULT 'member' CHECK (scope IN ('member','org')),
 user_id uuid REFERENCES users,
 version_id uuid NOT NULL,
 -- `pending` until first confirmed; `needs_reconfirm` while changed tools wait for the member.
 state text NOT NULL CHECK (state IN ('pending','active','needs_reconfirm')),
 enabled boolean NOT NULL DEFAULT true,
 -- Settings that are not secret; secret ones are vault ids by setting.
 config jsonb NOT NULL DEFAULT '{}', secrets jsonb NOT NULL DEFAULT '{}',
 -- An update fetched but not yet confirmed.
 pending_version_id uuid,
 confirmed_at timestamptz,
 created_at timestamptz NOT NULL DEFAULT now(), updated_at timestamptz NOT NULL DEFAULT now(),
 UNIQUE (org_id,id), UNIQUE (org_id,user_id,plugin_id),
 FOREIGN KEY (org_id,plugin_id) REFERENCES plugins(org_id,id) ON DELETE CASCADE,
 FOREIGN KEY (org_id,version_id) REFERENCES plugin_versions(org_id,id)
);
-- What the member agreed to, per version.
CREATE TABLE plugin_grants (
 org_id uuid NOT NULL, installation_id uuid NOT NULL, version_id uuid NOT NULL,
 permissions_digest text NOT NULL, granted_by uuid NOT NULL REFERENCES users,
 granted_at timestamptz NOT NULL DEFAULT now(),
 PRIMARY KEY (installation_id,version_id),
 FOREIGN KEY (org_id,installation_id) REFERENCES plugin_installations(org_id,id) ON DELETE CASCADE
);
-- Each tool as last listed, and the definition the member confirmed. A tool whose definition
-- changed is not offered to models until it is confirmed again.
CREATE TABLE plugin_tools (
 org_id uuid NOT NULL, installation_id uuid NOT NULL, server text NOT NULL, tool text NOT NULL,
 definition jsonb NOT NULL, digest text NOT NULL, approved_digest text,
 seen_at timestamptz NOT NULL DEFAULT now(),
 PRIMARY KEY (installation_id,server,tool),
 FOREIGN KEY (org_id,installation_id) REFERENCES plugin_installations(org_id,id) ON DELETE CASCADE
);
-- Accounts members connected for a plugin's server (OAuth); tokens are in the vault.
CREATE TABLE plugin_connections (
 org_id uuid NOT NULL, installation_id uuid NOT NULL, user_id uuid NOT NULL REFERENCES users,
 server text NOT NULL, issuer text NOT NULL,
 -- client_id, token_endpoint, resource, scopes
 client jsonb NOT NULL, token_secret uuid NOT NULL, expires_at timestamptz,
 connected_at timestamptz NOT NULL DEFAULT now(),
 PRIMARY KEY (installation_id,user_id,server),
 FOREIGN KEY (org_id,installation_id) REFERENCES plugin_installations(org_id,id) ON DELETE CASCADE
);
CREATE TABLE conversation_plugins (
 org_id uuid NOT NULL, conversation_id uuid NOT NULL, installation_id uuid NOT NULL,
 enabled boolean NOT NULL,
 PRIMARY KEY (conversation_id,installation_id),
 FOREIGN KEY (org_id,conversation_id) REFERENCES conversations(org_id,id) ON DELETE CASCADE,
 FOREIGN KEY (org_id,installation_id) REFERENCES plugin_installations(org_id,id) ON DELETE CASCADE
);
CREATE TABLE project_plugins (
 org_id uuid NOT NULL, project_id uuid NOT NULL, installation_id uuid NOT NULL,
 enabled boolean NOT NULL,
 PRIMARY KEY (project_id,installation_id),
 FOREIGN KEY (org_id,installation_id) REFERENCES plugin_installations(org_id,id) ON DELETE CASCADE
);
-- One-use authorizations for tool calls a member approved in a harness (Work): the gateway
-- consumes one instead of asking again.
CREATE TABLE approval_grants (
 id uuid PRIMARY KEY, org_id uuid NOT NULL, run_id uuid NOT NULL, exec_epoch bigint NOT NULL,
 tool text NOT NULL, args_digest text NOT NULL,
 expires_at timestamptz NOT NULL, consumed_at timestamptz,
 created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX approval_grants_lookup ON approval_grants(org_id,run_id,tool,args_digest) WHERE consumed_at IS NULL;
-- The plugins a run started with: [{installation, version}].
ALTER TABLE runs ADD COLUMN plugins jsonb NOT NULL DEFAULT '[]';
-- A gateway call's plugin, and the answers an input request is waiting for.
ALTER TABLE mcp_calls ADD COLUMN installation_id uuid, ADD COLUMN input jsonb;
DO $$ DECLARE t text; BEGIN
 FOREACH t IN ARRAY ARRAY['plugins','plugin_versions','plugin_installations','plugin_grants',
   'plugin_tools','plugin_connections','conversation_plugins','project_plugins','approval_grants'] LOOP
  EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY',t);
  EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY',t);
  EXECUTE format('CREATE POLICY org_isolation ON %I USING (org_id=nullif(current_setting(''app.org_id'',true),'''')::uuid) WITH CHECK (org_id=nullif(current_setting(''app.org_id'',true),'''')::uuid)',t);
  EXECUTE format('GRANT SELECT,INSERT,UPDATE,DELETE ON %I TO ryzhub_app',t);
  EXECUTE format('GRANT SELECT ON %I TO ryzhub_worker',t);
 END LOOP;
END $$;
-- OAuth flows in progress, found by their state when the browser returns (no organization is
-- known then). Short-lived.
CREATE TABLE oauth_flows (
 state text PRIMARY KEY, org_id uuid NOT NULL, user_id uuid NOT NULL, installation_id uuid NOT NULL,
 server text NOT NULL, issuer text NOT NULL, verifier text NOT NULL,
 -- client_id, token_endpoint, resource, scopes, redirect_uri, iss_required
 client jsonb NOT NULL,
 expires_at timestamptz NOT NULL
);
GRANT SELECT,INSERT,UPDATE,DELETE ON oauth_flows TO ryzhub_app;
-- Instance-wide stops, by plugin name or content hash. Read by every organization.
CREATE TABLE plugin_blocks (
 id uuid PRIMARY KEY, name text, content_hash text, reason text NOT NULL DEFAULT '',
 created_at timestamptz NOT NULL DEFAULT now(),
 CHECK (name IS NOT NULL OR content_hash IS NOT NULL)
);
GRANT SELECT,INSERT,DELETE ON plugin_blocks TO ryzhub_app;
GRANT SELECT ON plugin_blocks TO ryzhub_worker;
-- MCP App pages handed to a member's client, served from the plugin content origin by an
-- unguessable token for a short while.
CREATE TABLE plugin_app_pages (
 token text PRIMARY KEY, html text NOT NULL, expires_at timestamptz NOT NULL
);
GRANT SELECT,INSERT,DELETE ON plugin_app_pages TO ryzhub_app;
-- A plugin's question (MCP elicitation): a form, or a page to open.
ALTER TABLE pending_actions ADD COLUMN form jsonb;
-- A Chat plugin call waiting for the member's answer: the server's requests and its state.
ALTER TABLE tool_calls ADD COLUMN input jsonb;
