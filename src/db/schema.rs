use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params_from_iter};
use std::collections::HashSet;

const AGENTDESK_REPO_ID: &str = "itismyfield/AgentDesk";
const SESSION_AGENT_ID_BACKFILL_META_KEY: &str = "session_agent_id_backfill:v1";
const SESSION_TRANSCRIPT_AGENT_ID_BACKFILL_META_KEY: &str =
    "session_transcript_agent_id_backfill:v1";
const AUTO_QUEUE_PHASE_GATE_BACKFILL_META_KEY: &str = "auto_queue_phase_gate_backfill:v1";
const EXISTING_ID_LOOKUP_CHUNK_SIZE: usize = 500;
const PENDING_BLOCKED_STATUS_BACKFILL_META_KEY: &str = "pending_blocked_status_backfill:v1";

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
    let _ = conn.execute_batch("ALTER TABLE task_dispatches ADD COLUMN context TEXT;");
    let _ = conn.execute_batch("ALTER TABLE task_dispatches ADD COLUMN thread_id TEXT;");
    let _ =
        conn.execute_batch("ALTER TABLE task_dispatches ADD COLUMN retry_count INTEGER DEFAULT 0;");
    let _ = conn.execute_batch("ALTER TABLE task_dispatches ADD COLUMN completed_at DATETIME;");
    let _ = conn.execute_batch("ALTER TABLE meetings ADD COLUMN thread_id TEXT;");
    let _ = conn.execute_batch("ALTER TABLE meetings ADD COLUMN primary_provider TEXT;");
    let _ = conn.execute_batch("ALTER TABLE meetings ADD COLUMN reviewer_provider TEXT;");
    let _ = conn.execute_batch("ALTER TABLE meetings ADD COLUMN participant_names TEXT;");
    let _ = conn.execute_batch("ALTER TABLE meetings ADD COLUMN selection_reason TEXT;");
    let _ = conn.execute_batch("ALTER TABLE meetings ADD COLUMN created_at INTEGER;");
    let _ = conn.execute_batch("ALTER TABLE sessions ADD COLUMN thread_channel_id TEXT;");
    let _ = conn.execute_batch("ALTER TABLE sessions ADD COLUMN claude_session_id TEXT;");
    ensure_session_transcripts_schema(conn)?;
    ensure_turns_schema(conn)?;
    run_migration_once(
        conn,
        SESSION_AGENT_ID_BACKFILL_META_KEY,
        backfill_session_agent_ids,
    )?;
    run_migration_once(
        conn,
        SESSION_TRANSCRIPT_AGENT_ID_BACKFILL_META_KEY,
        backfill_session_transcript_agent_ids,
    )?;
    ensure_memento_feedback_stats_schema(conn)?;

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
    run_migration_once(
        conn,
        AUTO_QUEUE_PHASE_GATE_BACKFILL_META_KEY,
        backfill_auto_queue_phase_gates,
    )?;
    ensure_api_friction_schema(conn)?;

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
    run_migration_once(
        conn,
        PENDING_BLOCKED_STATUS_BACKFILL_META_KEY,
        backfill_pending_blocked_statuses,
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

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS card_retrospectives (
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
            success          INTEGER NOT NULL DEFAULT 0,
            result_json      TEXT NOT NULL,
            memory_payload   TEXT NOT NULL,
            sync_backend     TEXT,
            sync_status      TEXT NOT NULL DEFAULT 'skipped',
            sync_error       TEXT,
            created_at       DATETIME DEFAULT (datetime('now')),
            updated_at       DATETIME DEFAULT (datetime('now')),
            UNIQUE(card_id, dispatch_id, terminal_status)
        );
        CREATE INDEX IF NOT EXISTS idx_card_retrospectives_card_created
            ON card_retrospectives(card_id, created_at DESC);
        CREATE INDEX IF NOT EXISTS idx_card_retrospectives_issue_created
            ON card_retrospectives(issue_number, created_at DESC)
            WHERE issue_number IS NOT NULL;",
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

    // #436: Dispatch status transition audit trail
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS dispatch_events (
            id                INTEGER PRIMARY KEY AUTOINCREMENT,
            dispatch_id       TEXT NOT NULL REFERENCES task_dispatches(id) ON DELETE CASCADE,
            kanban_card_id    TEXT REFERENCES kanban_cards(id) ON DELETE SET NULL,
            dispatch_type     TEXT,
            from_status       TEXT,
            to_status         TEXT NOT NULL,
            transition_source TEXT NOT NULL,
            payload_json      TEXT,
            created_at        DATETIME DEFAULT (datetime('now'))
        );
        CREATE INDEX IF NOT EXISTS idx_dispatch_events_dispatch_id
            ON dispatch_events(dispatch_id);
        CREATE INDEX IF NOT EXISTS idx_dispatch_events_card_id
            ON dispatch_events(kanban_card_id);
        CREATE INDEX IF NOT EXISTS idx_dispatch_events_created_at
            ON dispatch_events(created_at);",
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
        );
        CREATE TABLE IF NOT EXISTS auto_queue_entry_transitions (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            entry_id        TEXT NOT NULL,
            from_status     TEXT,
            to_status       TEXT NOT NULL,
            trigger_source  TEXT NOT NULL,
            created_at      DATETIME DEFAULT CURRENT_TIMESTAMP
        );
        CREATE TABLE IF NOT EXISTS auto_queue_entry_dispatch_history (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            entry_id        TEXT NOT NULL REFERENCES auto_queue_entries(id) ON DELETE CASCADE,
            dispatch_id     TEXT NOT NULL REFERENCES task_dispatches(id) ON DELETE CASCADE,
            trigger_source  TEXT,
            created_at      DATETIME DEFAULT CURRENT_TIMESTAMP,
            UNIQUE(entry_id, dispatch_id)
        );
        CREATE TABLE IF NOT EXISTS auto_queue_phase_gates (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            run_id          TEXT NOT NULL REFERENCES auto_queue_runs(id) ON DELETE CASCADE,
            phase           INTEGER NOT NULL,
            status          TEXT NOT NULL DEFAULT 'pending',
            verdict         TEXT,
            dispatch_id     TEXT REFERENCES task_dispatches(id) ON DELETE CASCADE
                                CHECK(dispatch_id IS NULL OR TRIM(dispatch_id) <> ''),
            pass_verdict    TEXT NOT NULL DEFAULT 'phase_gate_passed',
            next_phase      INTEGER,
            final_phase     INTEGER NOT NULL DEFAULT 0,
            anchor_card_id  TEXT REFERENCES kanban_cards(id) ON DELETE SET NULL,
            failure_reason  TEXT,
            created_at      DATETIME DEFAULT CURRENT_TIMESTAMP,
            updated_at      DATETIME DEFAULT CURRENT_TIMESTAMP
        );
        CREATE INDEX IF NOT EXISTS idx_aq_entry_transitions_entry
            ON auto_queue_entry_transitions(entry_id);
        CREATE INDEX IF NOT EXISTS idx_aq_entry_transitions_created
            ON auto_queue_entry_transitions(created_at);
        CREATE INDEX IF NOT EXISTS idx_aq_entry_dispatch_history_entry
            ON auto_queue_entry_dispatch_history(entry_id);
        CREATE INDEX IF NOT EXISTS idx_aq_entry_dispatch_history_dispatch
            ON auto_queue_entry_dispatch_history(dispatch_id);
        CREATE INDEX IF NOT EXISTS idx_aq_entry_dispatch_history_created
            ON auto_queue_entry_dispatch_history(created_at);",
    )?;
    ensure_auto_queue_phase_gate_table_shape(conn)?;

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
    ensure_auto_queue_column(
        conn,
        "task_dispatches",
        "context",
        "ALTER TABLE task_dispatches ADD COLUMN context TEXT;",
    )?;
    backfill_auto_queue_dispatch_ids(conn)?;
    backfill_auto_queue_dispatch_history(conn)?;
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

