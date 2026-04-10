use anyhow::Result;
use rusqlite::Connection;

const AGENTDESK_REPO_ID: &str = "itismyfield/AgentDesk";

pub fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS kv_meta (
            key   TEXT PRIMARY KEY,
            value TEXT
        );",
    )?;

    let version: i64 = conn
        .query_row(
            "SELECT COALESCE((SELECT value FROM kv_meta WHERE key = 'schema_version'), '0')",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);

    if version < 1 {
        conn.execute_batch(include_str!("../../migrations/001_initial.sql"))?;
        conn.execute(
            "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('schema_version', '1')",
            [],
        )?;
        tracing::info!("Applied migration 001_initial");
    }

    // Ensure office_agents join table exists (additive, no migration bump needed)
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS office_agents (
            office_id   TEXT NOT NULL,
            agent_id    TEXT NOT NULL,
            department_id TEXT,
            joined_at   TEXT DEFAULT CURRENT_TIMESTAMP,
            PRIMARY KEY (office_id, agent_id)
        );",
    )?;

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS pr_tracking (
            card_id        TEXT PRIMARY KEY REFERENCES kanban_cards(id) ON DELETE CASCADE,
            repo_id        TEXT,
            worktree_path  TEXT,
            branch         TEXT,
            pr_number      INTEGER,
            head_sha       TEXT,
            state          TEXT NOT NULL DEFAULT 'create-pr',
            last_error     TEXT,
            created_at     TEXT DEFAULT CURRENT_TIMESTAMP,
            updated_at     TEXT DEFAULT CURRENT_TIMESTAMP
        );
        CREATE INDEX IF NOT EXISTS idx_pr_tracking_state ON pr_tracking(state);
        CREATE UNIQUE INDEX IF NOT EXISTS idx_pr_tracking_repo_pr
        ON pr_tracking(repo_id, pr_number)
        WHERE repo_id IS NOT NULL AND pr_number IS NOT NULL;",
    )?;

    // Additive columns — ALTER TABLE errors are ignored if column already exists
    let _ = conn.execute_batch("ALTER TABLE kanban_cards ADD COLUMN deferred_dod_json TEXT;");
    let _ = conn.execute_batch("ALTER TABLE github_repos ADD COLUMN default_agent_id TEXT;");
    let _ = conn.execute_batch("ALTER TABLE agents ADD COLUMN sprite_number INTEGER DEFAULT NULL;");
    let _ = conn.execute_batch("ALTER TABLE agents ADD COLUMN description TEXT;");
    let _ = conn.execute_batch("ALTER TABLE agents ADD COLUMN system_prompt TEXT;");
    let _ = conn.execute_batch("ALTER TABLE agents ADD COLUMN discord_channel_cc TEXT;");
    let _ = conn.execute_batch("ALTER TABLE agents ADD COLUMN discord_channel_cdx TEXT;");
    // #135: Per-repo and per-agent pipeline override (JSON)
    let _ = conn.execute_batch("ALTER TABLE github_repos ADD COLUMN pipeline_config TEXT;");
    let _ = conn.execute_batch("ALTER TABLE agents ADD COLUMN pipeline_config TEXT;");
    let _ = conn.execute_batch("ALTER TABLE task_dispatches ADD COLUMN thread_id TEXT;");
    let _ =
        conn.execute_batch("ALTER TABLE task_dispatches ADD COLUMN retry_count INTEGER DEFAULT 0;");
    let _ = conn.execute_batch("ALTER TABLE task_dispatches ADD COLUMN completed_at DATETIME;");
    let _ = conn.execute_batch("ALTER TABLE meetings ADD COLUMN thread_id TEXT;");
    let _ = conn.execute_batch("ALTER TABLE meetings ADD COLUMN primary_provider TEXT;");
    let _ = conn.execute_batch("ALTER TABLE meetings ADD COLUMN reviewer_provider TEXT;");
    let _ = conn.execute_batch("ALTER TABLE meetings ADD COLUMN participant_names TEXT;");
    let _ = conn.execute_batch("ALTER TABLE meetings ADD COLUMN created_at INTEGER;");
    let _ = conn.execute_batch("ALTER TABLE sessions ADD COLUMN thread_channel_id TEXT;");
    let _ = conn.execute_batch("ALTER TABLE sessions ADD COLUMN claude_session_id TEXT;");
    ensure_session_transcripts_schema(conn)?;

    // Office/department extended columns
    let _ = conn.execute_batch("ALTER TABLE offices ADD COLUMN name_ko TEXT;");
    let _ = conn.execute_batch("ALTER TABLE offices ADD COLUMN icon TEXT;");
    let _ = conn.execute_batch("ALTER TABLE offices ADD COLUMN color TEXT;");
    let _ = conn.execute_batch("ALTER TABLE offices ADD COLUMN description TEXT;");
    let _ = conn.execute_batch("ALTER TABLE offices ADD COLUMN sort_order INTEGER DEFAULT 0;");
    let _ = conn.execute_batch("ALTER TABLE offices ADD COLUMN created_at TEXT;");
    let _ = conn.execute_batch("ALTER TABLE departments ADD COLUMN name_ko TEXT;");
    let _ = conn.execute_batch("ALTER TABLE departments ADD COLUMN icon TEXT;");
    let _ = conn.execute_batch("ALTER TABLE departments ADD COLUMN color TEXT;");
    let _ = conn.execute_batch("ALTER TABLE departments ADD COLUMN description TEXT;");
    let _ = conn.execute_batch("ALTER TABLE departments ADD COLUMN sort_order INTEGER DEFAULT 0;");
    let _ = conn.execute_batch("ALTER TABLE departments ADD COLUMN created_at TEXT;");

    // Pipeline stages extension columns (dashboard v2)
    let _ = conn.execute_batch("ALTER TABLE pipeline_stages ADD COLUMN provider TEXT;");
    let _ = conn.execute_batch("ALTER TABLE pipeline_stages ADD COLUMN agent_override_id TEXT;");
    let _ = conn.execute_batch("ALTER TABLE pipeline_stages ADD COLUMN on_failure_target TEXT;");
    let _ =
        conn.execute_batch("ALTER TABLE pipeline_stages ADD COLUMN max_retries INTEGER DEFAULT 0;");
    let _ = conn.execute_batch("ALTER TABLE pipeline_stages ADD COLUMN parallel_with TEXT;");

    // Kanban card extended columns for policies
    let _ = conn.execute_batch("ALTER TABLE kanban_cards ADD COLUMN started_at TEXT;");
    let _ = conn.execute_batch("ALTER TABLE kanban_cards ADD COLUMN completed_at TEXT;");
    let _ = conn.execute_batch("ALTER TABLE kanban_cards ADD COLUMN blocked_reason TEXT;");
    let _ = conn.execute_batch("ALTER TABLE kanban_cards ADD COLUMN pipeline_stage_id TEXT;");
    let _ = conn.execute_batch("ALTER TABLE kanban_cards ADD COLUMN review_notes TEXT;");
    let _ = conn.execute_batch("ALTER TABLE kanban_cards ADD COLUMN review_status TEXT;");
    let _ = conn.execute_batch("ALTER TABLE kanban_cards ADD COLUMN requested_at TEXT;");
    let _ = conn.execute_batch("ALTER TABLE kanban_cards ADD COLUMN owner_agent_id TEXT;");
    let _ = conn.execute_batch("ALTER TABLE kanban_cards ADD COLUMN requester_agent_id TEXT;");
    let _ = conn.execute_batch("ALTER TABLE kanban_cards ADD COLUMN parent_card_id TEXT;");
    let _ = conn.execute_batch("ALTER TABLE kanban_cards ADD COLUMN depth INTEGER DEFAULT 0;");
    let _ = conn.execute_batch("ALTER TABLE kanban_cards ADD COLUMN sort_order INTEGER DEFAULT 0;");
    let _ = conn.execute_batch("ALTER TABLE kanban_cards ADD COLUMN description TEXT;");
    let _ = conn.execute_batch("ALTER TABLE kanban_cards ADD COLUMN active_thread_id TEXT;");
    let _ = conn.execute_batch("ALTER TABLE kanban_cards ADD COLUMN channel_thread_map TEXT;");
    let _ = conn.execute_batch("ALTER TABLE kanban_cards ADD COLUMN suggestion_pending_at TEXT;");
    let _ = conn.execute_batch("ALTER TABLE kanban_cards ADD COLUMN review_entered_at TEXT;");
    let _ = conn.execute_batch("ALTER TABLE kanban_cards ADD COLUMN awaiting_dod_at TEXT;");
    let _ = conn.execute_batch(
        "UPDATE agents
         SET discord_channel_cc = COALESCE(NULLIF(TRIM(discord_channel_cc), ''), NULLIF(TRIM(discord_channel_id), '')),
             discord_channel_cdx = COALESCE(NULLIF(TRIM(discord_channel_cdx), ''), NULLIF(TRIM(discord_channel_alt), ''));
         UPDATE agents
         SET discord_channel_id = COALESCE(NULLIF(TRIM(discord_channel_id), ''), NULLIF(TRIM(discord_channel_cc), '')),
             discord_channel_alt = COALESCE(NULLIF(TRIM(discord_channel_alt), ''), NULLIF(TRIM(discord_channel_cdx), ''));",
    );

    // Backfill lifecycle timestamps for existing cards that predate these columns.
    // Uses updated_at as best-available approximation; future transitions will use exact timestamps.
    let _ = conn.execute_batch(
        "UPDATE task_dispatches
         SET completed_at = COALESCE(completed_at, updated_at)
         WHERE status = 'completed' AND completed_at IS NULL;",
    );
    let _ = conn.execute_batch(
        "UPDATE kanban_cards SET requested_at = updated_at WHERE status = 'requested' AND requested_at IS NULL;
         UPDATE kanban_cards SET started_at = updated_at WHERE status = 'in_progress' AND started_at IS NULL;
         UPDATE kanban_cards SET review_entered_at = updated_at WHERE status = 'review' AND review_entered_at IS NULL;
         UPDATE kanban_cards SET awaiting_dod_at = updated_at WHERE status = 'review' AND review_status = 'awaiting_dod' AND awaiting_dod_at IS NULL;",
    );
    ensure_auto_queue_schema(conn)?;

    // Unique constraint: one kanban card per GitHub issue per repo.
    // Deduplicate existing rows first so CREATE UNIQUE INDEX succeeds.
    // Strategy: for each duplicate (github_issue_number, repo_id) group,
    // pick the "survivor" — the card with FK references (task_dispatches,
    // auto_queue_entries, review_decisions), or the most recently updated one.
    // Re-point all FK references to the survivor, then delete the rest.
    let _ = conn
        .execute_batch(
            "-- Re-point FK references from duplicate cards to the survivor.
             -- Survivor = the card with the most recent updated_at in each group.
             UPDATE task_dispatches SET kanban_card_id = (
                 SELECT kc2.id FROM kanban_cards kc2
                 WHERE kc2.github_issue_number = (
                     SELECT github_issue_number FROM kanban_cards WHERE id = task_dispatches.kanban_card_id
                 )
                 AND kc2.repo_id = (
                     SELECT repo_id FROM kanban_cards WHERE id = task_dispatches.kanban_card_id
                 )
                 ORDER BY kc2.updated_at DESC, kc2.created_at DESC
                 LIMIT 1
             )
             WHERE kanban_card_id IN (
                 SELECT id FROM kanban_cards kc
                 WHERE github_issue_number IS NOT NULL AND repo_id IS NOT NULL
                 AND EXISTS (
                     SELECT 1 FROM kanban_cards kc3
                     WHERE kc3.github_issue_number = kc.github_issue_number
                     AND kc3.repo_id = kc.repo_id
                     AND kc3.id != kc.id
                 )
             );
             UPDATE auto_queue_entries SET kanban_card_id = (
                 SELECT kc2.id FROM kanban_cards kc2
                 WHERE kc2.github_issue_number = (
                     SELECT github_issue_number FROM kanban_cards WHERE id = auto_queue_entries.kanban_card_id
                 )
                 AND kc2.repo_id = (
                     SELECT repo_id FROM kanban_cards WHERE id = auto_queue_entries.kanban_card_id
                 )
                 ORDER BY kc2.updated_at DESC, kc2.created_at DESC
                 LIMIT 1
             )
             WHERE kanban_card_id IN (
                 SELECT id FROM kanban_cards kc
                 WHERE github_issue_number IS NOT NULL AND repo_id IS NOT NULL
                 AND EXISTS (
                     SELECT 1 FROM kanban_cards kc3
                     WHERE kc3.github_issue_number = kc.github_issue_number
                     AND kc3.repo_id = kc.repo_id
                     AND kc3.id != kc.id
                 )
             );
             UPDATE review_decisions SET kanban_card_id = (
                 SELECT kc2.id FROM kanban_cards kc2
                 WHERE kc2.github_issue_number = (
                     SELECT github_issue_number FROM kanban_cards WHERE id = review_decisions.kanban_card_id
                 )
                 AND kc2.repo_id = (
                     SELECT repo_id FROM kanban_cards WHERE id = review_decisions.kanban_card_id
                 )
                 ORDER BY kc2.updated_at DESC, kc2.created_at DESC
                 LIMIT 1
             )
             WHERE kanban_card_id IN (
                 SELECT id FROM kanban_cards kc
                 WHERE github_issue_number IS NOT NULL AND repo_id IS NOT NULL
                 AND EXISTS (
                     SELECT 1 FROM kanban_cards kc3
                     WHERE kc3.github_issue_number = kc.github_issue_number
                     AND kc3.repo_id = kc.repo_id
                     AND kc3.id != kc.id
                 )
             );",
        )
        .ok();
    // Now delete the non-survivor duplicates (FK references already re-pointed).
    // Survivor = most recently updated card per (github_issue_number, repo_id).
    let deleted = conn
        .execute(
            "DELETE FROM kanban_cards
             WHERE github_issue_number IS NOT NULL AND repo_id IS NOT NULL
             AND id NOT IN (
                 SELECT id FROM (
                     SELECT id, ROW_NUMBER() OVER (
                         PARTITION BY github_issue_number, repo_id
                         ORDER BY updated_at DESC, created_at DESC
                     ) AS rn
                     FROM kanban_cards
                     WHERE github_issue_number IS NOT NULL AND repo_id IS NOT NULL
                 ) WHERE rn = 1
             )",
            [],
        )
        .unwrap_or(0);
    if deleted > 0 {
        tracing::warn!(
            "Cleaned up {deleted} duplicate kanban_cards rows (by github_issue_number, repo_id)"
        );
    }
    if let Err(e) = conn.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_kanban_cards_issue_repo \
         ON kanban_cards (github_issue_number, repo_id) \
         WHERE github_issue_number IS NOT NULL AND repo_id IS NOT NULL;",
    ) {
        tracing::error!("Failed to create unique index idx_kanban_cards_issue_repo: {e}");
    }

    // Remove stale kv_meta key that is no longer used (replaced by kanban_manager_channel_id)
    let _ = conn.execute("DELETE FROM kv_meta WHERE key = 'pmd_channel_id'", []);

    // Clean up stale review_status on done cards (fix for #80 — dismiss review loop)
    let cleaned = conn
        .execute(
            "UPDATE kanban_cards SET review_status = NULL WHERE status = 'done' AND review_status IS NOT NULL",
            [],
        )
        .unwrap_or(0);
    if cleaned > 0 {
        tracing::info!("Cleaned {cleaned} done cards with stale review_status (fix #80)");
    }
    // Cancel stale pending review/review-decision dispatches for done cards
    let cancelled_ids: Vec<String> = conn
        .prepare(
            "SELECT id FROM task_dispatches \
             WHERE status IN ('pending', 'dispatched') \
             AND dispatch_type IN ('review', 'review-decision') \
             AND kanban_card_id IN (SELECT id FROM kanban_cards WHERE status = 'done')",
        )
        .ok()
        .and_then(|mut stmt| {
            stmt.query_map([], |row| row.get::<_, String>(0))
                .ok()
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();
    let mut cancelled = 0;
    for dispatch_id in &cancelled_ids {
        cancelled +=
            crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_conn(conn, dispatch_id, None)
                .unwrap_or(0);
    }
    if cancelled > 0 {
        tracing::info!("Cancelled {cancelled} stale review dispatches for done cards (fix #80)");
    }

    // #116: Cancel duplicate pending review-decisions on non-done cards.
    // Keeps only the most recent (highest rowid) pending review-decision per card.
    let duplicate_ids: Vec<String> = conn
        .prepare(
            "SELECT id FROM task_dispatches \
             WHERE dispatch_type = 'review-decision' AND status IN ('pending', 'dispatched') \
             AND rowid NOT IN ( \
                 SELECT MAX(rowid) FROM task_dispatches \
                 WHERE dispatch_type = 'review-decision' AND status IN ('pending', 'dispatched') \
                 GROUP BY kanban_card_id \
             )",
        )
        .ok()
        .and_then(|mut stmt| {
            stmt.query_map([], |row| row.get::<_, String>(0))
                .ok()
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();
    let mut dup_cancelled: usize = 0;
    for dispatch_id in &duplicate_ids {
        dup_cancelled += crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_conn(
            conn,
            dispatch_id,
            Some("startup_reconcile_duplicate"),
        )
        .unwrap_or(0);
    }
    if dup_cancelled > 0 {
        tracing::info!(
            "Cancelled {dup_cancelled} duplicate pending review-decisions at startup (#116)"
        );
    }

    // #116: Idempotent pointer fix — always re-point latest_dispatch_id for cards
    // that have an active review-decision but latest_dispatch_id doesn't point to it.
    // This covers both freshly-cancelled duplicates AND broken state left by prior builds.
    let repointed: usize = conn
        .execute(
            "UPDATE kanban_cards SET latest_dispatch_id = ( \
                 SELECT td.id FROM task_dispatches td \
                 WHERE td.kanban_card_id = kanban_cards.id \
                 AND td.dispatch_type = 'review-decision' \
                 AND td.status IN ('pending', 'dispatched') \
                 ORDER BY td.rowid DESC LIMIT 1 \
             ) \
             WHERE id IN ( \
                 SELECT td2.kanban_card_id FROM task_dispatches td2 \
                 JOIN kanban_cards kc ON kc.id = td2.kanban_card_id \
                 WHERE td2.dispatch_type = 'review-decision' \
                 AND td2.status IN ('pending', 'dispatched') \
                 AND (kc.latest_dispatch_id IS NULL OR kc.latest_dispatch_id != td2.id) \
             )",
            [],
        )
        .unwrap_or(0);
    if repointed > 0 {
        tracing::info!(
            "Re-pointed latest_dispatch_id on {repointed} card(s) to active review-decision (#116)"
        );
    }

    // #116: Partial unique index — at most 1 active review-decision per card at DB level.
    // This prevents race conditions where concurrent create_dispatch_core calls
    // both see no pending review-decision and each insert one.
    let _ = conn.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_single_active_review_decision \
         ON task_dispatches (kanban_card_id) \
         WHERE dispatch_type = 'review-decision' AND status IN ('pending', 'dispatched');",
    );

    // #117: Canonical card-level review state — single source of truth for review lifecycle.
    // Replaces the scattered review_status/review_round/latest_dispatch_id as the canonical record.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS card_review_state (
            card_id             TEXT PRIMARY KEY REFERENCES kanban_cards(id),
            review_round        INTEGER NOT NULL DEFAULT 0,
            state               TEXT NOT NULL DEFAULT 'idle',
            pending_dispatch_id TEXT,
            last_verdict        TEXT,
            last_decision       TEXT,
            decided_by          TEXT,
            decided_at          TEXT,
            approach_change_round INTEGER,
            session_reset_round INTEGER,
            review_entered_at   TEXT,
            updated_at          TEXT DEFAULT (datetime('now'))
        );",
    )?;

    // Backfill card_review_state from existing kanban_cards data
    let backfilled: usize = conn
        .execute(
            "INSERT OR IGNORE INTO card_review_state (card_id, review_round, state, review_entered_at, updated_at)
             SELECT id, COALESCE(review_round, 0),
               CASE
                 WHEN status = 'done' THEN 'idle'
                 WHEN review_status = 'reviewing' THEN 'reviewing'
                 WHEN review_status = 'suggestion_pending' THEN 'suggestion_pending'
                 WHEN review_status = 'rework_pending' THEN 'rework_pending'
                 WHEN review_status = 'awaiting_dod' THEN 'awaiting_dod'
                 WHEN review_status = 'dilemma_pending' THEN 'dilemma_pending'
                 WHEN status = 'review' THEN 'reviewing'
                 ELSE 'idle'
               END,
               review_entered_at,
               datetime('now')
             FROM kanban_cards
             WHERE status NOT IN ('backlog', 'ready')",
            [],
        )
        .unwrap_or(0);
    if backfilled > 0 {
        tracing::info!("Backfilled {backfilled} card_review_state records (#117)");
    }

    // Populate pending_dispatch_id from active review-decision dispatches
    let _ = conn.execute(
        "UPDATE card_review_state SET pending_dispatch_id = (
             SELECT td.id FROM task_dispatches td
             WHERE td.kanban_card_id = card_review_state.card_id
             AND td.dispatch_type = 'review-decision'
             AND td.status IN ('pending', 'dispatched')
             ORDER BY td.rowid DESC LIMIT 1
         )
         WHERE card_id IN (
             SELECT kanban_card_id FROM task_dispatches
             WHERE dispatch_type = 'review-decision' AND status IN ('pending', 'dispatched')
         )",
        [],
    );

    // #118: Track approach-change round for repeated-finding detection
    let _ = conn.execute(
        "ALTER TABLE card_review_state ADD COLUMN approach_change_round INTEGER",
        [],
    );
    let _ = conn.execute(
        "ALTER TABLE card_review_state ADD COLUMN session_reset_round INTEGER",
        [],
    );

    // Rate limit cache table (provider → cached rate-limit JSON)
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS rate_limit_cache (
            provider   TEXT PRIMARY KEY,
            data       TEXT,
            fetched_at INTEGER
        );",
    )?;

    // Deferred hooks queue — persistent queue for hooks skipped when engine is busy (#125)
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS deferred_hooks (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            hook_name  TEXT NOT NULL,
            payload    TEXT NOT NULL DEFAULT '{}',
            status     TEXT NOT NULL DEFAULT 'pending',
            created_at DATETIME DEFAULT (datetime('now'))
        );",
    )?;
    // Add status column if upgrading from pre-status schema
    {
        let has_status: bool = conn
            .prepare("SELECT status FROM deferred_hooks LIMIT 0")
            .is_ok();
        if !has_status {
            conn.execute_batch(
                "ALTER TABLE deferred_hooks ADD COLUMN status TEXT NOT NULL DEFAULT 'pending';",
            )?;
        }
    }
    // Reset any 'processing' hooks from a previous crash back to 'pending'
    conn.execute_batch(
        "UPDATE deferred_hooks SET status = 'pending' WHERE status = 'processing';",
    )?;

    // Message outbox — async delivery queue to avoid self-referential HTTP deadlock (#120)
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS message_outbox (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            target     TEXT NOT NULL,
            content    TEXT NOT NULL,
            bot        TEXT NOT NULL DEFAULT 'announce',
            source     TEXT NOT NULL DEFAULT 'system',
            status     TEXT NOT NULL DEFAULT 'pending',
            created_at DATETIME DEFAULT (datetime('now')),
            sent_at    DATETIME,
            error      TEXT
        );",
    )?;

    // #144: Dispatch notification outbox — durable queue for Discord side-effects.
    // Replaces tokio::spawn fire-and-forget calls with a persistent outbox pattern.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS dispatch_outbox (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            dispatch_id  TEXT NOT NULL,
            action       TEXT NOT NULL,
            agent_id     TEXT,
            card_id      TEXT,
            title        TEXT,
            status       TEXT NOT NULL DEFAULT 'pending',
            created_at   DATETIME DEFAULT (datetime('now')),
            processed_at DATETIME,
            error        TEXT
        );",
    )?;

    // Kanban audit logs — transition history for cards (#155)
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS kanban_audit_logs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            card_id TEXT,
            from_status TEXT,
            to_status TEXT,
            source TEXT,
            result TEXT,
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP
        );",
    )?;

    // Audit logs table for analytics dashboard
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS audit_logs (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            entity_type TEXT,
            entity_id   TEXT,
            action      TEXT,
            timestamp   DATETIME DEFAULT CURRENT_TIMESTAMP,
            actor       TEXT
        );",
    )?;

    // #119: Review tuning outcomes — tracks verdict→decision classification
    // for aggregating false positive/negative rates to auto-tune review prompts.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS review_tuning_outcomes (
            id                 INTEGER PRIMARY KEY AUTOINCREMENT,
            card_id            TEXT,
            dispatch_id        TEXT,
            review_round       INTEGER,
            verdict            TEXT NOT NULL,
            decision           TEXT,
            outcome            TEXT NOT NULL,
            finding_categories TEXT,
            created_at         DATETIME DEFAULT (datetime('now'))
        );",
    )?;

    // #126: Add expires_at column to kv_meta for TTL support
    {
        let has_expires: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM pragma_table_info('kv_meta') WHERE name = 'expires_at'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(false);
        if !has_expires {
            conn.execute_batch("ALTER TABLE kv_meta ADD COLUMN expires_at TEXT;")?;
        }
    }

    // #174: Add retry_count column to dispatch_outbox for retry tracking
    {
        let has_retry: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM pragma_table_info('dispatch_outbox') WHERE name = 'retry_count'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(false);
        if !has_retry {
            conn.execute_batch(
                "ALTER TABLE dispatch_outbox ADD COLUMN retry_count INTEGER NOT NULL DEFAULT 0;",
            )?;
        }
    }

    // #209: Add next_attempt_at column to dispatch_outbox for retry backoff scheduling
    {
        let has_next_attempt: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM pragma_table_info('dispatch_outbox') WHERE name = 'next_attempt_at'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(false);
        if !has_next_attempt {
            conn.execute_batch("ALTER TABLE dispatch_outbox ADD COLUMN next_attempt_at DATETIME;")?;
        }
    }

    // #212: Session termination audit trail
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS session_termination_events (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            session_key     TEXT NOT NULL,
            dispatch_id     TEXT,
            killer_component TEXT NOT NULL,
            reason_code     TEXT NOT NULL,
            reason_text     TEXT,
            probe_snapshot  TEXT,
            last_offset     INTEGER,
            tmux_alive      INTEGER,
            created_at      DATETIME DEFAULT (datetime('now'))
        );
        CREATE INDEX IF NOT EXISTS idx_ste_session_key ON session_termination_events(session_key);
        CREATE INDEX IF NOT EXISTS idx_ste_dispatch_id ON session_termination_events(dispatch_id);
        CREATE INDEX IF NOT EXISTS idx_ste_created_at ON session_termination_events(created_at);",
    )?;

    // #398: Runtime supervisor decision audit
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS runtime_decisions (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            signal        TEXT NOT NULL,
            evidence_json TEXT NOT NULL,
            chosen_action TEXT NOT NULL,
            actor         TEXT NOT NULL,
            session_key   TEXT,
            dispatch_id   TEXT,
            created_at    DATETIME DEFAULT (datetime('now'))
        );
        CREATE INDEX IF NOT EXISTS idx_runtime_decisions_signal
            ON runtime_decisions(signal);
        CREATE INDEX IF NOT EXISTS idx_runtime_decisions_dispatch_id
            ON runtime_decisions(dispatch_id);
        CREATE INDEX IF NOT EXISTS idx_runtime_decisions_created_at
            ON runtime_decisions(created_at);",
    )?;

    // #189: Generic DM reply tracking — replaces family profile probe hardcode.
    // Agents register pending DM replies; router matches incoming DMs to pending entries.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS pending_dm_replies (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            source_agent TEXT NOT NULL,
            user_id      TEXT NOT NULL,
            channel_id   TEXT,
            context      TEXT NOT NULL DEFAULT '{}',
            status       TEXT NOT NULL DEFAULT 'pending',
            created_at   DATETIME DEFAULT (datetime('now')),
            consumed_at  DATETIME,
            expires_at   DATETIME
        );
        CREATE INDEX IF NOT EXISTS idx_pdr_user_status ON pending_dm_replies(user_id, status);",
    )?;

    seed_builtin_pipeline_stages(conn)?;

    Ok(())
}

