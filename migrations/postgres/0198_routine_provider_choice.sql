-- A routine may pin the provider channel and model its agent turn runs on; NULL inherits the
-- agent primary provider and that channel's model setting.
ALTER TABLE routines ADD COLUMN IF NOT EXISTS provider TEXT;
ALTER TABLE routines ADD COLUMN IF NOT EXISTS model TEXT;
ALTER TABLE routines DROP CONSTRAINT IF EXISTS routines_model_requires_provider_check;
ALTER TABLE routines ADD CONSTRAINT routines_model_requires_provider_check
    CHECK (model IS NULL OR provider IS NOT NULL);

-- Provider-keyed channel targets from config, so a non-primary provider channel (beyond the
-- claude/codex columns) stays resolvable.
ALTER TABLE agents ADD COLUMN IF NOT EXISTS provider_channels JSONB;
