-- New-session placement is independent of hard requirements and live ownership.
ALTER TABLE agents ADD COLUMN default_execution_node_id TEXT
    CHECK (default_execution_node_id IS NULL OR
           default_execution_node_id ~ '^[A-Za-z0-9_.-]{1,128}$');
