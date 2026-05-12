-- Migration 0053: automation_candidate_iterations
-- Analog of autoresearch's results.tsv: permanent per-iteration artifact records.
-- Each row records one LLM iteration result for an automation-candidate Kanban card.
-- The keep/discard verdict is computed by Rust (deterministic), not by the LLM.

CREATE TABLE IF NOT EXISTS automation_candidate_iterations (
    id               UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    card_id          TEXT        NOT NULL,
    iteration        INTEGER     NOT NULL CHECK (iteration >= 1),
    branch           TEXT        NOT NULL,
    commit_hash      TEXT,
    metric_before    DOUBLE PRECISION,
    metric_after     DOUBLE PRECISION,
    is_simplification BOOLEAN    NOT NULL DEFAULT FALSE,
    status           TEXT        NOT NULL CHECK (status IN ('keep', 'discard', 'crashed', 'timeout')),
    description      TEXT,
    allowed_write_paths_used  TEXT[],
    run_seconds      INTEGER,
    crash_trace      TEXT,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    UNIQUE (card_id, iteration)
);

CREATE INDEX IF NOT EXISTS idx_aci_card_id     ON automation_candidate_iterations (card_id);
CREATE INDEX IF NOT EXISTS idx_aci_status      ON automation_candidate_iterations (status);
CREATE INDEX IF NOT EXISTS idx_aci_created_at  ON automation_candidate_iterations (created_at DESC);
