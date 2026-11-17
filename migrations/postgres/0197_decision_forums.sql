-- One Decision forum per Discord category. `create_may_have_sent` is committed before the
-- single create request; while it stands without a visible forum, no second create is sent.
CREATE TABLE IF NOT EXISTS decision_forums (
    guild_id TEXT NOT NULL,
    category_id TEXT NOT NULL,
    forum_name TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('ready', 'create_may_have_sent', 'failed')),
    forum_id TEXT UNIQUE,
    error TEXT,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (guild_id, category_id),
    CHECK ((state = 'ready') = (forum_id IS NOT NULL))
);
