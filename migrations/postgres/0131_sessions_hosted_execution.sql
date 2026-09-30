-- Session-lifetime record of a hosted (Herdr) execution. NULL marks a legacy row
-- and stays the value for every existing and newly inserted row.
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS hosted_execution JSONB;

-- Ordinary cleanup may delete only a legacy row or a retired v1 record; any other
-- payload, including one this schema cannot read, keeps the row.
CREATE OR REPLACE FUNCTION agentdesk_hosted_execution_deletable(payload JSONB)
RETURNS BOOLEAN
LANGUAGE SQL
IMMUTABLE
AS $$
    SELECT payload IS NULL
        OR COALESCE(
            jsonb_typeof(payload) = 'object'
                AND payload -> 'schema' = '1'::jsonb
                AND payload ->> 'state' = 'retired',
            FALSE
        )
$$;
