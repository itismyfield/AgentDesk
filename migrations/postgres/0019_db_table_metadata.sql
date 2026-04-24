-- #1097 (910-3) DB source-of-truth metadata marker per table.
--
-- Each row declares whether a table is canonical in the database (`db`),
-- canonical on disk as a file/YAML (`file`), or a materialized mirror
-- that is rebuilt at startup from a file (`file-canonical`).
--
-- When `source_of_truth` is `file` or `file-canonical`, mutating API
-- routes are expected to reject writes with HTTP 405 and direct callers
-- to edit the file at `file_path` instead (see `src/db/table_metadata.rs`
-- and the pipeline_stages guard in `src/server/routes/pipeline.rs`).

CREATE TABLE IF NOT EXISTS db_table_metadata (
    table_name      TEXT PRIMARY KEY,
    source_of_truth TEXT NOT NULL CHECK (source_of_truth IN ('db', 'file', 'file-canonical')),
    file_path       TEXT,
    last_synced_at  TIMESTAMPTZ
);

-- Seed the one table this change targets: pipeline_stages is a mirror of
-- policies/default-pipeline.yaml.  The readonly API guard uses this row.
INSERT INTO db_table_metadata (table_name, source_of_truth, file_path, last_synced_at)
VALUES ('pipeline_stages', 'file-canonical', 'policies/default-pipeline.yaml', NULL)
ON CONFLICT (table_name) DO NOTHING;
