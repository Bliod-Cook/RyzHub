-- usage_records is partitioned by month like audit_logs, so retention drops whole partitions
-- instead of deleting rows. The primary key must include the partition key.
ALTER TABLE usage_records RENAME TO usage_records_unpartitioned;
ALTER INDEX usage_records_pkey RENAME TO usage_records_unpartitioned_pkey;
ALTER INDEX usage_records_org_time RENAME TO usage_records_unpartitioned_org_time;

CREATE TABLE usage_records (
    LIKE usage_records_unpartitioned INCLUDING DEFAULTS,
    PRIMARY KEY (id, created_at),
    FOREIGN KEY (org_id, run_id) REFERENCES runs(org_id, id),
    FOREIGN KEY (org_id, credential_id) REFERENCES model_credentials(org_id, id),
    FOREIGN KEY (user_id) REFERENCES users(id)
) PARTITION BY RANGE (created_at);
CREATE INDEX usage_records_org_time ON usage_records(org_id, created_at);

-- A partition for every month that has rows, plus the next two; maintain creates later ones.
DO $$ DECLARE start_at date; BEGIN
    FOR start_at IN
        SELECT DISTINCT date_trunc('month', created_at)::date FROM usage_records_unpartitioned
        UNION
        SELECT (date_trunc('month', now()) + i * interval '1 month')::date FROM generate_series(0, 2) i
    LOOP
        EXECUTE format('CREATE TABLE %I PARTITION OF usage_records FOR VALUES FROM (%L) TO (%L)',
            'usage_records_' || to_char(start_at, 'YYYY_MM'), start_at, start_at + interval '1 month');
    END LOOP;
END $$;
-- Keeps writes available if maintenance lapses.
CREATE TABLE usage_records_default PARTITION OF usage_records DEFAULT;

INSERT INTO usage_records SELECT * FROM usage_records_unpartitioned;
DROP TABLE usage_records_unpartitioned;

ALTER TABLE usage_records ENABLE ROW LEVEL SECURITY;
ALTER TABLE usage_records FORCE ROW LEVEL SECURITY;
CREATE POLICY org_isolation ON usage_records
    USING (org_id = nullif(current_setting('app.org_id', true), '')::uuid)
    WITH CHECK (org_id = nullif(current_setting('app.org_id', true), '')::uuid);
GRANT SELECT, INSERT, UPDATE, DELETE ON usage_records TO ryzhub_app;
GRANT SELECT ON usage_records TO ryzhub_worker;
-- Parent-table RLS is the only access path; leaf partitions are never granted.
DO $$ DECLARE leaf text; BEGIN
    FOR leaf IN
        SELECT c.relname FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid
        WHERE i.inhparent = 'usage_records'::regclass
    LOOP
        EXECUTE format('REVOKE ALL ON %I FROM ryzhub_app, ryzhub_worker', leaf);
    END LOOP;
END $$;
