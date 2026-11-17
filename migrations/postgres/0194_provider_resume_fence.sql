-- A user clear forces one fresh provider start even after a runtime restart.
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS provider_resume_fence JSONB;
