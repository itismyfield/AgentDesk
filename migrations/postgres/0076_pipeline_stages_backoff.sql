-- #3868 P2: persist the pipeline stage `backoff` policy (stop silently dropping it).
--
-- The pipeline API (`src/services/pipeline_routes.rs`) has long accepted a
-- `backoff` field on each stage, validated it against STAGE_BACKOFF_VALUES
-- (exponential/linear/none), and then **discarded it** — the INSERT never bound
-- the column and the table had no place to store it. An operator who POSTed
-- `backoff: exponential` got a 200, but the value vanished on the next GET
-- (silent data loss). This adds the missing column so the validated value is
-- actually persisted and round-trips back through the list/GET path.
--
-- Additive, nullable, no default and no backfill: existing rows get NULL, which
-- the API already serializes as `"backoff": null`. `IF NOT EXISTS` keeps the
-- migration idempotent.
--
-- DECLARATIVE-ONLY (see follow-up #3916): persisting `backoff` makes the API's
-- "forward-compat" promise real, but NO runtime executor reads this column yet —
-- like `on_failure` / `max_retries`, it is stored config metadata, not enforced
-- behavior. The retry/backoff executor wiring is tracked by #3916.
ALTER TABLE pipeline_stages
  ADD COLUMN IF NOT EXISTS backoff TEXT;
