CREATE OR REPLACE FUNCTION json_extract(input JSONB, path TEXT)
RETURNS TEXT
LANGUAGE plpgsql
STABLE
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
        WHEN invalid_parameter_value
            OR invalid_text_representation
            OR syntax_error
            OR invalid_sql_json_subscript
            OR sql_json_array_not_found
            OR sql_json_member_not_found
            OR sql_json_number_not_found
            OR sql_json_object_not_found
            OR singleton_sql_json_item_required
            OR sql_json_scalar_required THEN
            RETURN NULL;
    END;
END;
$$;
