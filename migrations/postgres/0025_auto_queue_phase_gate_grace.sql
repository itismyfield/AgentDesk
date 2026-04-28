ALTER TABLE auto_queue_runs
ADD COLUMN IF NOT EXISTS phase_gate_grace_until TIMESTAMPTZ;

WITH ranked AS (
    SELECT
        id,
        ROW_NUMBER() OVER (
            PARTITION BY run_id, phase, COALESCE(dispatch_id, '')
            ORDER BY updated_at DESC NULLS LAST, id DESC
        ) AS row_number
    FROM auto_queue_phase_gates
)
DELETE FROM auto_queue_phase_gates
WHERE id IN (
    SELECT id
    FROM ranked
    WHERE row_number > 1
);

WITH ranked AS (
    SELECT
        id,
        ROW_NUMBER() OVER (
            PARTITION BY dispatch_id
            ORDER BY updated_at DESC NULLS LAST, id DESC
        ) AS row_number
    FROM auto_queue_phase_gates
    WHERE dispatch_id IS NOT NULL
)
DELETE FROM auto_queue_phase_gates
WHERE id IN (
    SELECT id
    FROM ranked
    WHERE row_number > 1
);

CREATE UNIQUE INDEX IF NOT EXISTS uq_aq_phase_gates_run_phase_dispatch_key
    ON auto_queue_phase_gates(run_id, phase, COALESCE(dispatch_id, ''));

CREATE UNIQUE INDEX IF NOT EXISTS uq_aq_phase_gates_dispatch_id
    ON auto_queue_phase_gates(dispatch_id);

CREATE INDEX IF NOT EXISTS idx_aq_phase_gates_run_phase
    ON auto_queue_phase_gates(run_id, phase);

CREATE INDEX IF NOT EXISTS idx_aq_phase_gates_run_status
    ON auto_queue_phase_gates(run_id, status);

CREATE INDEX IF NOT EXISTS idx_aq_phase_gates_phase_dispatch
    ON auto_queue_phase_gates(phase, dispatch_id);
