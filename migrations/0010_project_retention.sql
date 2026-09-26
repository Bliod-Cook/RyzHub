-- Dedicated collector enumerates expired projects, then mutates through tenant RLS.
GRANT SELECT ON projects TO ryzhub_worker;
CREATE INDEX project_expiry ON projects(deleted_at) WHERE state='deletion_pending';
