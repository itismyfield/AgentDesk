-- #1699: prompt-manifest retention policy + per-layer max full-content size.
--
-- Adds two columns to `prompt_manifest_layers` so write-time truncation and the
-- background retention sweeper can mark a row as "full content trimmed" while
-- always preserving `content_sha256` and metadata for audit.
--
--   * `is_truncated`  — TRUE when stored `full_content` is a prefix of the
--                        original (or NULL because the sweeper trimmed it).
--                        Hash always reflects the *original* content.
--   * `original_bytes` — byte length of the original content. Lets the dashboard
--                        report storage cost without re-reading the (possibly
--                        trimmed) body.
--
-- Both columns are nullable / default false so existing rows remain valid.

ALTER TABLE prompt_manifest_layers
    ADD COLUMN IF NOT EXISTS is_truncated BOOLEAN NOT NULL DEFAULT FALSE;

ALTER TABLE prompt_manifest_layers
    ADD COLUMN IF NOT EXISTS original_bytes BIGINT;

-- Helps the retention sweeper quickly identify rows still carrying full content
-- past the configured horizon (`full_content IS NOT NULL AND created_at < ...`).
CREATE INDEX IF NOT EXISTS idx_prompt_manifest_layers_full_content_trim
    ON prompt_manifest_layers (manifest_id)
    WHERE full_content IS NOT NULL;
