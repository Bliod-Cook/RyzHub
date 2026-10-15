-- Push notifications are the only background jobs: the queue becomes theirs, without the
-- kind, priority and per-job attempt limit nothing else used.
DELETE FROM jobs WHERE kind <> 'push';
ALTER TABLE jobs RENAME TO push_jobs;
ALTER TABLE push_jobs DROP COLUMN kind, DROP COLUMN priority, DROP COLUMN max_attempts;
ALTER INDEX jobs_finished RENAME TO push_jobs_finished;
CREATE UNIQUE INDEX push_jobs_dedupe ON push_jobs(org_id, dedupe_key)
    WHERE dedupe_key IS NOT NULL AND state = 'queued';
CREATE INDEX push_jobs_due ON push_jobs(org_id, run_at) WHERE state IN ('queued', 'running');
