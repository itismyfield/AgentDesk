-- #2125 — user-facing phase-gate kind catalog
--
-- Add `phase_gate_kind` to auto_queue_entries so each entry persists the
-- catalog id (pr-confirm / deploy-gate / ...) the caller chose for the gate
-- that follows its batch_phase. NULL means "use the catalog's default_kind"
-- at read time — we don't backfill on existing rows so historical runs stay
-- untouched.
--
-- Validation of the value happens in the application layer
-- (`normalize_generate_entries`) against the in-memory catalog; the DB stores
-- the raw string to keep migrations decoupled from kind evolution.

ALTER TABLE auto_queue_entries
  ADD COLUMN IF NOT EXISTS phase_gate_kind TEXT;
