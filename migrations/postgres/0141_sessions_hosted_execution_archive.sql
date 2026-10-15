-- Keep the returned execution on its canonical row without changing legacy NULL semantics.
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS returned_hosted_execution JSONB;
