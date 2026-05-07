use super::*;
use crate::db::Db;
use crate::dispatch::test_support::{load_dispatch_events, test_db};

fn seed_user_cancel_fixture(db: &Db, card_id: &str, dispatch_id: &str, entry_id: &str) {
    let conn = db.separate_conn().unwrap();
    conn.execute(
        "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, created_at, updated_at) \
             VALUES (?1, 'User Cancel Card', 'in_progress', 'agent-1', datetime('now'), datetime('now'))",
        [card_id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at) \
             VALUES (?1, ?2, 'agent-1', 'implementation', 'dispatched', 'User Cancel', datetime('now'), datetime('now'))",
        sqlite_test::params![dispatch_id, card_id],
    )
    .unwrap();
    // Use a per-card run id so fixtures from sibling tests do not collide
    // when multiple regression assertions seed rows against the same DB.
    let run_id = format!("run-{entry_id}");
    conn.execute(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status) \
             VALUES (?1, 'repo', 'agent-1', 'active')",
        [&run_id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO auto_queue_entries \
                 (id, run_id, kanban_card_id, agent_id, status, dispatch_id, dispatched_at) \
             VALUES (?1, ?2, ?3, 'agent-1', 'dispatched', ?4, datetime('now'))",
        sqlite_test::params![entry_id, run_id, card_id, dispatch_id],
    )
    .unwrap();
}

#[test]
fn cancel_dispatch_resets_linked_auto_queue_entry() {
    let db = test_db();
    let conn = db.separate_conn().unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO agents (id, name, discord_channel_id, discord_channel_alt) \
             VALUES ('agent-1', 'Agent 1', '123', '456')",
        [],
    )
    .unwrap();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS auto_queue_runs (
                id TEXT PRIMARY KEY,
                repo TEXT,
                agent_id TEXT,
                status TEXT DEFAULT 'active'
            );
            CREATE TABLE IF NOT EXISTS auto_queue_entries (
                id TEXT PRIMARY KEY,
                run_id TEXT REFERENCES auto_queue_runs(id),
                kanban_card_id TEXT REFERENCES kanban_cards(id),
                agent_id TEXT,
                status TEXT DEFAULT 'pending',
                dispatch_id TEXT,
                dispatched_at DATETIME,
                completed_at DATETIME,
                updated_at DATETIME
            );
            CREATE TABLE IF NOT EXISTS auto_queue_entry_dispatch_history (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                entry_id TEXT NOT NULL,
                dispatch_id TEXT NOT NULL,
                trigger_source TEXT,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                UNIQUE(entry_id, dispatch_id)
            );",
    )
    .unwrap();
    let _ = conn.execute(
        "ALTER TABLE auto_queue_entries ADD COLUMN updated_at DATETIME",
        [],
    );
    conn.execute(
        "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, created_at, updated_at) \
             VALUES ('card-aq', 'AQ Card', 'requested', 'agent-1', datetime('now'), datetime('now'))",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at) \
             VALUES ('dispatch-aq', 'card-aq', 'agent-1', 'implementation', 'dispatched', 'AQ', datetime('now'), datetime('now'))",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status) VALUES ('run-aq', 'repo', 'agent-1', 'active')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, dispatch_id, dispatched_at) \
             VALUES ('entry-aq', 'run-aq', 'card-aq', 'agent-1', 'dispatched', 'dispatch-aq', datetime('now'))",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO sessions (session_key, agent_id, provider, status, active_dispatch_id, session_info, created_at) \
             VALUES ('session-aq', 'agent-1', 'claude', 'turn_active', 'dispatch-aq', 'live dispatch', datetime('now'))",
        [],
    )
    .unwrap();

    let cancelled =
        cancel_dispatch_and_reset_auto_queue_on_conn(&conn, "dispatch-aq", Some("test")).unwrap();
    assert_eq!(cancelled, 1);

    let dispatch_status: String = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = 'dispatch-aq'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(dispatch_status, "cancelled");

    let (entry_status, entry_dispatch_id): (String, Option<String>) = conn
        .query_row(
            "SELECT status, dispatch_id FROM auto_queue_entries WHERE id = 'entry-aq'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(entry_status, "pending");
    assert!(entry_dispatch_id.is_none());
    let (session_status, active_dispatch_id, session_info): (
        String,
        Option<String>,
        Option<String>,
    ) = conn
        .query_row(
            "SELECT status, active_dispatch_id, session_info FROM sessions WHERE session_key = 'session-aq'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(session_status, "idle");
    assert!(active_dispatch_id.is_none());
    assert_eq!(session_info.as_deref(), Some("Dispatch cancelled"));
    assert_eq!(
        load_dispatch_events(&conn, "dispatch-aq"),
        vec![(
            Some("dispatched".to_string()),
            "cancelled".to_string(),
            "cancel_dispatch".to_string()
        )],
        "dispatch cancellation must be audited"
    );
}

#[test]
fn user_cancel_reason_whitelist_matches_turn_bridge_and_user_prefix() {
    assert!(
        is_user_cancel_reason(Some("turn_bridge_cancelled")),
        "reaction-stop reason must be classified as a user cancel"
    );
    assert!(
        is_user_cancel_reason(Some("user_reaction_stop")),
        "any user_-prefixed reason must classify as user cancel"
    );
    assert!(
        !is_user_cancel_reason(Some("superseded_by_reseed")),
        "supersession is a system cancel"
    );
    assert!(
        !is_user_cancel_reason(Some("auto_cancelled_on_terminal_card")),
        "terminal-card cleanup is a system cancel"
    );
    assert!(!is_user_cancel_reason(None));
    assert!(!is_user_cancel_reason(Some("")));
    assert!(!is_user_cancel_reason(Some("   ")));
}

#[test]
fn cancel_dispatch_with_user_reason_moves_entry_to_user_cancelled() {
    let db = test_db();
    seed_user_cancel_fixture(&db, "card-815-user", "dispatch-815-user", "entry-815-user");

    let conn = db.separate_conn().unwrap();
    let cancelled = cancel_dispatch_and_reset_auto_queue_on_conn(
        &conn,
        "dispatch-815-user",
        Some("turn_bridge_cancelled"),
    )
    .unwrap();
    assert_eq!(cancelled, 1);

    let dispatch_status: String = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = 'dispatch-815-user'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(dispatch_status, "cancelled");

    let (entry_status, entry_dispatch_id, completed_at): (
        String,
        Option<String>,
        Option<String>,
    ) = conn
        .query_row(
            "SELECT status, dispatch_id, completed_at FROM auto_queue_entries WHERE id = 'entry-815-user'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        entry_status, "user_cancelled",
        "user cancel must transition entry to non-dispatchable user_cancelled"
    );
    assert!(
        entry_dispatch_id.is_none(),
        "user_cancelled entry must detach from its dispatch"
    );
    assert!(
        completed_at.is_some(),
        "user_cancelled entry must stamp completed_at so run-finalization treats it as terminal"
    );

    let pending_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM auto_queue_entries e \
                 JOIN auto_queue_runs r ON e.run_id = r.id \
                 WHERE r.status = 'active' AND e.status = 'pending'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        pending_count, 0,
        "next auto-queue tick must not find the user-cancelled entry"
    );
    assert!(
        crate::db::auto_queue::is_dispatchable_entry_status("pending"),
        "pending entries must remain dispatchable"
    );
    assert!(
        !crate::db::auto_queue::is_dispatchable_entry_status("user_cancelled"),
        "user_cancelled must be non-dispatchable"
    );

    let card_status: String = conn
        .query_row(
            "SELECT status FROM kanban_cards WHERE id = 'card-815-user'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        card_status, "in_progress",
        "user cancel must not mark the card done"
    );

    let run_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-entry-815-user'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        run_status, "active",
        "user cancel must leave the run resumable, not auto-complete it"
    );
}

