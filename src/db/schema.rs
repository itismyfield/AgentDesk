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
    let _ = conn.execute_batch("ALTER TABLE meetings ADD COLUMN thread_id TEXT;");
    let _ = conn.execute_batch("ALTER TABLE meetings ADD COLUMN primary_provider TEXT;");
    let _ = conn.execute_batch("ALTER TABLE meetings ADD COLUMN reviewer_provider TEXT;");
    let _ = conn.execute_batch("ALTER TABLE meetings ADD COLUMN participant_names TEXT;");
    let _ = conn.execute_batch("ALTER TABLE meetings ADD COLUMN created_at INTEGER;");
    let _ = conn.execute_batch("ALTER TABLE sessions ADD COLUMN thread_channel_id TEXT;");
    let _ = conn.execute_batch("ALTER TABLE sessions ADD COLUMN claude_session_id TEXT;");

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
        "UPDATE kanban_cards SET requested_at = updated_at WHERE status = 'requested' AND requested_at IS NULL;
         UPDATE kanban_cards SET started_at = updated_at WHERE status = 'in_progress' AND started_at IS NULL;
         UPDATE kanban_cards SET review_entered_at = updated_at WHERE status = 'review' AND review_entered_at IS NULL;
         UPDATE kanban_cards SET awaiting_dod_at = updated_at WHERE status = 'review' AND review_status = 'awaiting_dod' AND awaiting_dod_at IS NULL;",
    );

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

    // #197: dev-deploy and e2e-test stages disabled until #197 is implemented.
    // Re-enable when deploy-pipeline.js can handle these stages.
    // ensure_pipeline_stage(conn, AGENTDESK_REPO_ID, "dev-deploy", 100, Some("review_pass"), Some("self"), Some("no_rs_changes"))?;
    // ensure_pipeline_stage(conn, AGENTDESK_REPO_ID, "e2e-test", 200, None, Some("counter"), Some("no_rs_changes"))?;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_does_not_backfill_disabled_agentdesk_pipeline_stages_for_existing_repo() {
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

        let rows: Vec<(String, i64, Option<String>, Option<String>, Option<String>)> = conn
            .prepare(
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
            .unwrap();

        assert!(rows.is_empty());
    }

    #[test]
    fn seed_builtin_pipeline_stages_is_noop_while_disabled() {
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
        assert_eq!(count, 0);
    }
}