fn ensure_api_friction_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS api_friction_events (
            id                  TEXT PRIMARY KEY,
            fingerprint         TEXT NOT NULL,
            endpoint            TEXT NOT NULL,
            friction_type       TEXT NOT NULL,
            summary             TEXT NOT NULL,
            workaround          TEXT,
            suggested_fix       TEXT,
            docs_category       TEXT,
            keywords_json       TEXT NOT NULL DEFAULT '[]',
            payload_json        TEXT NOT NULL,
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
            created_at          DATETIME DEFAULT CURRENT_TIMESTAMP
        );
        CREATE INDEX IF NOT EXISTS idx_api_friction_events_fingerprint
            ON api_friction_events (fingerprint);
        CREATE INDEX IF NOT EXISTS idx_api_friction_events_dispatch_id
            ON api_friction_events (dispatch_id);
        CREATE INDEX IF NOT EXISTS idx_api_friction_events_created_at
            ON api_friction_events (created_at DESC);
        CREATE TABLE IF NOT EXISTS api_friction_issues (
            fingerprint   TEXT PRIMARY KEY,
            repo_id       TEXT NOT NULL,
            endpoint      TEXT NOT NULL,
            friction_type TEXT NOT NULL,
            title         TEXT NOT NULL,
            body          TEXT NOT NULL,
            issue_number  INTEGER,
            issue_url     TEXT,
            event_count   INTEGER NOT NULL DEFAULT 0,
            first_event_at DATETIME,
            last_event_at DATETIME,
            last_error    TEXT,
            created_at    DATETIME DEFAULT CURRENT_TIMESTAMP,
            updated_at    DATETIME DEFAULT CURRENT_TIMESTAMP
        );
        CREATE INDEX IF NOT EXISTS idx_api_friction_issues_repo
            ON api_friction_issues (repo_id, updated_at DESC);",
    )?;
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

fn auto_queue_index_exists(conn: &Connection, index_name: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type = 'index' AND name = ?1",
        [index_name],
        |row| row.get(0),
    )
    .unwrap_or(false)
}

#[derive(Debug)]
struct LegacyAutoQueuePhaseGateRow {
    rowid: i64,
    run_id: String,
    phase: i64,
    status: String,
    verdict: Option<String>,
    dispatch_id: Option<String>,
    pass_verdict: String,
    next_phase: Option<i64>,
    final_phase: i64,
    anchor_card_id: Option<String>,
    failure_reason: Option<String>,
    created_at: Option<String>,
    updated_at: Option<String>,
}

fn ensure_auto_queue_phase_gate_table_shape(conn: &Connection) -> Result<()> {
    let needs_rebuild = !auto_queue_has_column(conn, "auto_queue_phase_gates", "id");
    if needs_rebuild {
        rebuild_auto_queue_phase_gates_table(conn)?;
    } else {
        ensure_auto_queue_phase_gate_indexes(conn)?;
    }
    Ok(())
}

fn ensure_auto_queue_phase_gate_indexes(conn: &Connection) -> Result<()> {
    if auto_queue_index_exists(conn, "uq_aq_phase_gates_run_phase_dispatch_key")
        && auto_queue_index_exists(conn, "uq_aq_phase_gates_dispatch_id")
        && auto_queue_index_exists(conn, "idx_aq_phase_gates_run_phase")
        && auto_queue_index_exists(conn, "idx_aq_phase_gates_run_status")
        && auto_queue_index_exists(conn, "idx_aq_phase_gates_phase_dispatch")
    {
        return Ok(());
    }

    conn.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS uq_aq_phase_gates_run_phase_dispatch_key
            ON auto_queue_phase_gates(run_id, phase, COALESCE(dispatch_id, ''));
         CREATE UNIQUE INDEX IF NOT EXISTS uq_aq_phase_gates_dispatch_id
            ON auto_queue_phase_gates(dispatch_id);
         CREATE INDEX IF NOT EXISTS idx_aq_phase_gates_run_phase
            ON auto_queue_phase_gates(run_id, phase);
         CREATE INDEX IF NOT EXISTS idx_aq_phase_gates_run_status
            ON auto_queue_phase_gates(run_id, status);
         CREATE INDEX IF NOT EXISTS idx_aq_phase_gates_phase_dispatch
            ON auto_queue_phase_gates(phase, dispatch_id);",
    )?;
    Ok(())
}

