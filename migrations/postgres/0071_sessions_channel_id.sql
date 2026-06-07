-- #3207 (part 2) P0: scope worktree reuse by the unique Discord channel id.
--
-- The worktree-reuse resolve (`resolve_reusable_worktree` →
-- `restore_thread_worktree_path_from_db`) matches the persisted `sessions.cwd`
-- by `session_key`, which is derived from the sanitized/truncated channel NAME
-- (`provider/token/hostname:tmux_name`) — it carries NO channel/guild id. Two
-- ordinary channels whose names sanitize+truncate to the same value therefore
-- collide onto a single `session_key` row and would resolve EACH OTHER's cwd,
-- silently resuming one channel into another channel's working tree.
--
-- Persist the unique Discord channel id on the session row so the resolve can
-- require an exact `channel_id` match. A name collision then falls through to a
-- fresh worktree (whoever last ran in that session_key owns the worktree) and
-- can NEVER cross channels. Distinct from `thread_channel_id`, which stays NULL
-- for non-thread/fixed-channel sessions and gates a lot of GC logic — this is
-- the actual channel the turn runs in (thread id for threads, channel id for
-- ordinary channels).
ALTER TABLE sessions
  ADD COLUMN IF NOT EXISTS channel_id TEXT;

CREATE INDEX IF NOT EXISTS sessions_channel_id_idx
  ON sessions (channel_id)
  WHERE channel_id IS NOT NULL;
