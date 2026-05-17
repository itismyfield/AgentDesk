-- Restore the pre-0058 json_extract(JSONB, TEXT) implementation verbatim
-- from migration 0002_sqlite_compat_functions.sql so rollback reproduces
-- the exact 0057-era DDL (literal-key navigation via #>> string_to_array,
-- IMMUTABLE volatility, no jsonpath operators).
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
