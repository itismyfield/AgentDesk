-- #3022 (follow-up to #3006): persist run -> fresh-session ownership.
--
-- A fresh-strategy routine run that starts an agent turn creates exactly one
-- tmux-backed session. Until now the only persisted link was the run's
-- `turn_id`, and teardown resolved the concrete session by "latest session in
-- the routine log thread". That heuristic is unsafe across a dcserver restart:
-- after boot recovery interrupts a stale `running` run, the session it created
-- is orphaned, and "latest in thread" can no longer positively attribute that
-- orphan to its run (or worse, could match an unrelated session sharing the
-- thread). Recording the exact tmux session the run owns gives boot recovery
-- positive ownership proof so it can reap the orphan precisely and never touch
-- an unrelated live session.
--
-- Nullable with no default: existing rows (and runs that never start an agent
-- turn) legitimately own no session, so NULL means "owns nothing to reap".
ALTER TABLE IF EXISTS routine_runs
    ADD COLUMN IF NOT EXISTS owned_tmux_session TEXT;