fn rebuild_auto_queue_phase_gates_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "DROP INDEX IF EXISTS idx_aq_phase_gates_run_phase;
         DROP INDEX IF EXISTS idx_aq_phase_gates_run_status;
         DROP INDEX IF EXISTS idx_aq_phase_gates_phase_dispatch;
         DROP INDEX IF EXISTS uq_aq_phase_gates_run_phase_dispatch_key;
         DROP INDEX IF EXISTS uq_aq_phase_gates_dispatch_id;
         ALTER TABLE auto_queue_phase_gates RENAME TO auto_queue_phase_gates_legacy;
         CREATE TABLE auto_queue_phase_gates (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            run_id          TEXT NOT NULL REFERENCES auto_queue_runs(id) ON DELETE CASCADE,
            phase           INTEGER NOT NULL,
            status          TEXT NOT NULL DEFAULT 'pending',
            verdict         TEXT,
            dispatch_id     TEXT REFERENCES task_dispatches(id) ON DELETE CASCADE
                                CHECK(dispatch_id IS NULL OR TRIM(dispatch_id) <> ''),
            pass_verdict    TEXT NOT NULL DEFAULT 'phase_gate_passed',
            next_phase      INTEGER,
            final_phase     INTEGER NOT NULL DEFAULT 0,
            anchor_card_id  TEXT REFERENCES kanban_cards(id) ON DELETE SET NULL,
            failure_reason  TEXT,
            created_at      DATETIME DEFAULT CURRENT_TIMESTAMP,
            updated_at      DATETIME DEFAULT CURRENT_TIMESTAMP
         );",
    )?;

    let mut rows = conn
        .prepare(
            "SELECT
                rowid,
                run_id,
                phase,
                status,
                verdict,
                dispatch_id,
                COALESCE(NULLIF(TRIM(pass_verdict), ''), 'phase_gate_passed') AS pass_verdict,
                next_phase,
                CASE
                    WHEN LOWER(TRIM(CAST(COALESCE(final_phase, 0) AS TEXT))) IN ('1', 'true')
                        THEN 1
                    ELSE 0
                END AS final_phase,
                anchor_card_id,
                failure_reason,
                CAST(created_at AS TEXT),
                CAST(updated_at AS TEXT)
             FROM auto_queue_phase_gates_legacy",
        )?
        .query_map([], |row| {
            Ok(LegacyAutoQueuePhaseGateRow {
                rowid: row.get(0)?,
                run_id: row.get(1)?,
                phase: row.get(2)?,
                status: row.get(3)?,
                verdict: row.get(4)?,
                dispatch_id: row.get(5)?,
                pass_verdict: row.get(6)?,
                next_phase: row.get(7)?,
                final_phase: row.get(8)?,
                anchor_card_id: row.get(9)?,
                failure_reason: row.get(10)?,
                created_at: row.get(11)?,
                updated_at: row.get(12)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let valid_run_ids = load_existing_ids(
        conn,
        "auto_queue_runs",
        &rows
            .iter()
            .map(|row| row.run_id.clone())
            .collect::<Vec<_>>(),
    )?;
    let valid_dispatch_ids = load_existing_ids(
        conn,
        "task_dispatches",
        &rows
            .iter()
            .filter_map(|row| row.dispatch_id.clone())
            .collect::<Vec<_>>(),
    )?;
    let valid_anchor_card_ids = load_existing_ids(
        conn,
        "kanban_cards",
        &rows
            .iter()
            .filter_map(|row| row.anchor_card_id.clone())
            .collect::<Vec<_>>(),
    )?;

    let mut normalized_rows = Vec::new();
    for mut row in rows.drain(..) {
        row.run_id = row.run_id.trim().to_string();
        if row.run_id.is_empty() || !valid_run_ids.contains(row.run_id.as_str()) {
            continue;
        }

        row.status = if row.status.trim().is_empty() {
            "pending".to_string()
        } else {
            row.status.trim().to_string()
        };
        row.verdict = row
            .verdict
            .take()
            .and_then(|value| (!value.trim().is_empty()).then(|| value.trim().to_string()));
        row.dispatch_id = row.dispatch_id.take().and_then(|value| {
            let value = value.trim();
            if value.is_empty() || !valid_dispatch_ids.contains(value) {
                None
            } else {
                Some(value.to_string())
            }
        });
        row.anchor_card_id = row.anchor_card_id.take().and_then(|value| {
            let value = value.trim();
            if value.is_empty() || !valid_anchor_card_ids.contains(value) {
                None
            } else {
                Some(value.to_string())
            }
        });
        row.failure_reason = row
            .failure_reason
            .take()
            .and_then(|value| (!value.trim().is_empty()).then(|| value.trim().to_string()));
        row.created_at = row
            .created_at
            .take()
            .and_then(|value| (!value.trim().is_empty()).then(|| value.trim().to_string()));
        row.updated_at = row
            .updated_at
            .take()
            .and_then(|value| (!value.trim().is_empty()).then(|| value.trim().to_string()));
        row.final_phase = if row.final_phase != 0 { 1 } else { 0 };

        normalized_rows.push(row);
    }

    let groups_with_dispatch = normalized_rows
        .iter()
        .filter(|row| row.dispatch_id.is_some())
        .map(|row| (row.run_id.clone(), row.phase))
        .collect::<HashSet<_>>();

    normalized_rows.retain(|row| {
        row.dispatch_id.is_some()
            || !groups_with_dispatch.contains(&(row.run_id.clone(), row.phase))
    });

    normalized_rows.sort_by(|left, right| {
        phase_gate_status_priority(&left.status)
            .cmp(&phase_gate_status_priority(&right.status))
            .then_with(|| right.updated_at.cmp(&left.updated_at))
            .then_with(|| right.created_at.cmp(&left.created_at))
            .then_with(|| right.rowid.cmp(&left.rowid))
    });

    let mut seen_dispatch_ids = HashSet::new();
    let mut seen_group_dispatch_keys = HashSet::new();
    for row in normalized_rows {
        if let Some(dispatch_id) = row.dispatch_id.as_ref() {
            if !seen_dispatch_ids.insert(dispatch_id.clone()) {
                continue;
            }
        }

        let group_dispatch_key = (
            row.run_id.clone(),
            row.phase,
            row.dispatch_id.clone().unwrap_or_default(),
        );
        if !seen_group_dispatch_keys.insert(group_dispatch_key) {
            continue;
        }

        conn.execute(
            "INSERT INTO auto_queue_phase_gates (
                run_id,
                phase,
                status,
                verdict,
                dispatch_id,
                pass_verdict,
                next_phase,
                final_phase,
                anchor_card_id,
                failure_reason,
                created_at,
                updated_at
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
                COALESCE(?11, CURRENT_TIMESTAMP),
                COALESCE(?12, COALESCE(?11, CURRENT_TIMESTAMP))
            )",
            rusqlite::params![
                row.run_id,
                row.phase,
                row.status,
                row.verdict,
                row.dispatch_id,
                row.pass_verdict,
                row.next_phase,
                row.final_phase,
                row.anchor_card_id,
                row.failure_reason,
                row.created_at,
                row.updated_at,
            ],
        )?;
    }

    ensure_auto_queue_phase_gate_indexes(conn)?;
    conn.execute_batch("DROP TABLE auto_queue_phase_gates_legacy;")?;
    Ok(())
}

fn phase_gate_status_priority(status: &str) -> i32 {
    match status {
        "failed" => 0,
        "pending" => 1,
        "passed" => 2,
        _ => 3,
    }
}

fn load_existing_ids(conn: &Connection, table: &str, ids: &[String]) -> Result<HashSet<String>> {
    let mut normalized_ids = Vec::new();
    let mut seen = HashSet::new();
    for id in ids {
        let trimmed = id.trim();
        if trimmed.is_empty() || !seen.insert(trimmed.to_string()) {
            continue;
        }
        normalized_ids.push(trimmed.to_string());
    }

    if normalized_ids.is_empty() {
        return Ok(HashSet::new());
    }

    let mut existing = HashSet::new();
    // Legacy backfills can accumulate thousands of FK candidates, so keep each
    // IN(...) lookup below SQLite's bind-parameter limit during migrate().
    for chunk in normalized_ids.chunks(EXISTING_ID_LOOKUP_CHUNK_SIZE) {
        let placeholders = (0..chunk.len()).map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = match table {
            "auto_queue_runs" | "task_dispatches" | "kanban_cards" => {
                format!("SELECT id FROM {table} WHERE id IN ({placeholders})")
            }
            _ => unreachable!("unexpected id lookup table"),
        };

        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(chunk.iter()), |row| {
            row.get::<_, String>(0)
        })?;

        for row in rows {
            existing.insert(row?);
        }
    }

    Ok(existing)
}

fn filter_existing_dispatch_ids(conn: &Connection, dispatch_ids: &[String]) -> Result<Vec<String>> {
    let existing_dispatch_ids = load_existing_ids(conn, "task_dispatches", dispatch_ids)?;
    let mut filtered = Vec::new();
    let mut seen = HashSet::new();
    for dispatch_id in dispatch_ids {
        let trimmed = dispatch_id.trim();
        if trimmed.is_empty()
            || !existing_dispatch_ids.contains(trimmed)
            || !seen.insert(trimmed.to_string())
        {
            continue;
        }
        filtered.push(trimmed.to_string());
    }
    Ok(filtered)
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

fn backfill_auto_queue_dispatch_history(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "INSERT OR IGNORE INTO auto_queue_entry_dispatch_history (
            entry_id, dispatch_id, trigger_source, created_at
        )
        SELECT
            id,
            dispatch_id,
            'schema_backfill_current',
            COALESCE(dispatched_at, created_at, CURRENT_TIMESTAMP)
        FROM auto_queue_entries
        WHERE NULLIF(TRIM(dispatch_id), '') IS NOT NULL;",
    )?;

    let mut stmt = conn.prepare(
        "SELECT id, context, CAST(created_at AS TEXT)
         FROM task_dispatches
         WHERE NULLIF(TRIM(COALESCE(context, '')), '') IS NOT NULL",
    )?;
    let dispatches = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
        ))
    })?;

    for dispatch in dispatches {
        let (dispatch_id, context_raw, created_at) = dispatch?;
        let Ok(context) = serde_json::from_str::<serde_json::Value>(&context_raw) else {
            continue;
        };
        let Some(entry_id) = context
            .get("entry_id")
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty())
        else {
            continue;
        };

        let entry_exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM auto_queue_entries WHERE id = ?1)",
                [entry_id],
                |row| row.get(0),
            )
            .unwrap_or(false);
        if !entry_exists {
            continue;
        }
        conn.execute(
            "INSERT OR IGNORE INTO auto_queue_entry_dispatch_history (
                entry_id, dispatch_id, trigger_source, created_at
            ) VALUES (?1, ?2, 'schema_backfill_context', COALESCE(?3, CURRENT_TIMESTAMP))",
            rusqlite::params![entry_id, dispatch_id, created_at],
        )?;
    }

    Ok(())
}

