-- #4524: agent archive/unarchive was never used successfully and is removed.

UPDATE agents
SET status = 'idle',
    updated_at = NOW()
WHERE status = 'archived';

ALTER TABLE agents
    DROP CONSTRAINT agents_status_known_check,
    ADD CONSTRAINT agents_status_known_check
        CHECK (status IN ('idle', 'working'));

DROP TABLE IF EXISTS agent_archive;
