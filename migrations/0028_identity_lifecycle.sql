-- Signing in with other accounts (OpenID Connect, GitHub). An identity is a provider's
-- subject; it belongs to one user, who may have several. A user without a password signs in
-- only with identities (`password_hash` is then not a hash).
ALTER TABLE user_identities ADD COLUMN email text,
  ADD COLUMN created_at timestamptz NOT NULL DEFAULT now();
CREATE INDEX user_identities_user ON user_identities(user_id);
-- A sign-in in progress, found by its state when the browser returns.
CREATE TABLE login_flows (
 state text PRIMARY KEY, provider text NOT NULL, verifier text NOT NULL, nonce text NOT NULL,
 return_to text NOT NULL,
 -- Adding an identity to this signed-in user rather than signing in.
 link_user uuid REFERENCES users,
 expires_at timestamptz NOT NULL
);
-- A finished sign-in, which the client exchanges once for a session.
CREATE TABLE login_codes (
 code_hash bytea PRIMARY KEY, user_id uuid NOT NULL REFERENCES users, expires_at timestamptz NOT NULL
);
GRANT SELECT,INSERT,UPDATE,DELETE ON login_flows,login_codes TO ryzhub_app;

-- Where a workspace's files are: hot (on an execution node), archiving, cold (only in the
-- checkpoint store), restoring, purging (kept past the archive retention; objects being
-- deleted) and deleted. The timeline outlives the files.
ALTER TABLE workspace_heads ADD COLUMN storage_state text NOT NULL DEFAULT 'hot'
  CHECK (storage_state IN ('hot','archiving','cold','restoring','purging','deleted'));
UPDATE workspace_heads h SET storage_state='cold' FROM conversations c
 WHERE c.org_id=h.org_id AND c.id=h.conversation_id AND c.state='archived';
-- Retention counts from archiving; earlier versions did not record when.
UPDATE conversations SET archived_at=now() WHERE state='archived' AND archived_at IS NULL;
CREATE INDEX conversations_archived ON conversations(org_id,archived_at) WHERE state='archived';