fn backfill_auto_queue_phase_gates(conn: &Connection) -> Result<()> {
    let rows = conn
        .prepare(
            "SELECT key, value
             FROM kv_meta
             WHERE key LIKE 'aq_phase_gate:%'",
        )?
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    struct LegacyKvPhaseGateState {
        run_id: String,
        phase: i64,
        status: String,
        verdict: Option<String>,
        dispatch_ids: Vec<String>,
        pass_verdict: String,
        next_phase: Option<i64>,
        final_phase: i64,
        anchor_card_id: Option<String>,
        failure_reason: Option<String>,
        created_at: Option<String>,
    }

    let mut parsed_rows = Vec::new();
    let mut candidate_run_ids = Vec::new();
    let mut candidate_anchor_card_ids = Vec::new();
    for (key, raw_value) in &rows {
        let suffix = key.strip_prefix("aq_phase_gate:").unwrap_or(key);
        let mut parts = suffix.rsplitn(2, ':');
        let phase = parts
            .next()
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(0);
        let run_id_from_key = parts.next().unwrap_or_default().to_string();
        let Ok(state) = serde_json::from_str::<serde_json::Value>(raw_value) else {
            continue;
        };

        let run_id = state
            .get("run_id")
            .and_then(|value| value.as_str())
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(run_id_from_key.as_str())
            .to_string();
        let status = state
            .get("status")
            .and_then(|value| value.as_str())
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("pending")
            .to_string();
        let verdict = state
            .get("failed_verdict")
            .and_then(|value| value.as_str())
            .map(str::to_string);
        let next_phase = state.get("next_phase").and_then(|value| value.as_i64());
        let final_phase = state
            .get("final_phase")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        let anchor_card_id = state
            .get("anchor_card_id")
            .and_then(|value| value.as_str())
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string);
        let failure_reason = state
            .get("failed_reason")
            .and_then(|value| value.as_str())
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string);
        let created_at = state
            .get("created_at")
            .and_then(|value| value.as_str())
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string);
        let pass_verdict = state
            .get("gates")
            .and_then(|value| value.as_array())
            .and_then(|items| items.first())
            .and_then(|item| item.get("pass_verdict"))
            .and_then(|value| value.as_str())
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("phase_gate_passed")
            .to_string();
        let dispatch_ids = state
            .get("dispatch_ids")
            .and_then(|value| value.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str())
                    .filter(|value| !value.trim().is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let final_phase = if final_phase { 1 } else { 0 };

        candidate_run_ids.push(run_id.clone());
        if let Some(anchor_card_id) = anchor_card_id.as_ref() {
            candidate_anchor_card_ids.push(anchor_card_id.clone());
        }
        parsed_rows.push(LegacyKvPhaseGateState {
            run_id,
            phase,
            status,
            verdict,
            dispatch_ids,
            pass_verdict,
            next_phase,
            final_phase,
            anchor_card_id,
            failure_reason,
            created_at,
        });
    }

    let valid_run_ids = load_existing_ids(conn, "auto_queue_runs", &candidate_run_ids)?;
    let valid_anchor_card_ids =
        load_existing_ids(conn, "kanban_cards", &candidate_anchor_card_ids)?;

    for row in parsed_rows {
        if !valid_run_ids.contains(row.run_id.as_str()) {
            continue;
        }

        let valid_dispatch_ids = filter_existing_dispatch_ids(conn, &row.dispatch_ids)?;
        let anchor_card_id = row
            .anchor_card_id
            .filter(|value| valid_anchor_card_ids.contains(value.as_str()));

        if valid_dispatch_ids.is_empty() {
            conn.execute(
                "INSERT INTO auto_queue_phase_gates (
                    run_id,
                    phase,
                    status,
                    verdict,
                    dispatch_id,
                    pass_verdict,
                    next_phase,
                    final_phase,
                    anchor_card_id,
                    failure_reason,
                    created_at,
                    updated_at
                ) VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6, ?7, ?8, ?9, COALESCE(?10, CURRENT_TIMESTAMP), datetime('now'))",
                rusqlite::params![
                    row.run_id,
                    row.phase,
                    row.status,
                    row.verdict,
                    row.pass_verdict,
                    row.next_phase,
                    row.final_phase,
                    anchor_card_id,
                    row.failure_reason,
                    row.created_at,
                ],
            )?;
            continue;
        }

        for dispatch_id in valid_dispatch_ids {
            conn.execute(
                "INSERT INTO auto_queue_phase_gates (
                    run_id,
                    phase,
                    status,
                    verdict,
                    dispatch_id,
                    pass_verdict,
                    next_phase,
                    final_phase,
                    anchor_card_id,
                    failure_reason,
                    created_at,
                    updated_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, COALESCE(?11, CURRENT_TIMESTAMP), datetime('now'))
                ON CONFLICT(dispatch_id) DO UPDATE SET
                    run_id = excluded.run_id,
                    phase = excluded.phase,
                    status = excluded.status,
                    verdict = excluded.verdict,
                    pass_verdict = excluded.pass_verdict,
                    next_phase = excluded.next_phase,
                    final_phase = excluded.final_phase,
                    anchor_card_id = excluded.anchor_card_id,
                    failure_reason = excluded.failure_reason,
                    updated_at = datetime('now')",
                rusqlite::params![
                    row.run_id,
                    row.phase,
                    row.status,
                    row.verdict,
                    dispatch_id,
                    row.pass_verdict,
                    row.next_phase,
                    row.final_phase,
                    anchor_card_id,
                    row.failure_reason,
                    row.created_at,
                ],
            )?;
        }
    }

    conn.execute("DELETE FROM kv_meta WHERE key LIKE 'aq_phase_gate:%'", [])?;
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
            events_json       TEXT NOT NULL DEFAULT '[]',
            duration_ms       INTEGER,
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
    let _ = conn.execute_batch(
        "ALTER TABLE session_transcripts ADD COLUMN events_json TEXT NOT NULL DEFAULT '[]';",
    );
    let _ = conn.execute_batch("ALTER TABLE session_transcripts ADD COLUMN duration_ms INTEGER;");
    migrate_legacy_session_transcripts_agent_fk(conn)?;
    Ok(())
}

