-- Intake rows routed to a delegated channel carry the home epoch they were routed under; a claim
-- runs only while the channel's row still names that epoch. NULL is a gateway-rule row.
ALTER TABLE intake_outbox ADD COLUMN IF NOT EXISTS home_epoch BIGINT;

-- Every home epoch comes from one sequence, so a deleted and re-delegated channel never repeats an
-- epoch an older row or intake may still carry.
CREATE SEQUENCE IF NOT EXISTS o_channel_home_epochs;
SELECT setval(
    'o_channel_home_epochs',
    COALESCE((SELECT MAX(epoch) FROM o_channel_homes), 0) + 1,
    false
);