#[test]
fn user_cancelled_entry_can_be_restarted_via_pending_flip() {
    let db = test_db();
    seed_user_cancel_fixture(
        &db,
        "card-815-restart",
        "dispatch-815-restart",
        "entry-815-restart",
    );

    let conn = db.separate_conn().unwrap();
    cancel_dispatch_and_reset_auto_queue_on_conn(
        &conn,
        "dispatch-815-restart",
        Some("turn_bridge_cancelled"),
    )
    .unwrap();

    let (entry_status, run_status): (String, String) = conn
        .query_row(
            "SELECT e.status, r.status \
                 FROM auto_queue_entries e \
                 JOIN auto_queue_runs r ON e.run_id = r.id \
                 WHERE e.id = 'entry-815-restart'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(entry_status, "user_cancelled");
    assert_eq!(run_status, "active");

    let changed = conn
        .execute(
            "UPDATE auto_queue_entries
                 SET status = 'pending',
                     dispatch_id = NULL,
                     dispatched_at = NULL,
                     completed_at = NULL,
                     updated_at = datetime('now')
                 WHERE id = 'entry-815-restart' AND status = 'user_cancelled'",
            [],
        )
        .unwrap();
    assert!(
        changed > 0,
        "restart must transition user_cancelled -> pending"
    );

    let pending_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM auto_queue_entries e \
                 JOIN auto_queue_runs r ON e.run_id = r.id \
                 WHERE r.status = 'active' AND e.status = 'pending' AND e.id = 'entry-815-restart'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        pending_count, 1,
        "after restart, the entry must be re-dispatchable by the next tick"
    );
}

