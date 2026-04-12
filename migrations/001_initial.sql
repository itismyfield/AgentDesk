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
    created_at          DATETIME DEFAULT CURRENT_TIMESTAMP,
    updated_at          DATETIME DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS kanban_cards (
    id                  TEXT PRIMARY KEY,
    repo_id             TEXT,
    title               TEXT NOT NULL,
    status              TEXT DEFAULT 'backlog',
    priority            TEXT DEFAULT 'medium',
    assigned_agent_id   TEXT REFERENCES agents(id),
    github_issue_url    TEXT,
    github_issue_number INTEGER,
    latest_dispatch_id  TEXT,
    review_round        INTEGER DEFAULT 0,
    metadata            TEXT,
    created_at          DATETIME DEFAULT CURRENT_TIMESTAMP,
    updated_at          DATETIME DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS task_dispatches (
    id                  TEXT PRIMARY KEY,
    kanban_card_id      TEXT REFERENCES kanban_cards(id),
    from_agent_id       TEXT,
    to_agent_id         TEXT,
    dispatch_type       TEXT,
    status              TEXT DEFAULT 'pending',
    title               TEXT,
    context             TEXT,
    result              TEXT,
    parent_dispatch_id  TEXT,
    chain_depth         INTEGER DEFAULT 0,
    created_at          DATETIME DEFAULT CURRENT_TIMESTAMP,
    updated_at          DATETIME DEFAULT CURRENT_TIMESTAMP,
    completed_at        DATETIME
);

CREATE TABLE IF NOT EXISTS sessions (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    session_key         TEXT UNIQUE,
    agent_id            TEXT REFERENCES agents(id),
    provider            TEXT DEFAULT 'claude',
    status              TEXT DEFAULT 'disconnected',
    active_dispatch_id  TEXT,
    model               TEXT,
    session_info        TEXT,
    tokens              INTEGER DEFAULT 0,
    cwd                 TEXT,
    last_heartbeat      DATETIME,
    created_at          DATETIME DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS meetings (
    id                  TEXT PRIMARY KEY,
    channel_id          TEXT,
    title               TEXT,
    status              TEXT,
    effective_rounds    INTEGER,
    started_at          DATETIME,
    completed_at        DATETIME,
    summary             TEXT,
    selection_reason    TEXT
);

CREATE TABLE IF NOT EXISTS meeting_transcripts (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    meeting_id          TEXT REFERENCES meetings(id),
    seq                 INTEGER,
    round               INTEGER,
    speaker_agent_id    TEXT,
    speaker_name        TEXT,
    content             TEXT,
    is_summary          BOOLEAN DEFAULT FALSE
);

CREATE TABLE IF NOT EXISTS github_repos (
    id                  TEXT PRIMARY KEY,
    display_name        TEXT,
    sync_enabled        BOOLEAN DEFAULT TRUE,
    last_synced_at      DATETIME
);

CREATE TABLE IF NOT EXISTS dispatch_queue (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    kanban_card_id      TEXT REFERENCES kanban_cards(id),
    priority_score      REAL,
    queued_at           DATETIME DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS pipeline_stages (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    repo_id             TEXT,
    stage_name          TEXT,
    stage_order         INTEGER,
    trigger_after       TEXT,
    entry_skill         TEXT,
    timeout_minutes     INTEGER DEFAULT 60,
    on_failure          TEXT DEFAULT 'fail',
    skip_condition      TEXT
);

CREATE TABLE IF NOT EXISTS skills (
    id                  TEXT PRIMARY KEY,
    name                TEXT,
    description         TEXT,
    source_path         TEXT,
    trigger_patterns    TEXT,
    updated_at          DATETIME
);

CREATE TABLE IF NOT EXISTS skill_usage (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    skill_id            TEXT,
    agent_id            TEXT,
    session_key         TEXT,
    used_at             DATETIME DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS messages (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    sender_type         TEXT,
    sender_id           TEXT,
    receiver_type       TEXT,
    receiver_id         TEXT,
    content             TEXT,
    message_type        TEXT DEFAULT 'chat',
    created_at          DATETIME DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS offices (
    id                  TEXT PRIMARY KEY,
    name                TEXT,
    layout              TEXT
);

CREATE TABLE IF NOT EXISTS departments (
    id                  TEXT PRIMARY KEY,
    name                TEXT,
    office_id           TEXT REFERENCES offices(id)
);

CREATE TABLE IF NOT EXISTS review_decisions (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    kanban_card_id      TEXT REFERENCES kanban_cards(id),
    dispatch_id         TEXT,
    item_index          INTEGER,
    decision            TEXT,
    decided_at          DATETIME DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS rate_limit_cache (
    provider            TEXT PRIMARY KEY,
    data                TEXT,
    fetched_at          INTEGER
);
