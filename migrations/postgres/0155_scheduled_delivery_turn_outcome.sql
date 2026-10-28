-- #7420: a completion record bound to the exact turn of an agent delivery.
--
-- A `context_strategy='snapshot'` turn runs under an isolated session key, so
-- its completion never lands in the channel's `session_transcripts` (that would
-- leak the snapshot answer into the live channel context, #4658). The poller
-- read only transcripts and the best-effort `empty_response` quality event, so a
-- normal snapshot answer stayed `running` until the 1800s timeout failed it.
--
-- The turn's completer (bridge postlude, tmux watcher, standby relay, restart
-- recovery) now classifies the outcome and writes it onto the delivery row that
-- launched exactly that turn. The poller reads it before the legacy evidence.
-- First writer wins; a canceled or settled delivery takes no late record.
ALTER TABLE scheduled_message_deliveries
    ADD COLUMN IF NOT EXISTS turn_outcome    TEXT,
    ADD COLUMN IF NOT EXISTS turn_outcome_at TIMESTAMPTZ;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint WHERE conname = 'chk_smdel_turn_outcome'
    ) THEN
        ALTER TABLE scheduled_message_deliveries
            ADD CONSTRAINT chk_smdel_turn_outcome
            CHECK (turn_outcome IS NULL OR turn_outcome IN
                ('delivered', 'no_reply', 'provider_error', 'empty_response'));
    END IF;

    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint WHERE conname = 'chk_smdel_turn_outcome_pair'
    ) THEN
        ALTER TABLE scheduled_message_deliveries
            ADD CONSTRAINT chk_smdel_turn_outcome_pair
            CHECK ((turn_outcome IS NULL) = (turn_outcome_at IS NULL));
    END IF;
END $$;
