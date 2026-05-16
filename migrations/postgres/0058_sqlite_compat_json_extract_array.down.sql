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
        -- Restore the pre-strict jsonpath implementation so rollback keeps
        -- array-indexed paths such as $.items[0].id working.
        RETURN jsonb_path_query_first(input, path::jsonpath) #>> '{}';
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
