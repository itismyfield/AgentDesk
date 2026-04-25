-- 0021_agent_prompt_commits.sql
--
-- Mapping table joining agent prompt edits (commits) to the downstream
-- 7-day metric window in agent_quality_daily. Owned by #1105 (911-5).
--
-- One row per (agent, commit) pair. The post-commit hook in
-- docs/source-of-truth.md §prompts is the canonical writer; ad-hoc backfill
-- via INSERT ... ON CONFLICT DO NOTHING is allowed and idempotent on the
-- composite primary key.
--
-- See docs/agent-quality.md §6 for the join semantics:
--   window_start = committed_at
--   window_end   = committed_at + INTERVAL '7 days'

CREATE TABLE IF NOT EXISTS agent_prompt_commits (
    agent_id     TEXT        NOT NULL,
    commit_sha   TEXT        NOT NULL,
    committed_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (agent_id, commit_sha)
);

-- Reverse lookup: "what was the most recent prompt edit for this agent
-- before day X?" — used by the dashboard to anchor the 7d correlation
-- window when scrolling through historical metric rows.
CREATE INDEX IF NOT EXISTS idx_agent_prompt_commits_agent_committed
    ON agent_prompt_commits(agent_id, committed_at DESC);

-- Time-window scan: "all prompt edits in the last N days across all agents"
-- — used by the 2-week observation report to count edits in the window.
CREATE INDEX IF NOT EXISTS idx_agent_prompt_commits_committed
    ON agent_prompt_commits(committed_at DESC);