pub fn seed_builtin_pipeline_stages(conn: &Connection) -> Result<()> {
    let repo_exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM github_repos WHERE id = ?1)",
            [AGENTDESK_REPO_ID],
            |row| row.get(0),
        )
        .unwrap_or(false);
    if !repo_exists {
        return Ok(());
    }

    ensure_pipeline_stage(
        conn,
        AGENTDESK_REPO_ID,
        "dev-deploy",
        100,
        Some("review_pass"),
        Some("self"),
        Some("no_rs_changes"),
    )?;
    ensure_pipeline_stage(
        conn,
        AGENTDESK_REPO_ID,
        "e2e-test",
        200,
        None,
        Some("counter"),
        Some("no_rs_changes"),
    )?;

    Ok(())
}

fn ensure_pipeline_stage(
    conn: &Connection,
    repo_id: &str,
    stage_name: &str,
    stage_order: i64,
    trigger_after: Option<&str>,
    provider: Option<&str>,
    skip_condition: Option<&str>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO pipeline_stages (
            repo_id, stage_name, stage_order, trigger_after, provider, skip_condition
         )
         SELECT ?1, ?2, ?3, ?4, ?5, ?6
         WHERE NOT EXISTS (
            SELECT 1 FROM pipeline_stages WHERE repo_id = ?1 AND stage_name = ?2
         )",
        rusqlite::params![
            repo_id,
            stage_name,
            stage_order,
            trigger_after,
            provider,
            skip_condition,
        ],
    )?;
    Ok(())
}

