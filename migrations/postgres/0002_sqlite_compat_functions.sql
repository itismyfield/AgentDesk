CREATE OR REPLACE FUNCTION agentdesk_parse_sqlite_datetime(base_value TEXT)
RETURNS TIMESTAMPTZ
LANGUAGE plpgsql
STABLE
AS $$
BEGIN
    IF base_value IS NULL THEN
        RETURN NULL;
    END IF;

    IF lower(base_value) = 'now' THEN
        RETURN NOW();
    END IF;

    RETURN base_value::timestamptz;
EXCEPTION
    WHEN OTHERS THEN
        RETURN NULL;
END;
$$;

CREATE OR REPLACE FUNCTION agentdesk_apply_sqlite_datetime_modifier(
    base_ts TIMESTAMPTZ,
    modifier TEXT
)
RETURNS TIMESTAMPTZ
LANGUAGE plpgsql
STABLE
AS $$
BEGIN
    IF base_ts IS NULL THEN
        RETURN NULL;
    END IF;

    IF modifier IS NULL OR btrim(modifier) = '' THEN
        RETURN base_ts;
    END IF;

    RETURN base_ts + modifier::interval;
EXCEPTION
    WHEN OTHERS THEN
        RETURN NULL;
END;
$$;

CREATE OR REPLACE FUNCTION datetime(base_value TEXT)
RETURNS TIMESTAMPTZ
LANGUAGE SQL
STABLE
AS $$
    SELECT agentdesk_parse_sqlite_datetime(base_value);
$$;

CREATE OR REPLACE FUNCTION datetime(base_value TIMESTAMPTZ)
RETURNS TIMESTAMPTZ
LANGUAGE SQL
STABLE
AS $$
    SELECT base_value;
$$;

CREATE OR REPLACE FUNCTION datetime(base_value TIMESTAMP)
RETURNS TIMESTAMPTZ
LANGUAGE SQL
STABLE
AS $$
    SELECT base_value AT TIME ZONE 'UTC';
$$;

CREATE OR REPLACE FUNCTION datetime(base_value TEXT, modifier TEXT)
RETURNS TIMESTAMPTZ
LANGUAGE SQL
STABLE
AS $$
    SELECT agentdesk_apply_sqlite_datetime_modifier(
        agentdesk_parse_sqlite_datetime(base_value),
        modifier
    );
$$;

CREATE OR REPLACE FUNCTION datetime(base_value TIMESTAMPTZ, modifier TEXT)
RETURNS TIMESTAMPTZ
LANGUAGE SQL
STABLE
AS $$
    SELECT agentdesk_apply_sqlite_datetime_modifier(base_value, modifier);
$$;

CREATE OR REPLACE FUNCTION datetime(base_value TIMESTAMP, modifier TEXT)
RETURNS TIMESTAMPTZ
LANGUAGE SQL
STABLE
AS $$
    SELECT agentdesk_apply_sqlite_datetime_modifier(base_value AT TIME ZONE 'UTC', modifier);
$$;

CREATE OR REPLACE FUNCTION json_extract(input JSONB, path TEXT)
RETURNS TEXT
LANGUAGE plpgsql
IMMUTABLE
AS $$
BEGIN
    IF input IS NULL OR path IS NULL THEN
        RETURN NULL;
    END IF;

    IF path = '$' THEN
        RETURN input::text;
    END IF;

    IF path !~ '^\$((\.[A-Za-z0-9_]+)*)$' THEN
        RETURN NULL;
    END IF;

    RETURN input #>> string_to_array(substring(path FROM 3), '.');
END;
$$;

CREATE OR REPLACE FUNCTION json_extract(input TEXT, path TEXT)
RETURNS TEXT
LANGUAGE plpgsql
IMMUTABLE
AS $$
BEGIN
    IF input IS NULL THEN
        RETURN NULL;
    END IF;

    RETURN json_extract(input::jsonb, path);
EXCEPTION
    WHEN OTHERS THEN
        RETURN NULL;
END;
$$;
