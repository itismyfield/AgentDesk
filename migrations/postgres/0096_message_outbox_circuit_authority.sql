-- #4615 S3a: dormant channel-scoped message-outbox circuit authority.
--
-- Non-circuit producers leave every new column NULL and retain the existing
-- pending/held lifecycle. Circuit producers stamp the current intake owner and
-- circuit generation; later slices will add the worker delivery fence.

CREATE TABLE IF NOT EXISTS message_outbox_circuit_authority (
    provider            TEXT NOT NULL,
    channel_id          TEXT NOT NULL,
    owner_instance_id   TEXT NOT NULL,
    owner_generation    BIGINT NOT NULL,
    episode_key         TEXT NOT NULL,
    baseline_relay_offset BIGINT NOT NULL,
    open_generation     BIGINT NOT NULL,
    authority_epoch     BIGINT NOT NULL,
    revoked_at          TIMESTAMPTZ,
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (provider, channel_id),
    CONSTRAINT moca_provider_norm CHECK (provider = lower(btrim(provider)) AND provider <> ''),
    CONSTRAINT moca_channel_norm CHECK (channel_id = btrim(channel_id) AND channel_id <> ''),
    CONSTRAINT moca_owner_nonempty CHECK (btrim(owner_instance_id) <> ''),
    CONSTRAINT moca_episode_nonempty CHECK (btrim(episode_key) <> ''),
    CONSTRAINT moca_owner_generation_nonneg CHECK (owner_generation >= 0),
    CONSTRAINT moca_baseline_nonneg CHECK (baseline_relay_offset >= 0),
    CONSTRAINT moca_open_generation_nonneg CHECK (open_generation >= 0),
    CONSTRAINT moca_authority_epoch_positive CHECK (authority_epoch > 0)
);

ALTER TABLE message_outbox ADD COLUMN IF NOT EXISTS circuit_provider TEXT;
ALTER TABLE message_outbox ADD COLUMN IF NOT EXISTS circuit_channel_id TEXT;
ALTER TABLE message_outbox ADD COLUMN IF NOT EXISTS circuit_episode_key TEXT;
ALTER TABLE message_outbox ADD COLUMN IF NOT EXISTS circuit_baseline_relay_offset BIGINT;
ALTER TABLE message_outbox ADD COLUMN IF NOT EXISTS circuit_open_generation BIGINT;
ALTER TABLE message_outbox ADD COLUMN IF NOT EXISTS circuit_authority_epoch BIGINT;
ALTER TABLE message_outbox ADD COLUMN IF NOT EXISTS circuit_dedupe_ttl_secs BIGINT;
ALTER TABLE message_outbox ADD COLUMN IF NOT EXISTS circuit_owner_instance_id TEXT;
ALTER TABLE message_outbox ADD COLUMN IF NOT EXISTS circuit_owner_generation BIGINT;
ALTER TABLE message_outbox ADD COLUMN IF NOT EXISTS cancelled_at TIMESTAMPTZ;
ALTER TABLE message_outbox ADD COLUMN IF NOT EXISTS cancel_reason TEXT;
ALTER TABLE message_outbox ADD COLUMN IF NOT EXISTS delivery_fence_checked_at TIMESTAMPTZ;

-- A cancelled row is terminal and must release its dedupe identity exactly as a
-- failed row does. PostgreSQL cannot replace a partial-index predicate in place,
-- and sqlx runs this migration transactionally, so this DROP/CREATE takes a
-- write-blocking SHARE lock while the index is rebuilt. Keeping both statements
-- in one transaction avoids exposing a no-uniqueness window; an online
-- CONCURRENTLY replacement would require a separately non-transactional
-- migration and cannot atomically preserve the conflict-target contract.
DROP INDEX IF EXISTS uq_message_outbox_active_dedupe_key;
CREATE UNIQUE INDEX uq_message_outbox_active_dedupe_key
    ON message_outbox(dedupe_key)
    WHERE dedupe_key IS NOT NULL
      AND status NOT IN ('failed', 'cancelled');

ALTER TABLE message_outbox ADD CONSTRAINT message_outbox_circuit_stamp_complete
    CHECK (
        (circuit_provider IS NULL
         AND circuit_channel_id IS NULL
         AND circuit_episode_key IS NULL
         AND circuit_baseline_relay_offset IS NULL
         AND circuit_open_generation IS NULL
         AND circuit_authority_epoch IS NULL
         AND circuit_dedupe_ttl_secs IS NULL
         AND circuit_owner_instance_id IS NULL
         AND circuit_owner_generation IS NULL)
        OR
        (circuit_provider IS NOT NULL
         AND circuit_channel_id IS NOT NULL
         AND circuit_episode_key IS NOT NULL
         AND circuit_baseline_relay_offset IS NOT NULL
         AND circuit_open_generation IS NOT NULL
         AND circuit_authority_epoch IS NOT NULL
         AND circuit_dedupe_ttl_secs IS NOT NULL
         AND circuit_owner_instance_id IS NOT NULL
         AND circuit_owner_generation IS NOT NULL)
    ) NOT VALID;

ALTER TABLE message_outbox ADD CONSTRAINT message_outbox_circuit_stamp_values
    CHECK (
        circuit_provider IS NULL OR (
            circuit_provider = lower(btrim(circuit_provider))
            AND circuit_provider <> ''
            AND circuit_channel_id = btrim(circuit_channel_id)
            AND circuit_channel_id <> ''
            AND btrim(circuit_episode_key) <> ''
            AND circuit_baseline_relay_offset >= 0
            AND circuit_open_generation >= 0
            AND circuit_authority_epoch > 0
            AND circuit_dedupe_ttl_secs > 0
            AND btrim(circuit_owner_instance_id) <> ''
            AND circuit_owner_generation >= 0
        )
    ) NOT VALID;

ALTER TABLE message_outbox ADD CONSTRAINT message_outbox_cancelled_metadata
    CHECK (status <> 'cancelled' OR cancelled_at IS NOT NULL) NOT VALID;
