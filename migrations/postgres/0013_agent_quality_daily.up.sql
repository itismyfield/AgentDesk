CREATE TABLE IF NOT EXISTS agent_quality_daily (
    agent_id                       TEXT NOT NULL,
    day                            DATE NOT NULL,
    provider                       TEXT,
    channel_id                     TEXT,
    turn_success_count             BIGINT NOT NULL DEFAULT 0,
    turn_error_count               BIGINT NOT NULL DEFAULT 0,
    review_pass_count              BIGINT NOT NULL DEFAULT 0,
    review_fail_count              BIGINT NOT NULL DEFAULT 0,
    turn_sample_size               BIGINT NOT NULL DEFAULT 0,
    review_sample_size             BIGINT NOT NULL DEFAULT 0,
    sample_size                    BIGINT NOT NULL DEFAULT 0,
    turn_success_rate              DOUBLE PRECISION,
    review_pass_rate               DOUBLE PRECISION,
    turn_success_count_7d          BIGINT NOT NULL DEFAULT 0,
    turn_error_count_7d            BIGINT NOT NULL DEFAULT 0,
    review_pass_count_7d           BIGINT NOT NULL DEFAULT 0,
    review_fail_count_7d           BIGINT NOT NULL DEFAULT 0,
    turn_sample_size_7d            BIGINT NOT NULL DEFAULT 0,
    review_sample_size_7d          BIGINT NOT NULL DEFAULT 0,
    sample_size_7d                 BIGINT NOT NULL DEFAULT 0,
    turn_success_rate_7d           DOUBLE PRECISION,
    review_pass_rate_7d            DOUBLE PRECISION,
    measurement_unavailable_7d     BOOLEAN NOT NULL DEFAULT TRUE,
    turn_success_count_30d         BIGINT NOT NULL DEFAULT 0,
    turn_error_count_30d           BIGINT NOT NULL DEFAULT 0,
    review_pass_count_30d          BIGINT NOT NULL DEFAULT 0,
    review_fail_count_30d          BIGINT NOT NULL DEFAULT 0,
    turn_sample_size_30d           BIGINT NOT NULL DEFAULT 0,
    review_sample_size_30d         BIGINT NOT NULL DEFAULT 0,
    sample_size_30d                BIGINT NOT NULL DEFAULT 0,
    turn_success_rate_30d          DOUBLE PRECISION,
    review_pass_rate_30d           DOUBLE PRECISION,
    measurement_unavailable_30d    BOOLEAN NOT NULL DEFAULT TRUE,
    computed_at                    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (agent_id, day)
);

CREATE INDEX IF NOT EXISTS idx_agent_quality_daily_day
    ON agent_quality_daily(day DESC);

CREATE INDEX IF NOT EXISTS idx_agent_quality_daily_agent_day
    ON agent_quality_daily(agent_id, day DESC);

CREATE INDEX IF NOT EXISTS idx_agent_quality_daily_turn_success_7d
    ON agent_quality_daily(turn_success_rate_7d DESC)
    WHERE measurement_unavailable_7d = FALSE;

CREATE INDEX IF NOT EXISTS idx_agent_quality_daily_review_pass_7d
    ON agent_quality_daily(review_pass_rate_7d DESC)
    WHERE measurement_unavailable_7d = FALSE;