fn ensure_turns_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS turns (
            turn_id              TEXT PRIMARY KEY,
            session_key          TEXT,
            thread_id            TEXT,
            thread_title         TEXT,
            channel_id           TEXT NOT NULL,
            agent_id             TEXT,
            provider             TEXT,
            session_id           TEXT,
            dispatch_id          TEXT,
            started_at           TEXT NOT NULL,
            finished_at          TEXT NOT NULL,
            duration_ms          INTEGER,
            input_tokens         INTEGER NOT NULL DEFAULT 0,
            cache_create_tokens  INTEGER NOT NULL DEFAULT 0,
            cache_read_tokens    INTEGER NOT NULL DEFAULT 0,
            output_tokens        INTEGER NOT NULL DEFAULT 0,
            created_at           DATETIME DEFAULT CURRENT_TIMESTAMP
        );
        CREATE INDEX IF NOT EXISTS idx_turns_channel_id
            ON turns (channel_id);
        CREATE INDEX IF NOT EXISTS idx_turns_thread_id
            ON turns (thread_id);
        CREATE INDEX IF NOT EXISTS idx_turns_agent_id
            ON turns (agent_id);
        CREATE INDEX IF NOT EXISTS idx_turns_session_id
            ON turns (session_id);
        CREATE INDEX IF NOT EXISTS idx_turns_dispatch_id
            ON turns (dispatch_id);
        CREATE INDEX IF NOT EXISTS idx_turns_finished_at
            ON turns (finished_at DESC);",
    )?;
    Ok(())
}

fn run_migration_once(
    conn: &Connection,
    meta_key: &str,
    migration: fn(&Connection) -> Result<()>,
) -> Result<()> {
    let already_ran = conn
        .query_row(
            "SELECT 1 FROM kv_meta WHERE key = ?1 LIMIT 1",
            [meta_key],
            |_| Ok(()),
        )
        .is_ok();
    if already_ran {
        return Ok(());
    }

    migration(conn)?;
    conn.execute(
        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, '1')",
        [meta_key],
    )?;
    Ok(())
}

fn backfill_session_agent_ids(conn: &Connection) -> Result<()> {
    let sessions = conn
        .prepare(
            "SELECT session_key, thread_channel_id, active_dispatch_id
             FROM sessions
             WHERE NULLIF(TRIM(agent_id), '') IS NULL",
        )?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    for (session_key, thread_channel_id, dispatch_id) in sessions {
        let Some(agent_id) = crate::db::session_agent_resolution::resolve_agent_id_for_session(
            conn,
            None,
            Some(session_key.as_str()),
            None,
            thread_channel_id.as_deref(),
            dispatch_id.as_deref(),
        ) else {
            continue;
        };

        conn.execute(
            "UPDATE sessions
             SET agent_id = ?2
             WHERE session_key = ?1
               AND NULLIF(TRIM(agent_id), '') IS NULL",
            rusqlite::params![session_key, agent_id],
        )?;
    }

    Ok(())
}

