-- Image claims require the same transaction-local consumer declaration as mentions.
CREATE FUNCTION agentdesk_require_scheduled_image_consumer_v1()
RETURNS TRIGGER AS $$
BEGIN
    IF NEW.status = 'firing' AND OLD.status IS DISTINCT FROM 'firing'
       AND NEW.image_data IS NOT NULL
       AND COALESCE(current_setting('agentdesk.scheduled_image_consumer_v1', true), '') <> 'enabled'
    THEN
        RAISE EXCEPTION 'scheduled image claim requires consumer capability v1' USING ERRCODE = '55000';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER trg_scheduled_image_consumer_v1
    BEFORE UPDATE ON scheduled_messages FOR EACH ROW
    EXECUTE FUNCTION agentdesk_require_scheduled_image_consumer_v1();

CREATE FUNCTION agentdesk_require_outbox_image_consumer_v1()
RETURNS TRIGGER AS $$
BEGIN
    IF NEW.status = 'processing' AND NEW.attachment_data IS NOT NULL
       AND (OLD.status IS DISTINCT FROM 'processing'
            OR NEW.claim_owner IS DISTINCT FROM OLD.claim_owner
            OR NEW.claimed_at IS DISTINCT FROM OLD.claimed_at)
       AND COALESCE(current_setting('agentdesk.outbox_image_consumer_v1', true), '') <> 'enabled'
    THEN
        RAISE EXCEPTION 'outbox image claim requires consumer capability v1' USING ERRCODE = '55000';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER trg_outbox_image_consumer_v1
    BEFORE UPDATE ON message_outbox FOR EACH ROW
    EXECUTE FUNCTION agentdesk_require_outbox_image_consumer_v1();
