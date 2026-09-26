-- A conversation preference is copied into each new run. Running turns are immutable.
ALTER TABLE conversations ADD COLUMN model_credential_id uuid;
ALTER TABLE conversations ADD CONSTRAINT conversation_credential_tenant
    FOREIGN KEY(org_id,model_credential_id) REFERENCES model_credentials(org_id,id);
