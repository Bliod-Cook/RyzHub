-- The item an item belongs to: the subagent call whose agent produced it.
ALTER TABLE items ADD COLUMN parent_item_id uuid;

-- Members may bring their own Gemini keys.
ALTER TABLE model_credentials DROP CONSTRAINT model_credentials_provider_check;
ALTER TABLE model_credentials ADD CONSTRAINT model_credentials_provider_check
  CHECK(provider IN ('anthropic','openai','gemini'));

-- A model request's retained content: when it is deleted, and the content addresses of the
-- request and response under `llm/<id>/`. NULL when none was kept.
ALTER TABLE usage_records ADD COLUMN content_expires_at timestamptz,
  ADD COLUMN retained_content jsonb;
CREATE INDEX usage_records_content_expiry ON usage_records(content_expires_at)
  WHERE content_expires_at IS NOT NULL;

-- Chat reads tool results moved out of a context back by their call id.
CREATE INDEX tool_calls_call ON tool_calls(org_id, call_id);
