-- A plugin's permissions are the tools it denies; its other rules and `ui.declarative` had no
-- effect.
UPDATE plugin_versions SET manifest = jsonb_set(
  jsonb_set(manifest, '{permissions}', jsonb_build_object('deny', coalesce(
    (SELECT jsonb_agg(t.key ORDER BY t.key) FROM jsonb_each_text(manifest #> '{permissions,tools,tools}') t
     WHERE t.value = 'deny'), '[]'))),
  '{ui}', coalesce(manifest->'ui', '{}') - 'declarative');
-- Nothing read them: an installation's version is what its member confirmed.
DROP TABLE plugin_grants;
-- Installing confirms.
ALTER TABLE plugin_installations DROP CONSTRAINT plugin_installations_state_check,
  ADD CONSTRAINT plugin_installations_state_check CHECK (state IN ('active','needs_reconfirm'));

-- Sandboxes earlier versions only stopped are reclaimed as lost ones are.
UPDATE sandboxes SET state = 'lost' WHERE state = 'stopped';
ALTER TABLE sandboxes DROP CONSTRAINT sandboxes_state_check, ADD CONSTRAINT sandboxes_state_check CHECK (
    state IN ('provisioning', 'running', 'suspended', 'lost', 'archived', 'deleted'));

-- Checkpoints name the harness that wrote them; the earliest only named Codex's version.
UPDATE checkpoints SET manifest = manifest
    || jsonb_build_object('harness', 'codex', 'harness_version', manifest->'codex_version')
  WHERE manifest ? 'codex_version' AND NOT manifest ? 'harness';

-- Runs no longer choose a sandbox profile.
UPDATE conversations SET run_config = run_config - 'env_profile' WHERE run_config ? 'env_profile';
UPDATE runs SET run_config = run_config - 'env_profile' WHERE run_config ? 'env_profile';
UPDATE projects SET defaults = defaults - 'env_profile' WHERE defaults ? 'env_profile';
