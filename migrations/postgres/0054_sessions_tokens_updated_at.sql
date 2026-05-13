-- Add `tokens_updated_at` so callers can tell whether `sessions.tokens` reflects
-- a real turn-end snapshot or a stale value carried over since session creation.
-- Needed by the upcoming idle-recap UI (`📦 84k/200k`) which must avoid quoting
-- a token count that was last updated days ago.
--
-- Paired with a `upsert_hook_session_pg` change that:
--   1. preserves `sessions.tokens` when the incoming body omits `tokens`
--      (so `save_provider_session_id` and similar metadata-only hooks no
--      longer zero out a real turn-end snapshot — the root cause of
--      "tokens=0" in /api/sessions even after a token-heavy turn).
--   2. updates `tokens_updated_at = NOW()` only when an explicit `tokens`
--      value arrives, so the freshness stamp is honest.

ALTER TABLE sessions
  ADD COLUMN IF NOT EXISTS tokens_updated_at TIMESTAMPTZ;

CREATE INDEX IF NOT EXISTS sessions_tokens_updated_at_idx
  ON sessions (tokens_updated_at)
  WHERE tokens_updated_at IS NOT NULL;
