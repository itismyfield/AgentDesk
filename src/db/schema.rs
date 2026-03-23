use anyhow::Result;
use rusqlite::Connection;

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
    let _ = conn.execute_batch("ALTER TABLE task_dispatches ADD COLUMN thread_id TEXT;");
    let _ = conn.execute_batch("ALTER TABLE task_dispatches ADD COLUMN retry_count INTEGER DEFAULT 0;");
    let _ = conn.execute_batch("ALTER TABLE meetings ADD COLUMN thread_id TEXT;");
    let _ = conn.execute_batch("ALTER TABLE sessions ADD COLUMN thread_channel_id TEXT;");

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
        tracing::info!(
            "Cleaned {cleaned} done cards with stale review_status (fix #80)"
        );
    }
    // Cancel stale pending review/review-decision dispatches for done cards
    let cancelled = conn
        .execute(
            "UPDATE task_dispatches SET status = 'cancelled', updated_at = datetime('now') \
             WHERE status IN ('pending', 'dispatched') \
             AND dispatch_type IN ('review', 'review-decision') \
             AND kanban_card_id IN (SELECT id FROM kanban_cards WHERE status = 'done')",
            [],
        )
        .unwrap_or(0);
    if cancelled > 0 {
        tracing::info!(
            "Cancelled {cancelled} stale review dispatches for done cards (fix #80)"
        );
    }

    // Rate limit cache table (provider → cached rate-limit JSON)
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS rate_limit_cache (
            provider   TEXT PRIMARY KEY,
            data       TEXT,
            fetched_at INTEGER
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

    Ok(())
}
