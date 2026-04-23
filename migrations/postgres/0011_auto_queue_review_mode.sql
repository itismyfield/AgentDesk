ALTER TABLE auto_queue_runs
ADD COLUMN IF NOT EXISTS review_mode TEXT;

UPDATE auto_queue_runs
SET review_mode = 'enabled'
WHERE review_mode IS NULL;

ALTER TABLE auto_queue_runs
ALTER COLUMN review_mode SET DEFAULT 'enabled';

ALTER TABLE auto_queue_runs
ALTER COLUMN review_mode SET NOT NULL;
