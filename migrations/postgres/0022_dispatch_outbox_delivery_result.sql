-- 0022_dispatch_outbox_delivery_result.sql
--
-- Operator-facing delivery outcome for dispatch_outbox rows migrated to the
-- shared Discord outbound API (#1165).

ALTER TABLE dispatch_outbox
    ADD COLUMN IF NOT EXISTS delivery_status TEXT;

ALTER TABLE dispatch_outbox
    ADD COLUMN IF NOT EXISTS delivery_result JSONB;
