-- Runs created before immutable run_config existed. Preserve their recorded model/harness
-- during a rolling upgrade instead of resolving from today's instance defaults.
UPDATE runs r SET
 protocol=CASE WHEN c.mode='work' AND NOT r.read_only THEN 'responses' ELSE 'anthropic' END,
 run_config=jsonb_build_object(
  'harness',CASE WHEN c.mode='work' AND NOT r.read_only THEN 'codex' ELSE 'chat' END,
  'protocol',CASE WHEN c.mode='work' AND NOT r.read_only THEN 'responses' ELSE 'anthropic' END,
  'model',r.model,'reasoning','medium','approval','ask','plan',false)
FROM conversations c WHERE r.org_id=c.org_id AND r.conversation_id=c.id AND r.run_config='{}';

UPDATE conversations c SET run_config=(
 SELECT r.run_config FROM runs r
 WHERE r.org_id=c.org_id AND r.conversation_id=c.id AND NOT r.read_only AND r.harness<>'mock'
 ORDER BY r.started_at DESC,r.id DESC LIMIT 1
) WHERE c.run_config='{}' AND EXISTS(
 SELECT 1 FROM runs r WHERE r.org_id=c.org_id AND r.conversation_id=c.id AND NOT r.read_only AND r.harness<>'mock'
);
