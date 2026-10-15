-- Conversations no longer pin a credential: each request takes the member's, the
-- organization's or the platform's key.
ALTER TABLE conversations DROP COLUMN model_credential_id;
ALTER TABLE runs DROP COLUMN credential_id;
ALTER TABLE virtual_keys DROP COLUMN credential_id;

-- Model requests and responses are no longer kept.
ALTER TABLE usage_records DROP COLUMN content_expires_at, DROP COLUMN retained_content;

-- Credentials live in secrets only; `migrate-secrets` of an earlier version moved the rest.
ALTER TABLE model_credentials
    DROP CONSTRAINT model_credentials_one_key,
    DROP COLUMN encrypted_key,
    DROP COLUMN key_id,
    ADD CONSTRAINT model_credentials_one_key CHECK (secret_id IS NOT NULL OR revoked_at IS NOT NULL);
