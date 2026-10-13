-- A Chat sandbox outlives its run until `idle_until`, so the conversation's next run reuses it.
ALTER TABLE sandboxes ADD COLUMN idle_until timestamptz;
-- When the node copy of a checkpointed Chat workspace was removed; its next sandbox restores
-- it from the head checkpoint.
ALTER TABLE workspace_heads ADD COLUMN evicted_at timestamptz;
-- Node housekeeping finds idle workspaces and their changes across organizations.
GRANT SELECT ON workspace_heads, tool_calls, checkpoints TO ryzhub_worker;
