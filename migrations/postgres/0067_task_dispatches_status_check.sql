UPDATE task_dispatches
   SET status = CASE
       WHEN status IS NULL OR BTRIM(status) = '' THEN 'pending'
       WHEN LOWER(BTRIM(status)) IN ('pending', 'dispatched', 'completed', 'cancelled', 'failed')
           THEN LOWER(BTRIM(status))
       -- Dirty historical/manual values cannot be allowed to abort the
       -- constraint install. Quarantine them in a supported terminal state
       -- so active dispatch queries do not revive unknown legacy rows.
       ELSE 'failed'
   END;

ALTER TABLE task_dispatches
    ALTER COLUMN status SET DEFAULT 'pending';

ALTER TABLE task_dispatches
    ALTER COLUMN status SET NOT NULL;

ALTER TABLE task_dispatches
    ADD CONSTRAINT task_dispatches_status_known_check
    CHECK (status IN ('pending', 'dispatched', 'completed', 'cancelled', 'failed'));