pub(crate) fn ensure_auto_queue_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS auto_queue_runs (
            id          TEXT PRIMARY KEY,
            repo        TEXT,
            agent_id    TEXT,
            status      TEXT DEFAULT 'active',
            ai_model    TEXT,
            ai_rationale TEXT,
            timeout_minutes INTEGER DEFAULT 120,
            unified_thread  INTEGER DEFAULT 0,
            unified_thread_id TEXT,
            unified_thread_channel_id TEXT,
            max_concurrent_threads INTEGER DEFAULT 1,
            thread_group_count INTEGER DEFAULT 1,
            created_at  DATETIME DEFAULT CURRENT_TIMESTAMP,
            completed_at DATETIME
        );
        CREATE TABLE IF NOT EXISTS auto_queue_entries (
            id              TEXT PRIMARY KEY,
            run_id          TEXT REFERENCES auto_queue_runs(id),
            kanban_card_id  TEXT REFERENCES kanban_cards(id),
            agent_id        TEXT,
            priority_rank   INTEGER DEFAULT 0,
            reason          TEXT,
            status          TEXT DEFAULT 'pending',
            dispatch_id     TEXT,
            slot_index      INTEGER,
            thread_group    INTEGER DEFAULT 0,
            batch_phase     INTEGER DEFAULT 0,
            created_at      DATETIME DEFAULT CURRENT_TIMESTAMP,
            dispatched_at   DATETIME,
            completed_at    DATETIME
        );
        CREATE TABLE IF NOT EXISTS auto_queue_slots (
            agent_id             TEXT NOT NULL,
            slot_index           INTEGER NOT NULL,
            assigned_run_id      TEXT,
            assigned_thread_group INTEGER,
            thread_id_map        TEXT,
            created_at           DATETIME DEFAULT CURRENT_TIMESTAMP,
            updated_at           DATETIME DEFAULT CURRENT_TIMESTAMP,
            PRIMARY KEY (agent_id, slot_index)
        );",
    )?;

    ensure_auto_queue_column(
        conn,
        "auto_queue_runs",
        "unified_thread",
        "ALTER TABLE auto_queue_runs ADD COLUMN unified_thread INTEGER DEFAULT 0;",
    )?;
    ensure_auto_queue_column(
        conn,
        "auto_queue_runs",
        "unified_thread_id",
        "ALTER TABLE auto_queue_runs ADD COLUMN unified_thread_id TEXT;",
    )?;
    ensure_auto_queue_column(
        conn,
        "auto_queue_runs",
        "unified_thread_channel_id",
        "ALTER TABLE auto_queue_runs ADD COLUMN unified_thread_channel_id TEXT;",
    )?;
    ensure_auto_queue_column(
        conn,
        "auto_queue_entries",
        "thread_group",
        "ALTER TABLE auto_queue_entries ADD COLUMN thread_group INTEGER DEFAULT 0;",
    )?;
    ensure_auto_queue_column(
        conn,
        "auto_queue_entries",
        "batch_phase",
        "ALTER TABLE auto_queue_entries ADD COLUMN batch_phase INTEGER DEFAULT 0;",
    )?;
    ensure_auto_queue_column(
        conn,
        "auto_queue_runs",
        "max_concurrent_threads",
        "ALTER TABLE auto_queue_runs ADD COLUMN max_concurrent_threads INTEGER DEFAULT 1;",
    )?;
    ensure_auto_queue_column(
        conn,
        "auto_queue_runs",
        "thread_group_count",
        "ALTER TABLE auto_queue_runs ADD COLUMN thread_group_count INTEGER DEFAULT 1;",
    )?;
    ensure_auto_queue_column(
        conn,
        "auto_queue_entries",
        "dispatch_id",
        "ALTER TABLE auto_queue_entries ADD COLUMN dispatch_id TEXT;",
    )?;
    backfill_auto_queue_dispatch_ids(conn)?;
    ensure_auto_queue_column(
        conn,
        "auto_queue_entries",
        "slot_index",
        "ALTER TABLE auto_queue_entries ADD COLUMN slot_index INTEGER;",
    )?;

    if auto_queue_has_column(conn, "auto_queue_runs", "max_concurrent_per_agent") {
        let _ =
            conn.execute_batch("ALTER TABLE auto_queue_runs DROP COLUMN max_concurrent_per_agent;");
    }

    Ok(())
}

