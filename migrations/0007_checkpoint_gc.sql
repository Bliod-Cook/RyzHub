-- Discovery only; each mutation still runs through a tenant-scoped application transaction.
GRANT SELECT ON checkpoints TO ryzhub_worker;
CREATE INDEX checkpoint_expiry ON checkpoints(state, created_at, delete_after);
