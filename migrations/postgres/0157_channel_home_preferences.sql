CREATE TABLE channel_home_preferences (
    provider TEXT NOT NULL,
    raw_channel_id TEXT NOT NULL,
    mode TEXT NOT NULL,
    home_instance_id TEXT,
    revision BIGINT NOT NULL,
    operation_id UUID NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (provider, raw_channel_id),
    CONSTRAINT chp_provider_normalized CHECK (
        provider <> '' AND provider = lower(btrim(provider))
    ),
    CONSTRAINT chp_channel_normalized CHECK (
        raw_channel_id <> '' AND raw_channel_id = btrim(raw_channel_id)
    ),
    CONSTRAINT chp_mode_home CHECK (
        (mode = 'inherit' AND home_instance_id IS NULL)
        OR (mode = 'override' AND home_instance_id IS NOT NULL
            AND home_instance_id <> ''
            AND home_instance_id = btrim(home_instance_id))
    ),
    CONSTRAINT chp_revision_positive CHECK (revision > 0)
);
