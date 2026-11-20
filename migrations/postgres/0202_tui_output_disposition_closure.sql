-- The native turn a rejection closed, persisted with its reason: source identity and turn offsets only.
ALTER TABLE tui_output_dispositions
    ADD COLUMN IF NOT EXISTS closure JSONB
    CONSTRAINT tod_closure_object CHECK (closure IS NULL OR jsonb_typeof(closure) = 'object');
