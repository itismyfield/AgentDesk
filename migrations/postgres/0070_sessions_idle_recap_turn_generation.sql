-- #3148 (follow-up to #3146/#3147): close the residual idle-recap card
-- TOCTOU windows with a per-channel TURN GENERATION counter persisted on the
-- session row.
--
-- A turn claim (TUI or Discord-intake) bumps this counter atomically right
-- after it owns the mailbox turn. The detached idle-recap POST job captures
-- the counter at snapshot load (~20s before it persists its card) and folds a
-- compare-and-swap into the persist UPDATE's WHERE clause: the card is
-- persisted ONLY if the generation is unchanged. Any turn claimed during the
-- compose/persist window bumps the generation, so the persist CAS fails (0
-- rows affected) and the just-posted card is deleted instead of being left
-- over the now-active turn. The claim-bump and the persist-CAS serialize on
-- the same Postgres row, which is the atomicity Window 1 needs (an in-memory
-- counter could not be compared inside the persist UPDATE).

ALTER TABLE sessions
  ADD COLUMN IF NOT EXISTS idle_recap_turn_generation BIGINT NOT NULL DEFAULT 0;