fn ensure_auto_queue_column(conn: &Connection, table: &str, column: &str, ddl: &str) -> Result<()> {
    if !auto_queue_has_column(conn, table, column) {
        conn.execute_batch(ddl)?;
    }
    Ok(())
}

fn auto_queue_has_column(conn: &Connection, table: &str, column: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) > 0 FROM pragma_table_info(?1) WHERE name = ?2",
        rusqlite::params![table, column],
        |row| row.get(0),
    )
    .unwrap_or(false)
}

fn backfill_auto_queue_dispatch_ids(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "UPDATE auto_queue_entries SET dispatch_id = (
            SELECT td.id FROM task_dispatches td
            WHERE td.kanban_card_id = auto_queue_entries.kanban_card_id
              AND td.to_agent_id = auto_queue_entries.agent_id
              AND td.dispatch_type = 'implementation'
            ORDER BY td.created_at DESC LIMIT 1
        )
        WHERE auto_queue_entries.status IN ('dispatched', 'done')
          AND auto_queue_entries.dispatch_id IS NULL
          AND auto_queue_entries.rowid = (
              SELECT e.rowid FROM auto_queue_entries e
              WHERE e.kanban_card_id = auto_queue_entries.kanban_card_id
                AND e.agent_id = auto_queue_entries.agent_id
                AND e.status IN ('dispatched', 'done')
              ORDER BY e.created_at DESC LIMIT 1
          );",
    )?;
    Ok(())
}