fn backfill_pending_blocked_statuses(conn: &Connection) -> Result<()> {
    let legacy_cards = conn
        .prepare(
            "SELECT id, status, blocked_reason
             FROM kanban_cards
             WHERE status IN ('pending_decision', 'blocked')",
        )?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    if legacy_cards.is_empty() {
        return Ok(());
    }

    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| -> Result<usize> {
        let mut migrated = 0usize;

        for (card_id, legacy_status, existing_blocked_reason) in legacy_cards {
            let prior_status = conn
                .query_row(
                    "SELECT from_status FROM kanban_audit_logs
                     WHERE card_id = ?1 AND to_status = ?2
                     ORDER BY created_at DESC, id DESC LIMIT 1",
                    rusqlite::params![card_id, legacy_status],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
                .and_then(|status| {
                    let trimmed = status.trim();
                    if trimmed.is_empty() || matches!(trimmed, "pending_decision" | "blocked") {
                        None
                    } else {
                        Some(trimmed.to_string())
                    }
                });

            let fallback_reason = match legacy_status.as_str() {
                "pending_decision" => "manual intervention migrated from legacy pending_decision",
                "blocked" => "manual intervention migrated from legacy blocked",
                _ => "manual intervention migrated from legacy state",
            };
            let blocked_reason = existing_blocked_reason
                .and_then(|value| {
                    let trimmed = value.trim();
                    if trimmed.is_empty() {
                        None
                    } else {
                        Some(trimmed.to_string())
                    }
                })
                .unwrap_or_else(|| fallback_reason.to_string());

            let target_status = match legacy_status.as_str() {
                "pending_decision" => prior_status.unwrap_or_else(|| "review".to_string()),
                "blocked" => prior_status.unwrap_or_else(|| "in_progress".to_string()),
                _ => continue,
            };

            if target_status == "review" {
                conn.execute(
                    "UPDATE kanban_cards
                     SET status = 'review',
                         review_status = 'dilemma_pending',
                         blocked_reason = NULL,
                         review_entered_at = COALESCE(review_entered_at, updated_at, datetime('now')),
                         updated_at = datetime('now')
                     WHERE id = ?1",
                    [&card_id],
                )?;
                conn.execute(
                    "INSERT INTO card_review_state (
                         card_id, state, pending_dispatch_id, review_entered_at, updated_at
                     )
                     VALUES (
                         ?1, 'dilemma_pending', NULL,
                         COALESCE(
                             (SELECT review_entered_at FROM kanban_cards WHERE id = ?1),
                             datetime('now')
                         ),
                         datetime('now')
                     )
                     ON CONFLICT(card_id) DO UPDATE SET
                         state = 'dilemma_pending',
                         pending_dispatch_id = NULL,
                         review_entered_at = COALESCE(
                             card_review_state.review_entered_at,
                             excluded.review_entered_at
                         ),
                         updated_at = datetime('now')",
                    [&card_id],
                )?;
            } else {
                conn.execute(
                    "UPDATE kanban_cards
                     SET status = ?1,
                         review_status = NULL,
                         blocked_reason = ?2,
                         updated_at = datetime('now')
                     WHERE id = ?3",
                    rusqlite::params![target_status, blocked_reason, card_id],
                )?;
                conn.execute(
                    "INSERT INTO card_review_state (card_id, state, pending_dispatch_id, updated_at)
                     VALUES (?1, 'idle', NULL, datetime('now'))
                     ON CONFLICT(card_id) DO UPDATE SET
                         state = 'idle',
                         pending_dispatch_id = NULL,
                         updated_at = datetime('now')",
                    [&card_id],
                )?;
            }

            migrated += 1;
        }

        Ok(migrated)
    })();

    match result {
        Ok(migrated) => {
            conn.execute_batch("COMMIT")?;
            tracing::info!(
                "Migrated {migrated} legacy pending_decision/blocked kanban cards to manual-intervention model"
            );
            Ok(())
        }
        Err(err) => {
            conn.execute_batch("ROLLBACK").ok();
            Err(err)
        }
    }
}

fn backfill_session_transcript_agent_ids(conn: &Connection) -> Result<()> {
    let transcripts = conn
        .prepare(
            "SELECT id, session_key, dispatch_id
             FROM session_transcripts
             WHERE NULLIF(TRIM(agent_id), '') IS NULL",
        )?
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    for (id, session_key, dispatch_id) in transcripts {
        let Some(agent_id) = crate::db::session_agent_resolution::resolve_agent_id_for_session(
            conn,
            None,
            session_key.as_deref(),
            None,
            None,
            dispatch_id.as_deref(),
        ) else {
            continue;
        };

        conn.execute(
            "UPDATE session_transcripts
             SET agent_id = ?2
             WHERE id = ?1
               AND NULLIF(TRIM(agent_id), '') IS NULL",
            rusqlite::params![id, agent_id],
        )?;
    }

    Ok(())
}

