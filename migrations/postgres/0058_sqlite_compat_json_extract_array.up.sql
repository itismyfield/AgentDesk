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

    BEGIN
        -- Use Postgres jsonpath to evaluate SQLite json path syntax.
        -- Use strict mode so structurally mismatched paths return NULL
        -- instead of PostgreSQL lax-mode auto-unwrapping arrays.
        -- Using jsonb_path_query_first gets the first matching element.
        -- Using #>> '{}' converts scalars to text (unquoted strings, etc.)
        -- while preserving objects and arrays as JSON text representations,
        -- which matches SQLite json_extract() behavior.
        RETURN jsonb_path_query_first(input, ('strict ' || path)::jsonpath) #>> '{}';
    EXCEPTION
        WHEN OTHERS THEN
            RETURN NULL;
    END;
END;
$$;
