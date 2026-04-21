-- #816 (review fixes): soft-delete `skills` instead of hard-deleting, so the
-- /api/agents/:id/skills INNER JOIN keeps reaching historical rows and
-- transcript-based analytics (collect_known_skills) keep their allowlist.
ALTER TABLE IF EXISTS skills
    ADD COLUMN IF NOT EXISTS deleted_at BIGINT;
