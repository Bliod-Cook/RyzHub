-- Nothing reads or writes instance settings.
DROP TABLE instance_settings;

-- Model keys check the channel's writer epoch, which this copy could drift from.
ALTER TABLE conversations DROP COLUMN owner_epoch;

-- Every run names its harness and model.
ALTER TABLE runs ALTER COLUMN harness DROP DEFAULT, ALTER COLUMN model DROP DEFAULT;

-- Drafts are text only.
ALTER TABLE drafts DROP COLUMN attachment_ids;

ALTER TABLE memory_items DROP COLUMN valid_from;
