ALTER TABLE workspace_heads ADD COLUMN recovery_required boolean NOT NULL DEFAULT false;
ALTER TABLE workspace_jobs ADD COLUMN restore_checkpoint_id uuid;
ALTER TABLE workspace_jobs ADD COLUMN restore_source_id uuid;
ALTER TABLE workspace_jobs ADD COLUMN restore_applied boolean NOT NULL DEFAULT false;
ALTER TABLE workspace_jobs ADD CONSTRAINT restore_checkpoint_tenant
    FOREIGN KEY(org_id,restore_checkpoint_id) REFERENCES checkpoints(org_id,id);
ALTER TABLE workspace_jobs ADD CONSTRAINT restore_source_tenant
    FOREIGN KEY(org_id,restore_source_id) REFERENCES sandboxes(org_id,id);

-- Preserve an interrupted recovery when upgrading an existing installation.
UPDATE workspace_heads h SET recovery_required=true
WHERE EXISTS(SELECT 1 FROM sandboxes s WHERE s.org_id=h.org_id
    AND s.conversation_id=h.conversation_id AND s.persistent AND s.state='lost')
AND NOT EXISTS(SELECT 1 FROM sandboxes s WHERE s.org_id=h.org_id
    AND s.conversation_id=h.conversation_id AND s.persistent AND s.state IN ('running','archived'));
