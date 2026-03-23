use anyhow::Result;
use serde_json::json;

use crate::db::Db;
use crate::engine::{PolicyEngine, hooks::Hook};

/// Create a new dispatch for a kanban card.
///
/// - Inserts a record into `task_dispatches`
/// - Updates `kanban_cards.latest_dispatch_id` and sets status to "requested"
/// - Fires `OnCardTransition` hook (old_status -> requested)
///
/// Returns the dispatch ID.
pub fn create_dispatch(
    db: &Db,
    engine: &PolicyEngine,
    kanban_card_id: &str,
    to_agent_id: &str,
    dispatch_type: &str,
    title: &str,
    context: &serde_json::Value,
) -> Result<serde_json::Value> {
    let dispatch_id = uuid::Uuid::new_v4().to_string();

    // For review dispatches, inject reviewed_commit (HEAD) as server-side source of truth
    let context_str = if dispatch_type == "review" {
        let mut ctx_val = if context.is_object() {
            context.clone()
        } else {
            json!({})
        };
        if let Some(obj) = ctx_val.as_object_mut() {
            if !obj.contains_key("reviewed_commit") {
                let repo_dir = std::env::var("AGENTDESK_REPO_DIR")
                    .unwrap_or_else(|_| format!("{}/AgentDesk", env!("HOME")));
                if let Some(commit) = std::process::Command::new("git")
                    .args(["rev-parse", "HEAD"])
                    .current_dir(&repo_dir)
                    .output()
                    .ok()
                    .filter(|o| o.status.success())
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                {
                    obj.insert("reviewed_commit".to_string(), json!(commit));
                }
            }
        }
        serde_json::to_string(&ctx_val)?
    } else {
        serde_json::to_string(context)?
    };

    let conn = db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock error: {e}"))?;

    // Get current card status for the transition hook
    let old_status: String = conn
        .query_row(
            "SELECT status FROM kanban_cards WHERE id = ?1",
            [kanban_card_id],
            |row| row.get(0),
        )
        .map_err(|e| anyhow::anyhow!("Card not found: {e}"))?;

    // Insert dispatch
    conn.execute(
        "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, 'pending', ?5, ?6, datetime('now'), datetime('now'))",
        rusqlite::params![dispatch_id, kanban_card_id, to_agent_id, dispatch_type, title, context_str],
    )?;

    // Update kanban card — rework/review dispatches keep current status
    let is_review_type = dispatch_type == "review"
        || dispatch_type == "review-decision"
        || dispatch_type == "rework";
    if is_review_type {
        conn.execute(
            "UPDATE kanban_cards SET latest_dispatch_id = ?1, updated_at = datetime('now') WHERE id = ?2",
            rusqlite::params![dispatch_id, kanban_card_id],
        )?;
    } else {
        conn.execute(
            "UPDATE kanban_cards SET latest_dispatch_id = ?1, status = 'requested', requested_at = datetime('now'), updated_at = datetime('now') WHERE id = ?2",
            rusqlite::params![dispatch_id, kanban_card_id],
        )?;
    }

    // Read back the dispatch
    let dispatch = query_dispatch_row(&conn, &dispatch_id)?;
    drop(conn);

    // Fire OnCardTransition hook
    let _ = engine.fire_hook(
        Hook::OnCardTransition,
        json!({
            "card_id": kanban_card_id,
            "from": old_status,
            "to": "requested",
        }),
    );

    Ok(dispatch)
}

/// Complete a dispatch, setting its status to "completed" with the given result.
/// Fires `OnDispatchCompleted` hook.
pub fn complete_dispatch(
    db: &Db,
    engine: &PolicyEngine,
    dispatch_id: &str,
    result: &serde_json::Value,
) -> Result<serde_json::Value> {
    let result_str = serde_json::to_string(result)?;

    let conn = db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock error: {e}"))?;

    let changed = conn.execute(
        "UPDATE task_dispatches SET status = 'completed', result = ?1, updated_at = datetime('now') WHERE id = ?2 AND status != 'completed'",
        rusqlite::params![result_str, dispatch_id],
    )?;

    if changed == 0 {
        // Either not found or already completed — skip hook firing
        let exists: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM task_dispatches WHERE id = ?1",
                [dispatch_id],
                |row| row.get(0),
            )
            .unwrap_or(false);
        if exists {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ⏭ complete_dispatch: {dispatch_id} already completed, skipping hooks");
            let dispatch = query_dispatch_row(&conn, dispatch_id)?;
            drop(conn);
            return Ok(dispatch);
        }
        return Err(anyhow::anyhow!("Dispatch not found: {dispatch_id}"));
    }

    let dispatch = query_dispatch_row(&conn, dispatch_id)?;

    let kanban_card_id: Option<String> = conn
        .query_row(
            "SELECT kanban_card_id FROM task_dispatches WHERE id = ?1",
            [dispatch_id],
            |row| row.get(0),
        )
        .ok();

    // Capture card status BEFORE hooks fire (so we can detect changes after)
    let old_status: String = kanban_card_id
        .as_ref()
        .and_then(|cid| {
            conn.query_row(
                "SELECT status FROM kanban_cards WHERE id = ?1",
                [cid],
                |row| row.get(0),
            )
            .ok()
        })
        .unwrap_or_default();

    drop(conn);

    // Fire OnDispatchCompleted hook
    let _ = engine.fire_hook(
        Hook::OnDispatchCompleted,
        json!({
            "dispatch_id": dispatch_id,
            "kanban_card_id": kanban_card_id,
            "result": result,
        }),
    );

    // After OnDispatchCompleted, policies may have changed the card status via kanban.setStatus.
    // Since setStatus fires hooks internally (via fire_transition_hooks in the wrapper),
    // we only need to check for status changes made by the wrapper that need post-processing.
    // The kanban.setStatus wrapper handles OnCardTransition, OnCardTerminal, OnReviewEnter.
    // However, if the policy used setStatus, the hooks already fired during the hook execution.
    // We still check for review/done to handle edge cases where hooks create new dispatches.
    // After OnDispatchCompleted, policies change card status via kanban.setStatus (DB only).
    // We need to fire transition hooks for the new status since setStatus can't call
    // engine.fire_hook (it runs inside a hook, no engine reference).
    if let Some(ref card_id) = kanban_card_id {
        let new_status: Option<String> = {
            let conn = db.lock().map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
            conn.query_row(
                "SELECT status FROM kanban_cards WHERE id = ?1",
                [card_id],
                |row| row.get(0),
            )
            .ok()
        };
        if let Some(ref new_s) = new_status {
            if new_s != &old_status {
                crate::kanban::fire_transition_hooks(db, engine, card_id, &old_status, new_s);
            }
        }
    }

    Ok(dispatch)
}