fn ensure_session_transcripts_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS session_transcripts (
            id                INTEGER PRIMARY KEY AUTOINCREMENT,
            turn_id           TEXT NOT NULL UNIQUE,
            session_key       TEXT,
            channel_id        TEXT,
            agent_id          TEXT,
            provider          TEXT,
            dispatch_id       TEXT,
            user_message      TEXT NOT NULL DEFAULT '',
            assistant_message TEXT NOT NULL DEFAULT '',
            created_at        DATETIME DEFAULT CURRENT_TIMESTAMP
        );
        CREATE INDEX IF NOT EXISTS idx_session_transcripts_session_key
            ON session_transcripts (session_key);
        CREATE INDEX IF NOT EXISTS idx_session_transcripts_agent_id
            ON session_transcripts (agent_id);
        CREATE INDEX IF NOT EXISTS idx_session_transcripts_created_at
            ON session_transcripts (created_at DESC);
        CREATE VIRTUAL TABLE IF NOT EXISTS session_transcripts_fts USING fts5(
            session_transcript_id UNINDEXED,
            content,
            tokenize = 'unicode61'
        );",
    )?;
    migrate_legacy_session_transcripts_agent_fk(conn)?;
    Ok(())
}

fn migrate_legacy_session_transcripts_agent_fk(conn: &Connection) -> Result<()> {
    let table_sql: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master
             WHERE type = 'table' AND name = 'session_transcripts'",
            [],
            |row| row.get(0),
        )
        .ok();

    let needs_rebuild = table_sql
        .as_deref()
        .map(|sql| sql.contains("REFERENCES agents"))
        .unwrap_or(false);
    if !needs_rebuild {
        return Ok(());
    }

    conn.execute_batch(
        "DROP TABLE IF EXISTS session_transcripts_fts;
         ALTER TABLE session_transcripts RENAME TO session_transcripts_legacy;
         CREATE TABLE session_transcripts (
            id                INTEGER PRIMARY KEY AUTOINCREMENT,
            turn_id           TEXT NOT NULL UNIQUE,
            session_key       TEXT,
            channel_id        TEXT,
            agent_id          TEXT,
            provider          TEXT,
            dispatch_id       TEXT,
            user_message      TEXT NOT NULL DEFAULT '',
            assistant_message TEXT NOT NULL DEFAULT '',
            created_at        DATETIME DEFAULT CURRENT_TIMESTAMP
         );
         INSERT INTO session_transcripts (
            id,
            turn_id,
            session_key,
            channel_id,
            agent_id,
            provider,
            dispatch_id,
            user_message,
            assistant_message,
            created_at
         )
         SELECT
            id,
            turn_id,
            session_key,
            channel_id,
            agent_id,
            provider,
            dispatch_id,
            user_message,
            assistant_message,
            created_at
         FROM session_transcripts_legacy;
         DROP TABLE session_transcripts_legacy;
         CREATE INDEX IF NOT EXISTS idx_session_transcripts_session_key
            ON session_transcripts (session_key);
         CREATE INDEX IF NOT EXISTS idx_session_transcripts_agent_id
            ON session_transcripts (agent_id);
         CREATE INDEX IF NOT EXISTS idx_session_transcripts_created_at
            ON session_transcripts (created_at DESC);
         CREATE VIRTUAL TABLE session_transcripts_fts USING fts5(
            session_transcript_id UNINDEXED,
            content,
            tokenize = 'unicode61'
         );
         INSERT INTO session_transcripts_fts (session_transcript_id, content)
         SELECT
            id,
            CASE
                WHEN TRIM(user_message) <> '' AND TRIM(assistant_message) <> ''
                    THEN 'user:' || char(10) || user_message || char(10) || char(10)
                        || 'assistant:' || char(10) || assistant_message
                WHEN TRIM(user_message) <> ''
                    THEN 'user:' || char(10) || user_message
                WHEN TRIM(assistant_message) <> ''
                    THEN 'assistant:' || char(10) || assistant_message
                ELSE ''
            END
         FROM session_transcripts;",
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn builtin_stage_rows(
        conn: &Connection,
    ) -> Vec<(String, i64, Option<String>, Option<String>, Option<String>)> {
        conn.prepare(
            "SELECT stage_name, stage_order, trigger_after, provider, skip_condition
             FROM pipeline_stages
             WHERE repo_id = ?1
             ORDER BY stage_order ASC",
        )
        .unwrap()
        .query_map([AGENTDESK_REPO_ID], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap()
    }

    #[test]
    fn migrate_seeds_builtin_agentdesk_pipeline_stages_for_existing_repo() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE kv_meta (
                key   TEXT PRIMARY KEY,
                value TEXT
            );",
        )
        .unwrap();
        conn.execute_batch(include_str!("../../migrations/001_initial.sql"))
            .unwrap();
        conn.execute(
            "INSERT INTO kv_meta (key, value) VALUES ('schema_version', '1')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO github_repos (id, display_name, sync_enabled) VALUES (?1, 'AgentDesk', TRUE)",
            [AGENTDESK_REPO_ID],
        )
        .unwrap();

        migrate(&conn).unwrap();

        assert_eq!(
            builtin_stage_rows(&conn),
            vec![
                (
                    "dev-deploy".to_string(),
                    100,
                    Some("review_pass".to_string()),
                    Some("self".to_string()),
                    Some("no_rs_changes".to_string()),
                ),
                (
                    "e2e-test".to_string(),
                    200,
                    None,
                    Some("counter".to_string()),
                    Some("no_rs_changes".to_string()),
                ),
            ]
        );
    }

    #[test]
    fn seed_builtin_pipeline_stages_is_idempotent_for_agentdesk_repo() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute(
            "INSERT INTO github_repos (id, display_name, sync_enabled) VALUES (?1, 'AgentDesk', TRUE)",
            [AGENTDESK_REPO_ID],
        )
        .unwrap();

        seed_builtin_pipeline_stages(&conn).unwrap();
        seed_builtin_pipeline_stages(&conn).unwrap();

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pipeline_stages WHERE repo_id = ?1",
                [AGENTDESK_REPO_ID],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 2);
        assert_eq!(
            builtin_stage_rows(&conn),
            vec![
                (
                    "dev-deploy".to_string(),
                    100,
                    Some("review_pass".to_string()),
                    Some("self".to_string()),
                    Some("no_rs_changes".to_string()),
                ),
                (
                    "e2e-test".to_string(),
                    200,
                    None,
                    Some("counter".to_string()),
                    Some("no_rs_changes".to_string()),
                ),
            ]
        );
    }

    #[test]
    fn migrate_rebuilds_session_transcripts_without_agent_fk() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        conn.execute_batch(
            "CREATE TABLE kv_meta (
                key   TEXT PRIMARY KEY,
                value TEXT
            );",
        )
        .unwrap();
        conn.execute_batch(include_str!("../../migrations/001_initial.sql"))
            .unwrap();
        conn.execute(
            "INSERT INTO kv_meta (key, value) VALUES ('schema_version', '1')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO agents (id, name) VALUES ('legacy-agent', 'Legacy Agent')",
            [],
        )
        .unwrap();
        conn.execute_batch(
            "CREATE TABLE session_transcripts (
                id                INTEGER PRIMARY KEY AUTOINCREMENT,
                turn_id           TEXT NOT NULL UNIQUE,
                session_key       TEXT,
                channel_id        TEXT,
                agent_id          TEXT REFERENCES agents(id),
                provider          TEXT,
                dispatch_id       TEXT,
                user_message      TEXT NOT NULL DEFAULT '',
                assistant_message TEXT NOT NULL DEFAULT '',
                created_at        DATETIME DEFAULT CURRENT_TIMESTAMP
            );
            INSERT INTO session_transcripts (
                turn_id,
                session_key,
                channel_id,
                agent_id,
                provider,
                dispatch_id,
                user_message,
                assistant_message
            ) VALUES (
                'discord:legacy:1',
                'host:legacy',
                'legacy-channel',
                'legacy-agent',
                'codex',
                'dispatch-legacy',
                'legacy question',
                'legacy answer'
            );",
        )
        .unwrap();

        migrate(&conn).unwrap();

        let table_sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master
                 WHERE type = 'table' AND name = 'session_transcripts'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!table_sql.contains("REFERENCES agents"));

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM session_transcripts", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 1);

        let fts_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM session_transcripts_fts", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(fts_count, 1);
    }
}
