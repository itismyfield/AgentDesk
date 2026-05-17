CREATE INDEX IF NOT EXISTS idx_skill_usage_used_at ON skill_usage (used_at DESC);
CREATE INDEX IF NOT EXISTS idx_skill_usage_skill_id ON skill_usage (skill_id);
CREATE INDEX IF NOT EXISTS idx_skill_usage_agent_id ON skill_usage (agent_id);
