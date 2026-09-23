-- Hard execution requirements are central agent policy, snapshotted per intake.
ALTER TABLE agents ADD COLUMN execution_requirements JSONB NOT NULL DEFAULT '{}'::jsonb
    CHECK (jsonb_typeof(execution_requirements) = 'object');
ALTER TABLE intake_outbox ADD COLUMN execution_requirements JSONB NOT NULL DEFAULT '{}'::jsonb
    CHECK (jsonb_typeof(execution_requirements) = 'object');