fn ensure_memento_feedback_stats_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS memento_feedback_turn_stats (
            turn_id                       TEXT PRIMARY KEY,
            stat_date                     TEXT NOT NULL,
            agent_id                      TEXT NOT NULL,
            provider                      TEXT NOT NULL,
            recall_count                  INTEGER NOT NULL DEFAULT 0,
            manual_tool_feedback_count    INTEGER NOT NULL DEFAULT 0,
            manual_covered_recall_count   INTEGER NOT NULL DEFAULT 0,
            auto_tool_feedback_count      INTEGER NOT NULL DEFAULT 0,
            covered_recall_count          INTEGER NOT NULL DEFAULT 0,
            created_at                    DATETIME DEFAULT CURRENT_TIMESTAMP
        );
        CREATE INDEX IF NOT EXISTS idx_memento_feedback_turn_stats_date_agent
            ON memento_feedback_turn_stats (stat_date, agent_id, provider);",
    )?;

    conn.execute_batch(
        "DROP VIEW IF EXISTS memento_feedback_daily_stats;
         CREATE VIEW memento_feedback_daily_stats AS
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
                    THEN CAST(SUM(manual_covered_recall_count) AS REAL) / SUM(recall_count)
                ELSE 0.0
            END AS compliance_rate,
            CASE
                WHEN SUM(recall_count) > 0
                    THEN CAST(SUM(covered_recall_count) AS REAL) / SUM(recall_count)
                ELSE 0.0
            END AS coverage_rate
         FROM memento_feedback_turn_stats
         GROUP BY stat_date, agent_id, provider;",
    )?;

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
            events_json       TEXT NOT NULL DEFAULT '[]',
            duration_ms       INTEGER,
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
            events_json,
            duration_ms,
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
            COALESCE(events_json, '[]'),
            duration_ms,
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
        assert!(table_sql.contains("events_json"));
        assert!(table_sql.contains("duration_ms"));

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM session_transcripts", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 1);

        let events_json: String = conn
            .query_row(
                "SELECT events_json FROM session_transcripts WHERE turn_id = 'discord:legacy:1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(events_json, "[]");

        let fts_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM session_transcripts_fts", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(fts_count, 1);
    }

    #[test]
    fn migrate_backfills_session_and_transcript_agent_ids() {
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
            "INSERT INTO agents (id, name, discord_channel_alt)
             VALUES ('project-skillmanager', 'SkillManager', 'project-skillmanager-extremely-verbose-channel-cdx')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, created_at, updated_at)
             VALUES ('card-backfill-agent', 'Backfill Agent', 'in_progress', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches
             (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at)
             VALUES (
                'dispatch-backfill-agent',
                'card-backfill-agent',
                'project-skillmanager',
                'implementation',
                'dispatched',
                'Backfill Agent',
                datetime('now'),
                datetime('now')
             )",
            [],
        )
        .unwrap();

        conn.execute_batch(
            "CREATE TABLE session_transcripts (
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
            );",
        )
        .unwrap();

        let session_key = "codex/hash123/mac-mini:AgentDesk-codex-project-skillmanager-extremely-v";
        conn.execute(
            "INSERT INTO sessions (
                session_key, agent_id, provider, status, active_dispatch_id, last_heartbeat, created_at
             ) VALUES (
                ?1, NULL, 'codex', 'working', 'dispatch-backfill-agent', datetime('now'), datetime('now')
             )",
            [session_key],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_transcripts (
                turn_id, session_key, channel_id, agent_id, provider, dispatch_id, user_message, assistant_message
             ) VALUES (
                'discord:backfill:1', ?1, '1492661418665971792', NULL, 'codex', 'dispatch-backfill-agent', 'legacy user', 'legacy assistant'
             )",
            [session_key],
        )
        .unwrap();

        migrate(&conn).unwrap();

        let session_agent_id: Option<String> = conn
            .query_row(
                "SELECT agent_id FROM sessions WHERE session_key = ?1",
                [session_key],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(session_agent_id.as_deref(), Some("project-skillmanager"));

        let transcript_agent_id: Option<String> = conn
            .query_row(
                "SELECT agent_id FROM session_transcripts WHERE turn_id = 'discord:backfill:1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(transcript_agent_id.as_deref(), Some("project-skillmanager"));

        let session_backfill_flag: String = conn
            .query_row(
                "SELECT value FROM kv_meta WHERE key = ?1",
                [SESSION_AGENT_ID_BACKFILL_META_KEY],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(session_backfill_flag, "1");

        let transcript_backfill_flag: String = conn
            .query_row(
                "SELECT value FROM kv_meta WHERE key = ?1",
                [SESSION_TRANSCRIPT_AGENT_ID_BACKFILL_META_KEY],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(transcript_backfill_flag, "1");
    }

    #[test]
    fn backfill_auto_queue_phase_gates_moves_kv_json_into_table() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        migrate(&conn).unwrap();

        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, created_at, updated_at)
             VALUES ('card-phase-backfill', 'Phase Gate', 'done', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES ('run-phase-backfill', 'test/repo', 'agent-1', 'paused')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at, context
             ) VALUES (
                'dispatch-phase-backfill',
                'card-phase-backfill',
                'agent-1',
                'phase-gate',
                'pending',
                'Phase Gate',
                datetime('now'),
                datetime('now'),
                '{}'
             )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO kv_meta (key, value)
             VALUES ('aq_phase_gate:run-phase-backfill:2', ?1)",
            [serde_json::json!({
                "run_id": "run-phase-backfill",
                "batch_phase": 2,
                "next_phase": 3,
                "final_phase": false,
                "anchor_card_id": "card-phase-backfill",
                "status": "failed",
                "dispatch_ids": ["dispatch-phase-backfill"],
                "failed_verdict": "reject",
                "failed_reason": "needs follow-up",
                "created_at": "2026-04-13T00:00:00Z",
                "gates": [
                    { "pass_verdict": "phase_gate_passed" }
                ]
            })
            .to_string()],
        )
        .unwrap();

        backfill_auto_queue_phase_gates(&conn).unwrap();

        let row: (
            String,
            i64,
            String,
            Option<String>,
            Option<String>,
            Option<i64>,
            i64,
            Option<String>,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT run_id, phase, status, verdict, dispatch_id, next_phase, final_phase, anchor_card_id, failure_reason
                 FROM auto_queue_phase_gates
                 WHERE run_id = 'run-phase-backfill' AND phase = 2",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                        row.get(8)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(row.0, "run-phase-backfill");
        assert_eq!(row.1, 2);
        assert_eq!(row.2, "failed");
        assert_eq!(row.3.as_deref(), Some("reject"));
        assert_eq!(row.4.as_deref(), Some("dispatch-phase-backfill"));
        assert_eq!(row.5, Some(3));
        assert_eq!(row.6, 0);
        assert_eq!(row.7.as_deref(), Some("card-phase-backfill"));
        assert_eq!(row.8.as_deref(), Some("needs follow-up"));

        let legacy_key_exists: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM kv_meta WHERE key = 'aq_phase_gate:run-phase-backfill:2'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!legacy_key_exists);
    }

    #[test]
    fn backfill_auto_queue_phase_gates_filters_missing_fk_targets() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        migrate(&conn).unwrap();

        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, created_at, updated_at)
             VALUES ('card-phase-backfill-valid', 'Phase Gate FK', 'done', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES ('run-phase-backfill-valid', 'test/repo', 'agent-1', 'paused')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO kv_meta (key, value)
             VALUES ('aq_phase_gate:run-phase-backfill-valid:1', ?1)",
            [serde_json::json!({
                "run_id": "run-phase-backfill-valid",
                "batch_phase": 1,
                "final_phase": false,
                "anchor_card_id": "card-phase-backfill-missing",
                "status": "pending",
                "dispatch_ids": ["dispatch-phase-backfill-missing"]
            })
            .to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO kv_meta (key, value)
             VALUES ('aq_phase_gate:run-phase-backfill-missing:2', ?1)",
            [serde_json::json!({
                "run_id": "run-phase-backfill-missing",
                "batch_phase": 2,
                "anchor_card_id": "card-phase-backfill-valid",
                "status": "pending"
            })
            .to_string()],
        )
        .unwrap();

        backfill_auto_queue_phase_gates(&conn).unwrap();

        let rows: Vec<(String, i64, Option<String>)> = conn
            .prepare(
                "SELECT run_id, phase, anchor_card_id
                 FROM auto_queue_phase_gates
                 ORDER BY phase",
            )
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();

        assert_eq!(
            rows,
            vec![("run-phase-backfill-valid".to_string(), 1, None,)]
        );

        let legacy_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM kv_meta WHERE key LIKE 'aq_phase_gate:%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(legacy_count, 0);
    }

    #[test]
    fn backfill_auto_queue_phase_gates_chunks_large_fk_lookups() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        migrate(&conn).unwrap();

        let total = EXISTING_ID_LOOKUP_CHUNK_SIZE + 25;
        for index in 0..total {
            let card_id = format!("card-phase-batch-{index}");
            let run_id = format!("run-phase-batch-{index}");
            let dispatch_id = format!("dispatch-phase-batch-{index}");
            let phase_key = format!("aq_phase_gate:{run_id}:1");

            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, created_at, updated_at)
                 VALUES (?1, ?2, 'done', datetime('now'), datetime('now'))",
                rusqlite::params![card_id, format!("Phase Gate Card {index}")],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
                 VALUES (?1, 'test/repo', 'agent-1', 'paused')",
                rusqlite::params![run_id],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (
                    id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at, context
                 ) VALUES (
                    ?1, ?2, 'agent-1', 'phase-gate', 'pending', ?3, datetime('now'), datetime('now'), '{}'
                 )",
                rusqlite::params![dispatch_id, card_id, format!("Phase Gate Dispatch {index}")],
            )
            .unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
                rusqlite::params![
                    phase_key,
                    serde_json::json!({
                        "run_id": run_id,
                        "batch_phase": 1,
                        "final_phase": false,
                        "anchor_card_id": card_id,
                        "status": "pending",
                        "dispatch_ids": [dispatch_id]
                    })
                    .to_string()
                ],
            )
            .unwrap();
        }

        backfill_auto_queue_phase_gates(&conn).unwrap();

        let phase_gate_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM auto_queue_phase_gates", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(phase_gate_count, total as i64);

        let linked_anchor_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM auto_queue_phase_gates WHERE anchor_card_id IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(linked_anchor_count, total as i64);

        let legacy_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM kv_meta WHERE key LIKE 'aq_phase_gate:%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(legacy_count, 0);
    }

    #[test]
    fn auto_queue_phase_gates_enforce_foreign_keys_and_uniqueness() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        migrate(&conn).unwrap();

        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, created_at, updated_at)
             VALUES ('card-phase-fk', 'Phase Gate FK', 'ready', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES ('run-phase-fk', 'test/repo', 'agent-1', 'paused')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at, context
             ) VALUES (
                'dispatch-phase-fk',
                'card-phase-fk',
                'agent-1',
                'phase-gate',
                'pending',
                'Phase Gate FK',
                datetime('now'),
                datetime('now'),
                '{}'
             )",
            [],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO auto_queue_phase_gates (
                run_id, phase, status, dispatch_id, pass_verdict
             ) VALUES ('run-phase-fk', 1, 'pending', NULL, 'phase_gate_passed')",
            [],
        )
        .unwrap();
        assert!(
            conn.execute(
                "INSERT INTO auto_queue_phase_gates (
                    run_id, phase, status, dispatch_id, pass_verdict
                 ) VALUES ('run-phase-fk', 1, 'pending', NULL, 'phase_gate_passed')",
                [],
            )
            .is_err()
        );

        conn.execute(
            "INSERT INTO auto_queue_phase_gates (
                run_id, phase, status, dispatch_id, pass_verdict
             ) VALUES ('run-phase-fk', 2, 'pending', 'dispatch-phase-fk', 'phase_gate_passed')",
            [],
        )
        .unwrap();
        assert!(
            conn.execute(
                "INSERT INTO auto_queue_phase_gates (
                    run_id, phase, status, dispatch_id, pass_verdict
                 ) VALUES ('run-phase-fk', 3, 'pending', 'dispatch-phase-fk', 'phase_gate_passed')",
                [],
            )
            .is_err()
        );
        assert!(
            conn.execute(
                "INSERT INTO auto_queue_phase_gates (
                    run_id, phase, status, dispatch_id, pass_verdict
                 ) VALUES ('run-phase-fk', 4, 'pending', 'dispatch-missing', 'phase_gate_passed')",
                [],
            )
            .is_err()
        );
    }

    #[test]
    fn migrate_rebuilds_legacy_auto_queue_phase_gates_table() {
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
        conn.execute_batch(
            "CREATE TABLE auto_queue_runs (
                id          TEXT PRIMARY KEY,
                repo        TEXT,
                agent_id    TEXT,
                status      TEXT DEFAULT 'active',
                created_at  DATETIME DEFAULT CURRENT_TIMESTAMP,
                completed_at DATETIME
            );
            CREATE TABLE auto_queue_phase_gates (
                run_id          TEXT NOT NULL REFERENCES auto_queue_runs(id) ON DELETE CASCADE,
                phase           INTEGER NOT NULL,
                status          TEXT NOT NULL DEFAULT 'pending',
                verdict         TEXT,
                dispatch_id     TEXT UNIQUE REFERENCES task_dispatches(id) ON DELETE CASCADE,
                pass_verdict    TEXT NOT NULL DEFAULT 'phase_gate_passed',
                next_phase      INTEGER,
                final_phase     INTEGER NOT NULL DEFAULT 0,
                anchor_card_id  TEXT REFERENCES kanban_cards(id) ON DELETE SET NULL,
                failure_reason  TEXT,
                created_at      DATETIME DEFAULT CURRENT_TIMESTAMP,
                updated_at      DATETIME DEFAULT CURRENT_TIMESTAMP
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, created_at, updated_at)
             VALUES ('card-phase-legacy', 'Legacy Gate', 'ready', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES ('run-phase-legacy', 'test/repo', 'agent-1', 'paused')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_phase_gates (
                run_id, phase, status, dispatch_id, anchor_card_id, pass_verdict
             ) VALUES ('run-phase-legacy', 1, 'pending', NULL, 'card-phase-legacy', 'phase_gate_passed')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_phase_gates (
                run_id, phase, status, dispatch_id, anchor_card_id, pass_verdict
             ) VALUES ('run-phase-legacy', 1, 'failed', NULL, 'card-phase-legacy', 'phase_gate_passed')",
            [],
        )
        .unwrap();

        migrate(&conn).unwrap();

        assert!(auto_queue_has_column(&conn, "auto_queue_phase_gates", "id"));
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM auto_queue_phase_gates
                 WHERE run_id = 'run-phase-legacy' AND phase = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);

        let status: String = conn
            .query_row(
                "SELECT status FROM auto_queue_phase_gates
                 WHERE run_id = 'run-phase-legacy' AND phase = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "failed");
    }
}
