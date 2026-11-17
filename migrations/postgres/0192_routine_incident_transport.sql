-- Same-row transport state for the routine failure incident message.
--
-- message_outbox.transport_state       posting is committed before the Discord post; it ends as
--                                      acked (Discord message id stored) or unknown (may have been sent).
-- message_outbox.transport_message_id  the Discord message id an ack recorded.
-- A row that may have been posted keeps its dedupe fence: no worker, activation, GC or cancel path
-- may change its status or dedupe key, give its fence an expiry, or delete it, outside the transitions below.

ALTER TABLE IF EXISTS message_outbox
    ADD COLUMN IF NOT EXISTS transport_state TEXT,
    ADD COLUMN IF NOT EXISTS transport_message_id TEXT;

ALTER TABLE IF EXISTS message_outbox
    ADD CONSTRAINT message_outbox_transport_state_check
        CHECK (
            transport_state IS NULL
            OR (
                transport_state IN ('posting', 'acked', 'unknown')
                AND source = 'routine-runtime'
                AND reason_code = 'routine_failure_incident'
            )
        ),
    ADD CONSTRAINT message_outbox_transport_ack_check
        CHECK (
            transport_state IS DISTINCT FROM 'acked'
            OR NULLIF(btrim(transport_message_id), '') IS NOT NULL
        );

CREATE INDEX IF NOT EXISTS idx_message_outbox_routine_incident
    ON message_outbox (session_key)
    WHERE reason_code = 'routine_failure_incident';

CREATE FUNCTION agentdesk_guard_outbox_transport_v1()
RETURNS TRIGGER AS $$
BEGIN
    -- Skipping the row (RETURN NULL) keeps batch statements such as GC working for other rows.
    IF TG_OP = 'DELETE' THEN
        RETURN NULL;
    END IF;
    -- Making the fence permanent (expiry to NULL) only strengthens it: a redriven row carries a
    -- finite expiry, and re-reserving its key must still return the row.
    IF NEW.dedupe_key IS DISTINCT FROM OLD.dedupe_key
       OR (NEW.dedupe_expires_at IS DISTINCT FROM OLD.dedupe_expires_at
           AND NEW.dedupe_expires_at IS NOT NULL) THEN
        RETURN NULL;
    END IF;
    IF NEW.transport_state IS NOT DISTINCT FROM OLD.transport_state THEN
        IF NEW.status = OLD.status THEN
            RETURN NEW;
        END IF;
        RETURN NULL;
    END IF;
    IF OLD.transport_state = 'posting' AND (
           (NEW.transport_state = 'acked' AND NEW.status = 'sent')
           OR (NEW.transport_state = 'unknown' AND NEW.status = 'held')
           OR (NEW.transport_state IS NULL AND NEW.status IN ('pending', 'failed'))
       ) THEN
        RETURN NEW;
    END IF;
    IF OLD.transport_state = 'unknown' AND NEW.transport_state = 'acked' AND NEW.status = 'sent' THEN
        RETURN NEW;
    END IF;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER trg_outbox_transport_guard_update_v1
    BEFORE UPDATE ON message_outbox FOR EACH ROW
    WHEN (OLD.transport_state IS NOT NULL)
    EXECUTE FUNCTION agentdesk_guard_outbox_transport_v1();

CREATE TRIGGER trg_outbox_transport_guard_delete_v1
    BEFORE DELETE ON message_outbox FOR EACH ROW
    WHEN (OLD.transport_state IS NOT NULL)
    EXECUTE FUNCTION agentdesk_guard_outbox_transport_v1();
