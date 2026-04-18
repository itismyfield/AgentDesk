CREATE TABLE IF NOT EXISTS kv_meta (
    key        TEXT PRIMARY KEY,
    value      TEXT,
    expires_at TIMESTAMPTZ
);

CREATE TABLE IF NOT EXISTS agents (
    id                  TEXT PRIMARY KEY,
    name                TEXT NOT NULL,
    name_ko             TEXT,
    department          TEXT,
    provider            TEXT DEFAULT 'claude',
    discord_channel_id  TEXT,
    discord_channel_alt TEXT,
    discord_channel_cc  TEXT,
    discord_channel_cdx TEXT,
    avatar_emoji        TEXT,
    status              TEXT DEFAULT 'idle',
    xp                  INTEGER DEFAULT 0,
    skills              TEXT,
    sprite_number       INTEGER,
    description         TEXT,
    system_prompt       TEXT,
    pipeline_config     JSONB,
    created_at          TIMESTAMPTZ DEFAULT NOW(),
    updated_at          TIMESTAMPTZ DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS github_repos (
    id               TEXT PRIMARY KEY,
    display_name     TEXT,
    sync_enabled     BOOLEAN DEFAULT TRUE,
    last_synced_at   TIMESTAMPTZ,
    default_agent_id TEXT,
    pipeline_config  JSONB
);

CREATE TABLE IF NOT EXISTS kanban_cards (
    id                    TEXT PRIMARY KEY,
    repo_id               TEXT,
    title                 TEXT NOT NULL,
    status                TEXT DEFAULT 'backlog',
    priority              TEXT DEFAULT 'medium',
    assigned_agent_id     TEXT REFERENCES agents(id),
    github_issue_url      TEXT,
    github_issue_number   INTEGER,
    latest_dispatch_id    TEXT,
    review_round          INTEGER DEFAULT 0,
    metadata              JSONB,
    started_at            TIMESTAMPTZ,
    completed_at          TIMESTAMPTZ,
    blocked_reason        TEXT,
    pipeline_stage_id     TEXT,
    review_notes          TEXT,
    review_status         TEXT,
    requested_at          TIMESTAMPTZ,
    owner_agent_id        TEXT,
    requester_agent_id    TEXT,
    parent_card_id        TEXT,
    depth                 INTEGER DEFAULT 0,
    sort_order            INTEGER DEFAULT 0,
    description           TEXT,
    active_thread_id      TEXT,
    channel_thread_map    JSONB,
    suggestion_pending_at TIMESTAMPTZ,
    review_entered_at     TIMESTAMPTZ,
    awaiting_dod_at       TIMESTAMPTZ,
    deferred_dod_json     JSONB,
    created_at            TIMESTAMPTZ DEFAULT NOW(),
    updated_at            TIMESTAMPTZ DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS task_dispatches (
    id                 TEXT PRIMARY KEY,
    kanban_card_id     TEXT REFERENCES kanban_cards(id),
    from_agent_id      TEXT,
    to_agent_id        TEXT,
    dispatch_type      TEXT,
    status             TEXT DEFAULT 'pending',
    title              TEXT,
    context            TEXT,
    result             TEXT,
    parent_dispatch_id TEXT,
    chain_depth        INTEGER DEFAULT 0,
    thread_id          TEXT,
    retry_count        INTEGER DEFAULT 0,
    created_at         TIMESTAMPTZ DEFAULT NOW(),
    updated_at         TIMESTAMPTZ DEFAULT NOW(),
    completed_at       TIMESTAMPTZ
);

CREATE TABLE IF NOT EXISTS sessions (
    id                      BIGSERIAL PRIMARY KEY,
    session_key             TEXT UNIQUE,
    agent_id                TEXT REFERENCES agents(id),
    provider                TEXT DEFAULT 'claude',
    status                  TEXT DEFAULT 'disconnected',
    active_dispatch_id      TEXT,
    model                   TEXT,
    session_info            TEXT,
    tokens                  INTEGER DEFAULT 0,
    cwd                     TEXT,
    last_heartbeat          TIMESTAMPTZ,
    thread_channel_id       TEXT,
    claude_session_id       TEXT,
    raw_provider_session_id TEXT,
    created_at              TIMESTAMPTZ DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS meetings (
    id                TEXT PRIMARY KEY,
    channel_id        TEXT,
    title             TEXT,
    status            TEXT,
    effective_rounds  INTEGER,
    started_at        TIMESTAMPTZ,
    completed_at      TIMESTAMPTZ,
    summary           TEXT,
    thread_id         TEXT,
    primary_provider  TEXT,
    reviewer_provider TEXT,
    participant_names TEXT,
    selection_reason  TEXT,
    created_at        BIGINT
);

CREATE TABLE IF NOT EXISTS meeting_transcripts (
    id               BIGSERIAL PRIMARY KEY,
    meeting_id       TEXT REFERENCES meetings(id),
    seq              INTEGER,
    round            INTEGER,
    speaker_agent_id TEXT,
    speaker_name     TEXT,
    content          TEXT,
    is_summary       BOOLEAN DEFAULT FALSE
);

CREATE TABLE IF NOT EXISTS dispatch_queue (
    id             BIGSERIAL PRIMARY KEY,
    kanban_card_id TEXT REFERENCES kanban_cards(id),
    priority_score DOUBLE PRECISION,
    queued_at      TIMESTAMPTZ DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS pipeline_stages (
    id                 BIGSERIAL PRIMARY KEY,
    repo_id            TEXT,
    stage_name         TEXT,
    stage_order        INTEGER,
    trigger_after      TEXT,
    entry_skill        TEXT,
    timeout_minutes    INTEGER DEFAULT 60,
    on_failure         TEXT DEFAULT 'fail',
    skip_condition     TEXT,
    provider           TEXT,
    agent_override_id  TEXT,
    on_failure_target  TEXT,
    max_retries        INTEGER DEFAULT 0,
    parallel_with      TEXT
);

CREATE TABLE IF NOT EXISTS skills (
    id               TEXT PRIMARY KEY,
    name             TEXT,
    description      TEXT,
    source_path      TEXT,
    trigger_patterns TEXT,
    updated_at       TIMESTAMPTZ
);

CREATE TABLE IF NOT EXISTS skill_usage (
    id          BIGSERIAL PRIMARY KEY,
    skill_id    TEXT,
    agent_id    TEXT,
    session_key TEXT,
    used_at     TIMESTAMPTZ DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS turns (
    turn_id             TEXT PRIMARY KEY,
    session_key         TEXT,
    thread_id           TEXT,
    thread_title        TEXT,
    channel_id          TEXT NOT NULL,
    agent_id            TEXT,
    provider            TEXT,
    session_id          TEXT,
    dispatch_id         TEXT,
    started_at          TIMESTAMPTZ NOT NULL,
    finished_at         TIMESTAMPTZ NOT NULL,
    duration_ms         INTEGER,
    input_tokens        INTEGER NOT NULL DEFAULT 0,
    cache_create_tokens INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens   INTEGER NOT NULL DEFAULT 0,
    output_tokens       INTEGER NOT NULL DEFAULT 0,
    created_at          TIMESTAMPTZ DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS messages (
    id            BIGSERIAL PRIMARY KEY,
    sender_type   TEXT,
    sender_id     TEXT,
    receiver_type TEXT,
    receiver_id   TEXT,
    content       TEXT,
    message_type  TEXT DEFAULT 'chat',
    created_at    TIMESTAMPTZ DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS offices (
    id          TEXT PRIMARY KEY,
    name        TEXT,
    layout      TEXT,
    name_ko     TEXT,
    icon        TEXT,
    color       TEXT,
    description TEXT,
    sort_order  INTEGER DEFAULT 0,
    created_at  TIMESTAMPTZ
);

CREATE TABLE IF NOT EXISTS departments (
    id          TEXT PRIMARY KEY,
    name        TEXT,
    office_id   TEXT REFERENCES offices(id),
    name_ko     TEXT,
    icon        TEXT,
    color       TEXT,
    description TEXT,
    sort_order  INTEGER DEFAULT 0,
    created_at  TIMESTAMPTZ
);

CREATE TABLE IF NOT EXISTS office_agents (
    office_id     TEXT NOT NULL,
    agent_id      TEXT NOT NULL,
    department_id TEXT,
    joined_at     TIMESTAMPTZ DEFAULT NOW(),
    PRIMARY KEY (office_id, agent_id)
);

CREATE TABLE IF NOT EXISTS review_decisions (
    id             BIGSERIAL PRIMARY KEY,
    kanban_card_id TEXT REFERENCES kanban_cards(id),
    dispatch_id    TEXT,
    item_index     INTEGER,
    decision       TEXT,
    decided_at     TIMESTAMPTZ DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS rate_limit_cache (
    provider   TEXT PRIMARY KEY,
    data       TEXT,
    fetched_at BIGINT
);

CREATE TABLE IF NOT EXISTS pr_tracking (
    card_id              TEXT PRIMARY KEY REFERENCES kanban_cards(id) ON DELETE CASCADE,
    repo_id              TEXT,
    worktree_path        TEXT,
    branch               TEXT,
    pr_number            INTEGER,
    head_sha             TEXT,
    state                TEXT NOT NULL DEFAULT 'create-pr',
    last_error           TEXT,
    dispatch_generation  TEXT NOT NULL DEFAULT '',
    review_round         INTEGER NOT NULL DEFAULT 0,
    retry_count          INTEGER NOT NULL DEFAULT 0,
    created_at           TIMESTAMPTZ DEFAULT NOW(),
    updated_at           TIMESTAMPTZ DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS card_review_state (
    card_id                TEXT PRIMARY KEY REFERENCES kanban_cards(id),
    review_round           INTEGER NOT NULL DEFAULT 0,
    state                  TEXT NOT NULL DEFAULT 'idle',
    pending_dispatch_id    TEXT,
    last_verdict           TEXT,
    last_decision          TEXT,
    decided_by             TEXT,
    decided_at             TIMESTAMPTZ,
    approach_change_round  INTEGER,
    session_reset_round    INTEGER,
    review_entered_at      TIMESTAMPTZ,
    updated_at             TIMESTAMPTZ DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS deferred_hooks (
    id         BIGSERIAL PRIMARY KEY,
    hook_name  TEXT NOT NULL,
    payload    TEXT NOT NULL DEFAULT '{}',
    status     TEXT NOT NULL DEFAULT 'pending',
    created_at TIMESTAMPTZ DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS message_outbox (
    id         BIGSERIAL PRIMARY KEY,
    target     TEXT NOT NULL,
    content    TEXT NOT NULL,
    bot        TEXT NOT NULL DEFAULT 'announce',
    source     TEXT NOT NULL DEFAULT 'system',
    status     TEXT NOT NULL DEFAULT 'pending',
    created_at TIMESTAMPTZ DEFAULT NOW(),
    sent_at    TIMESTAMPTZ,
    error      TEXT,
    claimed_at TIMESTAMPTZ,
    claim_owner TEXT
);

CREATE TABLE IF NOT EXISTS dispatch_outbox (
    id              BIGSERIAL PRIMARY KEY,
    dispatch_id     TEXT NOT NULL,
    action          TEXT NOT NULL,
    agent_id        TEXT,
    card_id         TEXT,
    title           TEXT,
    status          TEXT NOT NULL DEFAULT 'pending',
    retry_count     INTEGER NOT NULL DEFAULT 0,
    next_attempt_at TIMESTAMPTZ,
    created_at      TIMESTAMPTZ DEFAULT NOW(),
    processed_at    TIMESTAMPTZ,
    error           TEXT
);

CREATE TABLE IF NOT EXISTS kanban_audit_logs (
    id          BIGSERIAL PRIMARY KEY,
    card_id     TEXT,
    from_status TEXT,
    to_status   TEXT,
    source      TEXT,
    result      TEXT,
    created_at  TIMESTAMPTZ DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS audit_logs (
    id          BIGSERIAL PRIMARY KEY,
    entity_type TEXT,
    entity_id   TEXT,
    action      TEXT,
    timestamp   TIMESTAMPTZ DEFAULT NOW(),
    actor       TEXT
);

CREATE TABLE IF NOT EXISTS review_tuning_outcomes (
    id                 BIGSERIAL PRIMARY KEY,
    card_id            TEXT,
    dispatch_id        TEXT,
    review_round       INTEGER,
    verdict            TEXT NOT NULL,
    decision           TEXT,
    outcome            TEXT NOT NULL,
    finding_categories TEXT,
    created_at         TIMESTAMPTZ DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS card_retrospectives (
    id               TEXT PRIMARY KEY,
    card_id          TEXT NOT NULL REFERENCES kanban_cards(id) ON DELETE CASCADE,
    dispatch_id      TEXT NOT NULL REFERENCES task_dispatches(id) ON DELETE CASCADE,
    terminal_status  TEXT NOT NULL,
    repo_id          TEXT,
    issue_number     INTEGER,
    title            TEXT NOT NULL,
    topic            TEXT NOT NULL,
    content          TEXT NOT NULL,
    review_round     INTEGER NOT NULL DEFAULT 0,
    review_notes     TEXT,
    duration_seconds INTEGER,
    success          BOOLEAN NOT NULL DEFAULT FALSE,
    result_json      JSONB NOT NULL,
    memory_payload   JSONB NOT NULL,
    sync_backend     TEXT,
    sync_status      TEXT NOT NULL DEFAULT 'skipped',
    sync_error       TEXT,
    created_at       TIMESTAMPTZ DEFAULT NOW(),
    updated_at       TIMESTAMPTZ DEFAULT NOW(),
    UNIQUE (card_id, dispatch_id, terminal_status)
);

CREATE TABLE IF NOT EXISTS session_termination_events (
    id               BIGSERIAL PRIMARY KEY,
    session_key      TEXT NOT NULL,
    dispatch_id      TEXT,
    killer_component TEXT NOT NULL,
    reason_code      TEXT NOT NULL,
    reason_text      TEXT,
    probe_snapshot   TEXT,
    last_offset      INTEGER,
    tmux_alive       INTEGER,
    created_at       TIMESTAMPTZ DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS dispatch_events (
    id                BIGSERIAL PRIMARY KEY,
    dispatch_id       TEXT NOT NULL REFERENCES task_dispatches(id) ON DELETE CASCADE,
    kanban_card_id    TEXT REFERENCES kanban_cards(id) ON DELETE SET NULL,
    dispatch_type     TEXT,
    from_status       TEXT,
    to_status         TEXT NOT NULL,
    transition_source TEXT NOT NULL,
    payload_json      JSONB,
    created_at        TIMESTAMPTZ DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS runtime_decisions (
    id            BIGSERIAL PRIMARY KEY,
    signal        TEXT NOT NULL,
    evidence_json JSONB NOT NULL,
    chosen_action TEXT NOT NULL,
    actor         TEXT NOT NULL,
    session_key   TEXT,
    dispatch_id   TEXT,
    created_at    TIMESTAMPTZ DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS pending_dm_replies (
    id           BIGSERIAL PRIMARY KEY,
    source_agent TEXT NOT NULL,
    user_id      TEXT NOT NULL,
    channel_id   TEXT,
    context      JSONB NOT NULL DEFAULT '{}'::jsonb,
    status       TEXT NOT NULL DEFAULT 'pending',
    created_at   TIMESTAMPTZ DEFAULT NOW(),
    consumed_at  TIMESTAMPTZ,
    expires_at   TIMESTAMPTZ
);

CREATE TABLE IF NOT EXISTS auto_queue_runs (
    id                        TEXT PRIMARY KEY,
    repo                      TEXT,
    agent_id                  TEXT,
    status                    TEXT DEFAULT 'active',
    ai_model                  TEXT,
    ai_rationale              TEXT,
    timeout_minutes           INTEGER DEFAULT 120,
    unified_thread            BOOLEAN DEFAULT FALSE,
    unified_thread_id         TEXT,
    unified_thread_channel_id TEXT,
    max_concurrent_threads    INTEGER DEFAULT 1,
    thread_group_count        INTEGER DEFAULT 1,
    deploy_phases             TEXT,
    created_at                TIMESTAMPTZ DEFAULT NOW(),
    completed_at              TIMESTAMPTZ
);

CREATE TABLE IF NOT EXISTS auto_queue_entries (
    id             TEXT PRIMARY KEY,
    run_id         TEXT REFERENCES auto_queue_runs(id),
    kanban_card_id TEXT REFERENCES kanban_cards(id),
    agent_id       TEXT,
    priority_rank  INTEGER DEFAULT 0,
    reason         TEXT,
    status         TEXT DEFAULT 'pending',
    dispatch_id    TEXT,
    slot_index     INTEGER,
    thread_group   INTEGER DEFAULT 0,
    batch_phase    INTEGER DEFAULT 0,
    created_at     TIMESTAMPTZ DEFAULT NOW(),
    dispatched_at  TIMESTAMPTZ,
    completed_at   TIMESTAMPTZ
);

CREATE TABLE IF NOT EXISTS auto_queue_slots (
    agent_id              TEXT NOT NULL,
    slot_index            INTEGER NOT NULL,
    assigned_run_id       TEXT,
    assigned_thread_group INTEGER,
    thread_id_map         JSONB,
    created_at            TIMESTAMPTZ DEFAULT NOW(),
    updated_at            TIMESTAMPTZ DEFAULT NOW(),
    PRIMARY KEY (agent_id, slot_index)
);

CREATE TABLE IF NOT EXISTS auto_queue_entry_transitions (
    id             BIGSERIAL PRIMARY KEY,
    entry_id       TEXT NOT NULL,
    from_status    TEXT,
    to_status      TEXT NOT NULL,
    trigger_source TEXT NOT NULL,
    created_at     TIMESTAMPTZ DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS auto_queue_entry_dispatch_history (
    id             BIGSERIAL PRIMARY KEY,
    entry_id       TEXT NOT NULL REFERENCES auto_queue_entries(id) ON DELETE CASCADE,
    dispatch_id    TEXT NOT NULL REFERENCES task_dispatches(id) ON DELETE CASCADE,
    trigger_source TEXT,
    created_at     TIMESTAMPTZ DEFAULT NOW(),
    UNIQUE (entry_id, dispatch_id)
);

CREATE TABLE IF NOT EXISTS auto_queue_phase_gates (
    id             BIGSERIAL PRIMARY KEY,
    run_id         TEXT NOT NULL REFERENCES auto_queue_runs(id) ON DELETE CASCADE,
    phase          INTEGER NOT NULL,
    status         TEXT NOT NULL DEFAULT 'pending',
    verdict        TEXT,
    dispatch_id    TEXT REFERENCES task_dispatches(id) ON DELETE CASCADE
                       CHECK (dispatch_id IS NULL OR BTRIM(dispatch_id) <> ''),
    pass_verdict   TEXT NOT NULL DEFAULT 'phase_gate_passed',
    next_phase     INTEGER,
    final_phase    BOOLEAN NOT NULL DEFAULT FALSE,
    anchor_card_id TEXT REFERENCES kanban_cards(id) ON DELETE SET NULL,
    failure_reason TEXT,
    created_at     TIMESTAMPTZ DEFAULT NOW(),
    updated_at     TIMESTAMPTZ DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS api_friction_events (
    id                  TEXT PRIMARY KEY,
    fingerprint         TEXT NOT NULL,
    endpoint            TEXT NOT NULL,
    friction_type       TEXT NOT NULL,
    summary             TEXT NOT NULL,
    workaround          TEXT,
    suggested_fix       TEXT,
    docs_category       TEXT,
    keywords_json       JSONB NOT NULL DEFAULT '[]'::jsonb,
    payload_json        JSONB NOT NULL,
    session_key         TEXT,
    channel_id          TEXT,
    provider            TEXT,
    dispatch_id         TEXT,
    card_id             TEXT,
    repo_id             TEXT,
    github_issue_number INTEGER,
    task_summary        TEXT,
    agent_id            TEXT,
    memory_backend      TEXT,
    memory_status       TEXT NOT NULL DEFAULT 'pending',
    memory_error        TEXT,
    created_at          TIMESTAMPTZ DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS api_friction_issues (
    fingerprint    TEXT PRIMARY KEY,
    repo_id        TEXT NOT NULL,
    endpoint       TEXT NOT NULL,
    friction_type  TEXT NOT NULL,
    title          TEXT NOT NULL,
    body           TEXT NOT NULL,
    issue_number   INTEGER,
    issue_url      TEXT,
    event_count    INTEGER NOT NULL DEFAULT 0,
    first_event_at TIMESTAMPTZ,
    last_event_at  TIMESTAMPTZ,
    last_error     TEXT,
    created_at     TIMESTAMPTZ DEFAULT NOW(),
    updated_at     TIMESTAMPTZ DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS session_transcripts (
    id                BIGSERIAL PRIMARY KEY,
    turn_id           TEXT NOT NULL UNIQUE,
    session_key       TEXT,
    channel_id        TEXT,
    agent_id          TEXT,
    provider          TEXT,
    dispatch_id       TEXT,
    user_message      TEXT NOT NULL DEFAULT '',
    assistant_message TEXT NOT NULL DEFAULT '',
    events_json       JSONB NOT NULL DEFAULT '[]'::jsonb,
    duration_ms       INTEGER,
    created_at        TIMESTAMPTZ DEFAULT NOW(),
    search_tsv        TSVECTOR GENERATED ALWAYS AS (
        to_tsvector(
            'simple',
            COALESCE(user_message, '') || ' ' || COALESCE(assistant_message, '')
        )
    ) STORED
);

CREATE TABLE IF NOT EXISTS memento_feedback_turn_stats (
    turn_id                     TEXT PRIMARY KEY,
    stat_date                   TEXT NOT NULL,
    agent_id                    TEXT NOT NULL,
    provider                    TEXT NOT NULL,
    recall_count                INTEGER NOT NULL DEFAULT 0,
    manual_tool_feedback_count  INTEGER NOT NULL DEFAULT 0,
    manual_covered_recall_count INTEGER NOT NULL DEFAULT 0,
    auto_tool_feedback_count    INTEGER NOT NULL DEFAULT 0,
    covered_recall_count        INTEGER NOT NULL DEFAULT 0,
    created_at                  TIMESTAMPTZ DEFAULT NOW()
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_single_active_review
    ON task_dispatches (kanban_card_id)
    WHERE dispatch_type = 'review' AND status IN ('pending', 'dispatched');

CREATE UNIQUE INDEX IF NOT EXISTS idx_single_active_review_decision
    ON task_dispatches (kanban_card_id)
    WHERE dispatch_type = 'review-decision' AND status IN ('pending', 'dispatched');

CREATE UNIQUE INDEX IF NOT EXISTS idx_single_active_create_pr
    ON task_dispatches (kanban_card_id)
    WHERE dispatch_type = 'create-pr' AND status IN ('pending', 'dispatched');

CREATE INDEX IF NOT EXISTS idx_pr_tracking_state ON pr_tracking(state);
CREATE UNIQUE INDEX IF NOT EXISTS idx_pr_tracking_repo_pr
    ON pr_tracking(repo_id, pr_number)
    WHERE repo_id IS NOT NULL AND pr_number IS NOT NULL;
CREATE UNIQUE INDEX IF NOT EXISTS uq_pipeline_stages_repo_stage
    ON pipeline_stages(repo_id, stage_name);

CREATE UNIQUE INDEX IF NOT EXISTS uq_dispatch_outbox_one_shot_action
    ON dispatch_outbox(dispatch_id, action)
    WHERE action IN ('notify', 'followup');

CREATE INDEX IF NOT EXISTS idx_message_outbox_status_claimed_at
    ON message_outbox(status, claimed_at, id);

CREATE INDEX IF NOT EXISTS idx_card_retrospectives_card_created
    ON card_retrospectives(card_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_card_retrospectives_issue_created
    ON card_retrospectives(issue_number, created_at DESC)
    WHERE issue_number IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_ste_session_key ON session_termination_events(session_key);
CREATE INDEX IF NOT EXISTS idx_ste_dispatch_id ON session_termination_events(dispatch_id);
CREATE INDEX IF NOT EXISTS idx_ste_created_at ON session_termination_events(created_at);

CREATE INDEX IF NOT EXISTS idx_dispatch_events_dispatch_id
    ON dispatch_events(dispatch_id);
CREATE INDEX IF NOT EXISTS idx_dispatch_events_card_id
    ON dispatch_events(kanban_card_id);
CREATE INDEX IF NOT EXISTS idx_dispatch_events_created_at
    ON dispatch_events(created_at);

CREATE INDEX IF NOT EXISTS idx_runtime_decisions_signal
    ON runtime_decisions(signal);
CREATE INDEX IF NOT EXISTS idx_runtime_decisions_dispatch_id
    ON runtime_decisions(dispatch_id);
CREATE INDEX IF NOT EXISTS idx_runtime_decisions_created_at
    ON runtime_decisions(created_at);

CREATE INDEX IF NOT EXISTS idx_pdr_user_status
    ON pending_dm_replies(user_id, status);

CREATE INDEX IF NOT EXISTS idx_aq_entry_transitions_entry
    ON auto_queue_entry_transitions(entry_id);
CREATE INDEX IF NOT EXISTS idx_aq_entry_transitions_created
    ON auto_queue_entry_transitions(created_at);
CREATE INDEX IF NOT EXISTS idx_aq_entry_dispatch_history_entry
    ON auto_queue_entry_dispatch_history(entry_id);
CREATE INDEX IF NOT EXISTS idx_aq_entry_dispatch_history_dispatch
    ON auto_queue_entry_dispatch_history(dispatch_id);
CREATE INDEX IF NOT EXISTS idx_aq_entry_dispatch_history_created
    ON auto_queue_entry_dispatch_history(created_at);
CREATE UNIQUE INDEX IF NOT EXISTS uq_auto_queue_entry_card
    ON auto_queue_entries(run_id, kanban_card_id)
    WHERE status NOT IN ('skipped', 'cancelled');

CREATE INDEX IF NOT EXISTS idx_api_friction_events_fingerprint
    ON api_friction_events(fingerprint);
CREATE INDEX IF NOT EXISTS idx_api_friction_events_dispatch_id
    ON api_friction_events(dispatch_id);
CREATE INDEX IF NOT EXISTS idx_api_friction_events_created_at
    ON api_friction_events(created_at DESC);
CREATE INDEX IF NOT EXISTS idx_api_friction_issues_repo
    ON api_friction_issues(repo_id, updated_at DESC);

CREATE INDEX IF NOT EXISTS idx_session_transcripts_session_key
    ON session_transcripts(session_key);
CREATE INDEX IF NOT EXISTS idx_session_transcripts_agent_id
    ON session_transcripts(agent_id);
CREATE INDEX IF NOT EXISTS idx_session_transcripts_created_at
    ON session_transcripts(created_at DESC);
CREATE INDEX IF NOT EXISTS idx_session_transcripts_search_tsv
    ON session_transcripts USING GIN(search_tsv);

CREATE INDEX IF NOT EXISTS idx_turns_channel_id
    ON turns(channel_id);
CREATE INDEX IF NOT EXISTS idx_turns_thread_id
    ON turns(thread_id);
CREATE INDEX IF NOT EXISTS idx_turns_agent_id
    ON turns(agent_id);
CREATE INDEX IF NOT EXISTS idx_memento_feedback_turn_stats_date_agent
    ON memento_feedback_turn_stats(stat_date, agent_id, provider);

CREATE OR REPLACE VIEW memento_feedback_daily_stats AS
SELECT
    stat_date,
    agent_id,
    provider,
    SUM(recall_count) AS recall_count,
    SUM(manual_tool_feedback_count + auto_tool_feedback_count) AS tool_feedback_count,
    SUM(manual_tool_feedback_count) AS manual_tool_feedback_count,
    SUM(manual_covered_recall_count) AS manual_covered_recall_count,
    SUM(auto_tool_feedback_count) AS auto_tool_feedback_count,
    SUM(covered_recall_count) AS covered_recall_count,
    CASE
        WHEN SUM(recall_count) > 0
            THEN SUM(manual_covered_recall_count)::DOUBLE PRECISION / SUM(recall_count)
        ELSE 0.0
    END AS compliance_rate,
    CASE
        WHEN SUM(recall_count) > 0
            THEN SUM(covered_recall_count)::DOUBLE PRECISION / SUM(recall_count)
        ELSE 0.0
    END AS coverage_rate
FROM memento_feedback_turn_stats
GROUP BY stat_date, agent_id, provider;