#[test]
fn cancel_dispatch_with_system_reason_preserves_pending_reset() {
    let db = test_db();
    seed_user_cancel_fixture(&db, "card-815-sys", "dispatch-815-sys", "entry-815-sys");

    let conn = db.separate_conn().unwrap();
    let cancelled = cancel_dispatch_and_reset_auto_queue_on_conn(
        &conn,
        "dispatch-815-sys",
        Some("superseded_by_reseed"),
    )
    .unwrap();
    assert_eq!(cancelled, 1);

    let (entry_status, entry_dispatch_id): (String, Option<String>) = conn
        .query_row(
            "SELECT status, dispatch_id FROM auto_queue_entries WHERE id = 'entry-815-sys'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        entry_status, "pending",
        "system cancels must still reset the entry to pending"
    );
    assert!(
        entry_dispatch_id.is_none(),
        "system cancel must still clear the stale dispatch pointer"
    );

    let pending_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM auto_queue_entries e \
                 JOIN auto_queue_runs r ON e.run_id = r.id \
                 WHERE r.status = 'active' AND e.status = 'pending'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        pending_count, 1,
        "system cancel must keep entry visible to the next tick"
    );
}

#[test]
fn user_stop_does_not_redispatch() {
    let db = test_db();
    seed_user_cancel_fixture(&db, "card-821-nore", "dispatch-821-nore", "entry-821-nore");

    let conn = db.separate_conn().unwrap();
    cancel_dispatch_and_reset_auto_queue_on_conn(
        &conn,
        "dispatch-821-nore",
        Some("turn_bridge_cancelled"),
    )
    .unwrap();

    let entry_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_entries WHERE id = 'entry-821-nore'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        entry_status, "user_cancelled",
        "user stop must mark the entry non-dispatchable"
    );

    let pending_visible: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM auto_queue_entries e \
                 JOIN auto_queue_runs r ON e.run_id = r.id \
                 WHERE r.status = 'active' AND e.status = 'pending' \
                   AND e.id = 'entry-821-nore'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        pending_visible, 0,
        "user-cancelled entries must not be seen by the next auto-queue tick"
    );
    assert!(
        !crate::db::auto_queue::is_dispatchable_entry_status("user_cancelled"),
        "user_cancelled must be a non-dispatchable terminal status"
    );
}

#[test]
fn user_stop_does_not_mark_done() {
    let db = test_db();
    seed_user_cancel_fixture(&db, "card-821-nd", "dispatch-821-nd", "entry-821-nd");

    let conn = db.separate_conn().unwrap();
    cancel_dispatch_and_reset_auto_queue_on_conn(
        &conn,
        "dispatch-821-nd",
        Some("turn_bridge_cancelled"),
    )
    .unwrap();

    let card_status: String = conn
        .query_row(
            "SELECT status FROM kanban_cards WHERE id = 'card-821-nd'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        card_status, "in_progress",
        "user cancel must NOT force the card into a terminal status"
    );
    assert_ne!(card_status, "done");
    assert_ne!(card_status, "review");
}

#[test]
fn terminal_card_cancels_live_dispatch_without_requeue() {
    let db = test_db();
    seed_user_cancel_fixture(&db, "card-821-tc", "dispatch-821-tc", "entry-821-tc");

    let conn = db.separate_conn().unwrap();
    let cancelled = cancel_active_dispatches_for_card_on_conn(
        &conn,
        "card-821-tc",
        Some("auto_cancelled_on_terminal_card"),
    )
    .unwrap();
    assert_eq!(cancelled, 1, "live dispatch must be cancelled");

    let dispatch_status: String = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = 'dispatch-821-tc'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(dispatch_status, "cancelled");

    let (entry_status, entry_dispatch_id): (String, Option<String>) = conn
        .query_row(
            "SELECT status, dispatch_id FROM auto_queue_entries WHERE id = 'entry-821-tc'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        entry_status, "dispatched",
        "terminal cancel must NOT reset the auto-queue entry to pending"
    );
    assert_eq!(
        entry_dispatch_id.as_deref(),
        Some("dispatch-821-tc"),
        "terminal cancel must leave the entry's dispatch pointer intact"
    );

    let pending_visible: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM auto_queue_entries e \
                 JOIN auto_queue_runs r ON e.run_id = r.id \
                 WHERE r.status = 'active' AND e.status = 'pending' \
                   AND e.kanban_card_id = 'card-821-tc'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        pending_visible, 0,
        "terminal cancel must not make the entry pick-able by the tick"
    );
}
