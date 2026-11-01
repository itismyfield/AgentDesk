-- The turn nonce of the headless bridge turn that queued this row. Terminal evidence accepts a
-- headless row only for the frame's own turn; rows without it never count as delivery evidence.
ALTER TABLE message_outbox ADD COLUMN IF NOT EXISTS producer_turn_nonce TEXT;