/// Read a single dispatch row as JSON.
pub fn query_dispatch_row(
    conn: &rusqlite::Connection,
    dispatch_id: &str,
) -> Result<serde_json::Value> {
    conn.query_row(
        "SELECT id, kanban_card_id, from_agent_id, to_agent_id, dispatch_type, status, title, context, result, parent_dispatch_id, chain_depth, created_at, updated_at
         FROM task_dispatches WHERE id = ?1",
        [dispatch_id],
        |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "kanban_card_id": row.get::<_, Option<String>>(1)?,
                "from_agent_id": row.get::<_, Option<String>>(2)?,
                "to_agent_id": row.get::<_, Option<String>>(3)?,
                "dispatch_type": row.get::<_, Option<String>>(4)?,
                "status": row.get::<_, String>(5)?,
                "title": row.get::<_, Option<String>>(6)?,
                "context": row.get::<_, Option<String>>(7)?.and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()),
                "result": row.get::<_, Option<String>>(8)?.and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()),
                "parent_dispatch_id": row.get::<_, Option<String>>(9)?,
                "chain_depth": row.get::<_, i64>(10)?,
                "created_at": row.get::<_, String>(11)?,
                "updated_at": row.get::<_, String>(12)?,
            }))
        },
    )
    .map_err(|e| anyhow::anyhow!("Dispatch query error: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn test_db() -> Db {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        Arc::new(Mutex::new(conn))
    }

    fn test_engine(db: &Db) -> PolicyEngine {
        let config = crate::config::Config::default();
        PolicyEngine::new(&config, db.clone()).unwrap()
    }

    fn seed_card(db: &Db, card_id: &str, status: &str) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, created_at, updated_at) VALUES (?1, 'Test Card', ?2, datetime('now'), datetime('now'))",
            rusqlite::params![card_id, status],
        )
        .unwrap();
    }

    #[test]
    fn create_dispatch_inserts_and_updates_card() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-1", "ready");

        let dispatch = create_dispatch(
            &db,
            &engine,
            "card-1",
            "agent-1",
            "implementation",
            "Do the thing",
            &json!({"key": "value"}),
        )
        .unwrap();

        assert_eq!(dispatch["status"], "pending");
        assert_eq!(dispatch["kanban_card_id"], "card-1");
        assert_eq!(dispatch["to_agent_id"], "agent-1");
        assert_eq!(dispatch["dispatch_type"], "implementation");
        assert_eq!(dispatch["title"], "Do the thing");

        // Card should be updated
        let conn = db.lock().unwrap();
        let (card_status, latest_dispatch_id): (String, String) = conn
            .query_row(
                "SELECT status, latest_dispatch_id FROM kanban_cards WHERE id = 'card-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(card_status, "requested");
        assert_eq!(latest_dispatch_id, dispatch["id"].as_str().unwrap());
    }

    #[test]
    fn create_dispatch_for_nonexistent_card_fails() {
        let db = test_db();
        let engine = test_engine(&db);

        let result = create_dispatch(
            &db,
            &engine,
            "nonexistent",
            "agent-1",
            "implementation",
            "title",
            &json!({}),
        );
        assert!(result.is_err());
    }

    #[test]
    fn complete_dispatch_updates_status() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-2", "ready");

        let dispatch = create_dispatch(
            &db,
            &engine,
            "card-2",
            "agent-1",
            "implementation",
            "title",
            &json!({}),
        )
        .unwrap();
        let dispatch_id = dispatch["id"].as_str().unwrap().to_string();

        let completed =
            complete_dispatch(&db, &engine, &dispatch_id, &json!({"output": "done"})).unwrap();

        assert_eq!(completed["status"], "completed");
    }

    #[test]
    fn complete_dispatch_nonexistent_fails() {
        let db = test_db();
        let engine = test_engine(&db);

        let result = complete_dispatch(&db, &engine, "nonexistent", &json!({}));
        assert!(result.is_err());
    }
}
