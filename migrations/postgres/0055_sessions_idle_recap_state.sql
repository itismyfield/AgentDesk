-- Track the active idle-recap notification so the policy loop can delete the
-- previous message before posting a new one each 5-min cycle, and the
-- message_handler can clear the notification the moment the user sends the
-- next message in that channel.
--
-- Paired with the new policy module `policies/timeouts/idle-recap.js` and
-- the new API endpoint `POST /api/sessions/{key}/idle-recap`.

ALTER TABLE sessions
  ADD COLUMN IF NOT EXISTS idle_recap_message_id BIGINT,
  ADD COLUMN IF NOT EXISTS idle_recap_channel_id BIGINT,
  ADD COLUMN IF NOT EXISTS idle_recap_posted_at TIMESTAMPTZ;

-- The partial-index condition is `WHERE idle_recap_message_id IS NOT NULL`,
-- which at migration time matches zero rows (the column was just added),
-- so the non-concurrent build is effectively free and follows the
-- existing CREATE INDEX IF NOT EXISTS pattern used by surrounding
-- migrations (the runner wraps each statement in its own transaction,
-- which CREATE INDEX CONCURRENTLY would forbid).
CREATE INDEX IF NOT EXISTS sessions_idle_recap_channel_idx
  ON sessions (idle_recap_channel_id)
  WHERE idle_recap_message_id IS NOT NULL;
