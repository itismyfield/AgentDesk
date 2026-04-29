CREATE TABLE IF NOT EXISTS issue_announcements (
    id BIGSERIAL PRIMARY KEY,
    repo TEXT NOT NULL,
    issue_number BIGINT NOT NULL,
    issue_url TEXT,
    title TEXT NOT NULL,
    agent_id TEXT,
    channel_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    completed_at TIMESTAMPTZ,
    completion_pr_number BIGINT,
    completion_pr_url TEXT,
    completion_kind TEXT,
    last_edit_error TEXT,
    invalid_at TIMESTAMPTZ,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT issue_announcements_completion_kind_check
        CHECK (completion_kind IS NULL OR completion_kind IN ('closed', 'merged')),
    CONSTRAINT issue_announcements_repo_issue_unique UNIQUE (repo, issue_number)
);

CREATE INDEX IF NOT EXISTS idx_issue_announcements_open
    ON issue_announcements(repo, issue_number)
    WHERE completed_at IS NULL AND invalid_at IS NULL;

CREATE INDEX IF NOT EXISTS idx_issue_announcements_message
    ON issue_announcements(channel_id, message_id);
