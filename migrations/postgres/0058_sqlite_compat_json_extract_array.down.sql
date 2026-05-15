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
