-- Body-free rejection intent; no original prompt, response, snapshot or replay payload is stored.
CREATE TABLE IF NOT EXISTS tui_output_dispositions (
    output_key TEXT PRIMARY KEY,
    identity JSONB NOT NULL,
    reason TEXT NOT NULL,
    template_version INTEGER NOT NULL DEFAULT 1 CHECK (template_version = 1),
    attempt UUID,
    message_id TEXT,
    CHECK ((message_id IS NULL) OR (attempt IS NOT NULL)),
    CHECK (jsonb_typeof(identity) = 'object')
);
