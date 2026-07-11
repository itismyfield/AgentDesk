-- The recovery-safe scheduler always persists a recurrence anchor. Preserve
-- an active trigger-now definition's future parent slot; historical rows use
-- the legacy fallback to their own fire slot. Then restore the non-null
-- invariant expected by the scheduler queries.
UPDATE scheduled_message_deliveries AS delivery
SET resume_scheduled_at = CASE
        WHEN message.status = 'firing'
         AND message.in_flight_delivery_id = delivery.id
        THEN message.scheduled_at
        ELSE delivery.fire_scheduled_at
    END
FROM scheduled_messages AS message
WHERE delivery.scheduled_message_id = message.id
  AND delivery.resume_scheduled_at IS NULL;

ALTER TABLE scheduled_message_deliveries
    ALTER COLUMN resume_scheduled_at SET NOT NULL;
