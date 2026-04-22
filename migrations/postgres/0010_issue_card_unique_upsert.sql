CREATE TEMP TABLE github_issue_card_dedupe_map AS
WITH ranked AS (
    SELECT
        id,
        FIRST_VALUE(id) OVER (
            PARTITION BY repo_id, github_issue_number
            ORDER BY created_at ASC NULLS LAST, id ASC
        ) AS canonical_id,
        ROW_NUMBER() OVER (
            PARTITION BY repo_id, github_issue_number
            ORDER BY created_at ASC NULLS LAST, id ASC
        ) AS row_num
    FROM kanban_cards
    WHERE repo_id IS NOT NULL
      AND github_issue_number IS NOT NULL
)
SELECT id AS duplicate_id, canonical_id
FROM ranked
WHERE row_num > 1;

UPDATE task_dispatches td
SET kanban_card_id = map.canonical_id
FROM github_issue_card_dedupe_map map
WHERE td.kanban_card_id = map.duplicate_id;

UPDATE dispatch_queue dq
SET kanban_card_id = map.canonical_id
FROM github_issue_card_dedupe_map map
WHERE dq.kanban_card_id = map.duplicate_id;

UPDATE review_decisions rd
SET kanban_card_id = map.canonical_id
FROM github_issue_card_dedupe_map map
WHERE rd.kanban_card_id = map.duplicate_id;

UPDATE dispatch_events de
SET kanban_card_id = map.canonical_id
FROM github_issue_card_dedupe_map map
WHERE de.kanban_card_id = map.duplicate_id;

UPDATE auto_queue_entries aqe
SET kanban_card_id = map.canonical_id
FROM github_issue_card_dedupe_map map
WHERE aqe.kanban_card_id = map.duplicate_id;

UPDATE auto_queue_phase_gates aqpg
SET anchor_card_id = map.canonical_id
FROM github_issue_card_dedupe_map map
WHERE aqpg.anchor_card_id = map.duplicate_id;

UPDATE kanban_cards kc
SET parent_card_id = map.canonical_id
FROM github_issue_card_dedupe_map map
WHERE kc.parent_card_id = map.duplicate_id;

UPDATE dispatch_outbox dox
SET card_id = map.canonical_id
FROM github_issue_card_dedupe_map map
WHERE dox.card_id = map.duplicate_id;

UPDATE kanban_audit_logs kal
SET card_id = map.canonical_id
FROM github_issue_card_dedupe_map map
WHERE kal.card_id = map.duplicate_id;

UPDATE review_tuning_outcomes rto
SET card_id = map.canonical_id
FROM github_issue_card_dedupe_map map
WHERE rto.card_id = map.duplicate_id;

UPDATE api_friction_events afe
SET card_id = map.canonical_id
FROM github_issue_card_dedupe_map map
WHERE afe.card_id = map.duplicate_id;

INSERT INTO pr_tracking (
    card_id,
    repo_id,
    worktree_path,
    branch,
    pr_number,
    head_sha,
    state,
    last_error,
    dispatch_generation,
    review_round,
    retry_count,
    created_at,
    updated_at
)
SELECT
    map.canonical_id,
    pt.repo_id,
    pt.worktree_path,
    pt.branch,
    pt.pr_number,
    pt.head_sha,
    pt.state,
    pt.last_error,
    pt.dispatch_generation,
    pt.review_round,
    pt.retry_count,
    pt.created_at,
    pt.updated_at
FROM pr_tracking pt
JOIN github_issue_card_dedupe_map map ON map.duplicate_id = pt.card_id
WHERE NOT EXISTS (
    SELECT 1
    FROM pr_tracking existing
    WHERE existing.card_id = map.canonical_id
);

DELETE FROM pr_tracking pt
USING github_issue_card_dedupe_map map
WHERE pt.card_id = map.duplicate_id;

INSERT INTO card_review_state (
    card_id,
    review_round,
    state,
    pending_dispatch_id,
    last_verdict,
    last_decision,
    decided_by,
    decided_at,
    approach_change_round,
    session_reset_round,
    review_entered_at,
    updated_at
)
SELECT
    map.canonical_id,
    crs.review_round,
    crs.state,
    crs.pending_dispatch_id,
    crs.last_verdict,
    crs.last_decision,
    crs.decided_by,
    crs.decided_at,
    crs.approach_change_round,
    crs.session_reset_round,
    crs.review_entered_at,
    crs.updated_at
FROM card_review_state crs
JOIN github_issue_card_dedupe_map map ON map.duplicate_id = crs.card_id
WHERE NOT EXISTS (
    SELECT 1
    FROM card_review_state existing
    WHERE existing.card_id = map.canonical_id
);

DELETE FROM card_review_state crs
USING github_issue_card_dedupe_map map
WHERE crs.card_id = map.duplicate_id;

UPDATE card_retrospectives cr
SET card_id = map.canonical_id
FROM github_issue_card_dedupe_map map
WHERE cr.card_id = map.duplicate_id
  AND NOT EXISTS (
      SELECT 1
      FROM card_retrospectives existing
      WHERE existing.card_id = map.canonical_id
        AND existing.dispatch_id = cr.dispatch_id
        AND existing.terminal_status = cr.terminal_status
  );

DELETE FROM card_retrospectives cr
USING github_issue_card_dedupe_map map
WHERE cr.card_id = map.duplicate_id;

DELETE FROM kanban_cards kc
USING github_issue_card_dedupe_map map
WHERE kc.id = map.duplicate_id;

DROP TABLE github_issue_card_dedupe_map;

CREATE UNIQUE INDEX IF NOT EXISTS idx_kanban_cards_repo_issue_unique
    ON kanban_cards (repo_id, github_issue_number)
    WHERE repo_id IS NOT NULL AND github_issue_number IS NOT NULL;
