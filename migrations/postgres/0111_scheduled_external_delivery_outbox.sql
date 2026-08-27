-- Durable provider fan-out for scheduled push messages.
--
-- The existing Discord message_outbox and this provider-neutral outbox are
-- populated in the same transaction as the scheduled fire transition. Raw
-- provider targets exist only while a definition or delivery remains active;
-- terminal transitions scrub them and retain count-only summaries.

ALTER TABLE scheduled_messages
    ADD COLUMN provider_targets JSONB,
    ADD COLUMN provider_target_summary JSONB;

ALTER TABLE scheduled_messages
    DROP CONSTRAINT chk_smsg_push_target_required;

ALTER TABLE scheduled_messages
    ADD CONSTRAINT chk_smsg_provider_target_shape CHECK (
        (provider_targets IS NULL AND provider_target_summary IS NULL)
        OR (
            provider_targets IS NOT NULL
            AND provider_target_summary IS NOT NULL
            AND delivery_kind = 'push'
            AND status IN ('scheduled', 'firing')
        )
        OR (
            provider_targets IS NULL
            AND provider_target_summary IS NOT NULL
            AND delivery_kind = 'push'
            AND status IN ('sent', 'failed', 'canceled', 'expired')
        )
    ),
    ADD CONSTRAINT chk_smsg_push_target_required CHECK (
        delivery_kind <> 'push'
        OR target_channel_id IS NOT NULL
        OR provider_targets IS NOT NULL
    );

COMMENT ON COLUMN scheduled_messages.provider_targets IS
    'Validated provider targets retained only while the reservation is active.';
COMMENT ON COLUMN scheduled_messages.provider_target_summary IS
    'Recipient-free provider/account/count summary safe for API responses.';

CREATE TABLE scheduled_external_delivery_outbox (
    id                    UUID PRIMARY KEY,
    scheduled_delivery_id TEXT NOT NULL REFERENCES scheduled_message_deliveries(id),
    provider              TEXT NOT NULL,
    audience              TEXT NOT NULL,
    account_id            TEXT NOT NULL,
    payload               JSONB,
    requested_count       SMALLINT NOT NULL,
    status                TEXT NOT NULL DEFAULT 'pending',
    claim_owner           TEXT,
    claim_token           UUID,
    claimed_at            TIMESTAMPTZ,
    lease_expires_at      TIMESTAMPTZ,
    dispatch_started_at   TIMESTAMPTZ,
    next_attempt_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    deliver_before        TIMESTAMPTZ NOT NULL,
    retry_count           SMALLINT NOT NULL DEFAULT 0,
    successful_count      SMALLINT,
    failed_count          SMALLINT,
    error_code            TEXT,
    created_at            TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at            TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    finished_at           TIMESTAMPTZ,

    CONSTRAINT uq_scheduled_external_delivery_target
        UNIQUE (scheduled_delivery_id, provider, audience),
    CONSTRAINT chk_scheduled_external_delivery_provider
        CHECK (provider ~ '^[a-z][a-z0-9_-]{0,31}$'),
    CONSTRAINT chk_scheduled_external_delivery_audience
        CHECK (audience ~ '^[a-z][a-z0-9_-]{0,31}$'),
    CONSTRAINT chk_scheduled_external_delivery_account
        CHECK (account_id ~ '^[a-z0-9][a-z0-9-]{0,31}$'),
    CONSTRAINT chk_scheduled_external_delivery_status CHECK (
        status IN ('pending', 'processing', 'success', 'partial_success',
                   'failed', 'unknown')
    ),
    CONSTRAINT chk_scheduled_external_delivery_requested_count
        CHECK (requested_count BETWEEN 1 AND 1000),
    CONSTRAINT chk_scheduled_external_delivery_retry_count
        CHECK (retry_count BETWEEN 0 AND 100),
    CONSTRAINT chk_scheduled_external_delivery_window
        CHECK (deliver_before > created_at),
    CONSTRAINT chk_scheduled_external_delivery_claim CHECK (
        (
            status = 'processing'
            AND claim_owner IS NOT NULL
            AND claim_token IS NOT NULL
            AND claimed_at IS NOT NULL
            AND lease_expires_at IS NOT NULL
        ) OR (
            status <> 'processing'
            AND claim_owner IS NULL
            AND claim_token IS NULL
            AND claimed_at IS NULL
            AND lease_expires_at IS NULL
        )
    ),
    CONSTRAINT chk_scheduled_external_delivery_dispatch CHECK (
        status <> 'pending' OR dispatch_started_at IS NULL
    ),
    CONSTRAINT chk_scheduled_external_delivery_payload CHECK (
        (
            status IN ('pending', 'processing')
            AND payload IS NOT NULL
            AND successful_count IS NULL
            AND failed_count IS NULL
            AND finished_at IS NULL
        ) OR (
            status IN ('success', 'partial_success', 'failed')
            AND payload IS NULL
            AND successful_count IS NOT NULL
            AND failed_count IS NOT NULL
            AND successful_count + failed_count = requested_count
            AND finished_at IS NOT NULL
        ) OR (
            status = 'unknown'
            AND payload IS NULL
            AND successful_count IS NULL
            AND failed_count IS NULL
            AND finished_at IS NOT NULL
        )
    )
);

CREATE INDEX idx_scheduled_external_delivery_claim
    ON scheduled_external_delivery_outbox(next_attempt_at, created_at)
    WHERE status = 'pending';

CREATE INDEX idx_scheduled_external_delivery_lease
    ON scheduled_external_delivery_outbox(lease_expires_at)
    WHERE status = 'processing';

CREATE INDEX idx_scheduled_external_delivery_parent
    ON scheduled_external_delivery_outbox(scheduled_delivery_id, created_at);

CREATE OR REPLACE FUNCTION agentdesk_scrub_terminal_scheduled_provider_targets()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.status IN ('sent', 'failed', 'canceled', 'expired')
       AND NEW.provider_targets IS NOT NULL THEN
        NEW.provider_targets := NULL;
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER trg_scrub_terminal_scheduled_provider_targets
BEFORE INSERT OR UPDATE OF status ON scheduled_messages
FOR EACH ROW
EXECUTE FUNCTION agentdesk_scrub_terminal_scheduled_provider_targets();
