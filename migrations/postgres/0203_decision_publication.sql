-- Where a request's starter post stands: stored (nothing sent), posting (the single POST may have
-- gone out), disabled (the post exists, buttons disabled) or post_failed (Discord refused it).
ALTER TABLE decision_requests
    ADD COLUMN IF NOT EXISTS lifecycle TEXT NOT NULL DEFAULT 'stored'
        CHECK (lifecycle IN ('stored', 'posting', 'disabled', 'post_failed')),
    ADD COLUMN IF NOT EXISTS interface_version SMALLINT,
    ADD COLUMN IF NOT EXISTS provider TEXT,
    ADD COLUMN IF NOT EXISTS bot_user_id TEXT,
    ADD COLUMN IF NOT EXISTS forum_id TEXT,
    ADD COLUMN IF NOT EXISTS thread_id TEXT,
    ADD COLUMN IF NOT EXISTS starter_message_id TEXT,
    ADD COLUMN IF NOT EXISTS post_error TEXT;

-- The opaque button handle of each option, written with the journal before the post.
ALTER TABLE decision_options ADD COLUMN IF NOT EXISTS handle TEXT UNIQUE;
