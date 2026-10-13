-- Audit records may carry structured detail; egress connections record bytes and refusals.
ALTER TABLE audit_logs ADD COLUMN detail jsonb;
-- How far each node's egress log is stored, counted within the host process `egress_boot`.
ALTER TABLE execution_nodes
    ADD COLUMN egress_boot uuid,
    ADD COLUMN egress_seq bigint NOT NULL DEFAULT 0;
