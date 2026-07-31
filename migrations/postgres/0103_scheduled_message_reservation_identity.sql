-- Preserve the synthetic Discord source identity across a scheduled delivery
-- re-arm. A retry must reuse the same headless reservation so queue source-id
-- dedupe treats it as one logical prompt rather than another durable obligation.
ALTER TABLE scheduled_message_deliveries
    ADD COLUMN IF NOT EXISTS reservation_user_msg_id BIGINT;
