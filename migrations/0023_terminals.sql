-- Refreshed while a terminal is attached to a Work sandbox, which keeps it from idle suspension.
ALTER TABLE sandboxes ADD COLUMN terminal_seen_at timestamptz;
