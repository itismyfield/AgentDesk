ALTER TABLE prompt_manifests
    ADD COLUMN IF NOT EXISTS total_input_bytes BIGINT NOT NULL DEFAULT 0;
