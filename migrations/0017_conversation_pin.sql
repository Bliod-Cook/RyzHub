-- A pinned conversation leads its creator's list, most recently pinned first.
ALTER TABLE conversations ADD COLUMN pinned_at timestamptz;
