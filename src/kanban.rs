//! Central kanban state machine.
//!
//! ALL card status transitions MUST go through `transition_status()`.
//! This ensures hooks fire, auto-queue syncs, and notifications are sent.
//!
//! ## Transition Rules
//!
//! | From | To | Requires |
//! |------|----|----------|
//! | backlog | ready | Free (no dispatch needed) |
//! | ready | backlog | Free (no dispatch needed) |
//! | ready | requested | Active dispatch (pending/dispatched) |
//! | requested | in_progress | Active dispatch + session working acknowledgement |
//! | in_progress | review | Dispatch completion triggers this |
//! | review | done | Review pass verdict |
//! | review | in_progress | Rework dispatch (review-decision accept) |
//! | * | pending_decision | Timeout/gate failure (force via policy) |
//! | * | blocked | Agent signal or timeout (force via policy) |
//!
//! ## Dispatch Acknowledgement
//!
//! `requested → in_progress` requires an explicit dispatch acknowledgement:
//! 1. Session must have `active_dispatch_id` set (links session to dispatch)
//! 2. Session status must change to `working` (triggers onSessionStatusChange)
//! 3. Policy checks `dispatch_type` is `implementation` or `rework` (not review)
//! 4. Policy checks `card.status === "requested"` (prevents re-entry)

use crate::db::Db;
use crate::engine::PolicyEngine;
use crate::engine::hooks::Hook;
use anyhow::Result;
use serde_json::json;

/// Transition a kanban card to a new status.
///
/// This is the ONLY correct way to change a card's status.
/// It handles:
/// 1. Dispatch validation (C: dispatch required for non-free transitions)
/// 2. DB UPDATE with appropriate timestamp fields
/// 3. Audit logging (D: all transitions logged)
/// 4. OnCardTransition hook
/// 5. OnReviewEnter hook (when → review)
/// 6. OnCardTerminal hook (when → done)
/// 7. auto_queue_entries sync (when → done)
///
/// `source`: who initiated the transition (e.g., "api", "policy", "pmd")
/// `force`: PMD-only override to bypass dispatch validation
pub fn transition_status(
    db: &Db,
    engine: &PolicyEngine,
    card_id: &str,
    new_status: &str,
) -> Result<TransitionResult> {
    transition_status_with_opts(db, engine, card_id, new_status, "system", false)
}

/// Full transition with source tracking and force override.
pub fn transition_status_with_opts(
    db: &Db,
    engine: &PolicyEngine,
    card_id: &str,
    new_status: &str,
    source: &str,
    force: bool,
) -> Result<TransitionResult> {
    let conn = db.lock().map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    // Get current status
    let old_status: String = conn
        .query_row(
            "SELECT status FROM kanban_cards WHERE id = ?1",
            [card_id],
            |row| row.get(0),
        )
        .map_err(|_| anyhow::anyhow!("card not found: {card_id}"))?;

    if old_status == new_status {
        return Ok(TransitionResult {
            changed: false,
            from: old_status,
            to: new_status.to_string(),
        });
    }

    // ── C: Dispatch validation ──
    // Free transitions: backlog ↔ ready (no dispatch needed)
    let free_transition = matches!(
        (old_status.as_str(), new_status),
        ("backlog", "ready") | ("ready", "backlog")
    );

    if !free_transition && !force {
        // Check for active dispatch — only pending/dispatched count as active.
        // Completed dispatches are historical and must NOT authorize new transitions.
        let has_active_dispatch: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM task_dispatches \
                 WHERE kanban_card_id = ?1 AND status IN ('pending', 'dispatched')",
                [card_id],
                |row| row.get(0),
            )
            .unwrap_or(false);

        if !has_active_dispatch {
            log_audit(
                &conn,
                card_id,
                &old_status,
                new_status,
                source,
                "BLOCKED: no active dispatch",
            );
            tracing::warn!(
                "[kanban] Blocked transition {} → {} for card {} (no active dispatch, source: {})",
                old_status,
                new_status,
                card_id,
                source
            );
            notify_pmd_violation(
                &conn,
                card_id,
                &old_status,
                new_status,
                source,
                "no active dispatch",
            );
            return Err(anyhow::anyhow!(
                "Status transition {} → {} requires an active dispatch (pending/dispatched). Completed dispatches do not qualify. Use dispatch lifecycle or PMD force override.",
                old_status,
                new_status
            ));
        }
    }

    // Validate transition: done requires passing through review first
    if new_status == "done"
        && !force
        && !matches!(
            old_status.as_str(),
            "review" | "blocked" | "pending_decision" | "done"
        )
    {
        log_audit(
            &conn,
            card_id,
            &old_status,
            new_status,
            source,
            "BLOCKED: review required",
        );
        tracing::warn!(
            "[kanban] Blocked invalid transition {} → done for card {} (must go through review)",
            old_status,
            card_id
        );
        notify_pmd_violation(
            &conn,
            card_id,
            &old_status,
            new_status,
            source,
            "review required",
        );
        return Err(anyhow::anyhow!(
            "Cannot transition from {} to done directly. Must go through review first.",
            old_status
        ));
    }

    // Build UPDATE with appropriate extra fields
    let extra = match new_status {
        "in_progress" => ", started_at = COALESCE(started_at, datetime('now'))",
        "requested" => ", requested_at = datetime('now')",
        "done" => ", completed_at = datetime('now'), review_status = NULL",
        _ => "",
    };
    let sql = format!(
        "UPDATE kanban_cards SET status = ?1, updated_at = datetime('now'){extra} WHERE id = ?2"
    );
    conn.execute(&sql, rusqlite::params![new_status, card_id])?;

    // ── D: Audit log ──
    log_audit(&conn, card_id, &old_status, new_status, source, "OK");

    // Sync auto_queue_entries on terminal status
    if new_status == "done" {
        conn.execute(
            "UPDATE auto_queue_entries SET status = 'done', completed_at = datetime('now') \
             WHERE kanban_card_id = ?1 AND status = 'dispatched'",
            [card_id],
        )
        .ok();
    }

    drop(conn);

    // GitHub auto-sync (close on done, comment on review)
    github_sync_on_transition(db, card_id, new_status);

    // Fire hooks
    let _ = engine.fire_hook(
        Hook::OnCardTransition,
        json!({
            "card_id": card_id,
            "from": old_status,
            "to": new_status,
        }),
    );

    if new_status == "done" {
        let _ = engine.fire_hook(
            Hook::OnCardTerminal,
            json!({
                "card_id": card_id,
                "status": "done",
            }),
        );
    }

    if new_status == "review" {
        let _ = engine.fire_hook(
            Hook::OnReviewEnter,
            json!({
                "card_id": card_id,
                "from": old_status,
            }),
        );
    }

    Ok(TransitionResult {
        changed: true,
        from: old_status,
        to: new_status.to_string(),
    })
}

#[derive(Debug)]
pub struct TransitionResult {
    pub changed: bool,
    pub from: String,
    pub to: String,
}

/// Fire hooks for a status transition that already happened in the DB.
/// Use this when the DB UPDATE was done elsewhere (e.g., update_card with mixed fields).
pub fn fire_transition_hooks(db: &Db, engine: &PolicyEngine, card_id: &str, from: &str, to: &str) {
    if from == to {
        return;
    }

    // Audit log
    if let Ok(conn) = db.lock() {
        log_audit(&conn, card_id, from, to, "hook", "OK");
    }

    // Capture pre-hook dispatch ID to detect new dispatches created by hooks
    let pre_dispatch_id: Option<String> = db.lock().ok().and_then(|conn| {
        conn.query_row(
            "SELECT latest_dispatch_id FROM kanban_cards WHERE id = ?1",
            [card_id],
            |row| row.get(0),
        )
        .ok()
        .flatten()
    });

    // Sync auto_queue_entries + GitHub on terminal status
    if to == "done" {
        if let Ok(conn) = db.lock() {
            conn.execute(
                "UPDATE auto_queue_entries SET status = 'done', completed_at = datetime('now') \
                 WHERE kanban_card_id = ?1 AND status = 'dispatched'",
                [card_id],
            )
            .ok();
        }
    }

    // GitHub auto-sync
    github_sync_on_transition(db, card_id, to);

    let _ = engine.fire_hook(
        Hook::OnCardTransition,
        json!({
            "card_id": card_id,
            "from": from,
            "to": to,
        }),
    );

    if to == "done" {
        let _ = engine.fire_hook(
            Hook::OnCardTerminal,
            json!({
                "card_id": card_id,
                "status": "done",
            }),
        );
    }

    if to == "review" {
        let _ = engine.fire_hook(
            Hook::OnReviewEnter,
            json!({
                "card_id": card_id,
                "from": from,
            }),
        );
    }

    // After all hooks, check if a new dispatch was created (by onCardTerminal, onReviewEnter, etc.)
    // and send Discord notification. This handles auto-queue's next dispatch creation.
    notify_new_dispatches_after_hooks(db, card_id, pre_dispatch_id.as_deref());
}

/// Check if hooks created new dispatches and send Discord notifications.
fn notify_new_dispatches_after_hooks(db: &Db, card_id: &str, pre_dispatch_id: Option<&str>) {
    let info: Option<(String, String, String, Option<i64>)> = db
        .lock()
        .ok()
        .and_then(|conn| {
            conn.query_row(
                "SELECT kc.assigned_agent_id, kc.title, COALESCE(kc.latest_dispatch_id, ''), kc.github_issue_number \
                 FROM kanban_cards kc WHERE kc.id = ?1",
                [card_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .ok()
        });

    let Some((agent_id, title, new_dispatch_id, issue_number)) = info else {
        return;
    };

    // Only notify if a NEW dispatch was created
    if new_dispatch_id.is_empty() || Some(new_dispatch_id.as_str()) == pre_dispatch_id {
        return;
    }

    // Check for any new pending dispatches created in the last few seconds
    let pending_dispatches: Vec<(String, String, String, String, String, Option<i64>)> = db
        .lock()
        .ok()
        .map(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT td.id, td.to_agent_id, td.dispatch_type, kc.title, \
                     COALESCE(kc.github_issue_url, ''), kc.github_issue_number \
                     FROM task_dispatches td \
                     JOIN kanban_cards kc ON td.kanban_card_id = kc.id \
                     WHERE td.status = 'pending' AND td.created_at > datetime('now', '-5 seconds')",
                )
                .ok();
            stmt.as_mut()
                .and_then(|s| {
                    s.query_map([], |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                        ))
                    })
                    .ok()
                })
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
                .unwrap_or_default()
        })
        .unwrap_or_default();

    if pending_dispatches.is_empty() {
        return;
    }

    // Delegate all dispatch types to send_dispatch_to_discord which handles:
    // - Thread creation/reuse
    // - dispatch_notified guard (dedup)
    // - Proper channel routing (primary vs alt for review)
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        let db_clone = db.clone();
        for (dispatch_id, agent_id, _, title, _, _) in pending_dispatches {
            let db_c = db_clone.clone();
            let card_id = card_id.to_string();
            handle.spawn(async move {
                crate::server::routes::dispatches::send_dispatch_to_discord(
                    &db_c, &agent_id, &title, &card_id, &dispatch_id,
                )
                .await;
            });
        }
    }
}

/// Sync GitHub issue state when kanban card transitions.
fn github_sync_on_transition(db: &Db, card_id: &str, new_status: &str) {
    let info: Option<(String, Option<i64>)> = db
        .lock()
        .ok()
        .and_then(|conn| {
            conn.query_row(
                "SELECT COALESCE(github_issue_url, ''), github_issue_number FROM kanban_cards WHERE id = ?1",
                [card_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .ok()
        });

    let Some((issue_url, issue_number)) = info else {
        return;
    };
    if issue_url.is_empty() {
        return;
    }

    let repo = match issue_url
        .strip_prefix("https://github.com/")
        .and_then(|s| s.find("/issues/").map(|i| &s[..i]))
    {
        Some(r) => r.to_string(),
        None => return,
    };
    let Some(num) = issue_number else { return };

    match new_status {
        "done" => {
            let _ = std::process::Command::new("gh")
                .args(["issue", "close", &num.to_string(), "--repo", &repo])
                .output();
        }
        "review" => {
            let comment = "🔍 칸반 상태: **review** (카운터모델 리뷰 진행 중)";
            let _ = std::process::Command::new("gh")
                .args([
                    "issue",
                    "comment",
                    &num.to_string(),
                    "--repo",
                    &repo,
                    "--body",
                    comment,
                ])
                .output();
        }
        _ => {}
    }
}

/// Send a violation alert to the PMD/kanban-manager channel via announce bot.
fn notify_pmd_violation(
    conn: &rusqlite::Connection,
    card_id: &str,
    from: &str,
    to: &str,
    source: &str,
    reason: &str,
) {
    // Look up card title for the notification
    let title: String = conn
        .query_row(
            "SELECT COALESCE(title, id) FROM kanban_cards WHERE id = ?1",
            [card_id],
            |row| row.get(0),
        )
        .unwrap_or_else(|_| card_id.to_string());

    // Read kanban_manager_channel_id from kv_meta
    let km_channel: Option<String> = conn
        .query_row(
            "SELECT value FROM kv_meta WHERE key = 'kanban_manager_channel_id'",
            [],
            |row| row.get(0),
        )
        .ok();

    let Some(km_channel) = km_channel else {
        tracing::debug!(
            "[kanban] No kanban_manager_channel_id configured, skipping violation alert"
        );
        return;
    };
    let Some(channel_num) = km_channel.parse::<u64>().ok() else {
        tracing::warn!("[kanban] Invalid kanban_manager_channel_id: {km_channel}");
        return;
    };

    let message = format!(
        "⚠️ **칸반 위반 감지**\n\n\
         카드: {title}\n\
         시도: {from} → {to}\n\
         차단 사유: {reason}\n\
         호출자: {source}\n\
         카드 ID: {card_id}"
    );

    let token = match crate::credential::read_bot_token("announce") {
        Some(t) => t,
        None => {
            tracing::debug!("[kanban] No announce bot token, skipping violation alert");
            return;
        }
    };

    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move {
            let client = reqwest::Client::new();
            let _ = client
                .post(format!(
                    "https://discord.com/api/v10/channels/{channel_num}/messages"
                ))
                .header("Authorization", format!("Bot {}", token))
                .json(&serde_json::json!({"content": message}))
                .send()
                .await;
        });
    }
}

/// Log a kanban state transition to audit_logs table.
fn log_audit(
    conn: &rusqlite::Connection,
    card_id: &str,
    from: &str,
    to: &str,
    source: &str,
    result: &str,
) {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS kanban_audit_logs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            card_id TEXT,
            from_status TEXT,
            to_status TEXT,
            source TEXT,
            result TEXT,
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP
        )",
    )
    .ok();
    conn.execute(
        "INSERT INTO kanban_audit_logs (card_id, from_status, to_status, source, result) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![card_id, from, to, source, result],
    )
    .ok();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS audit_logs (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            entity_type TEXT,
            entity_id   TEXT,
            action      TEXT,
            timestamp   DATETIME DEFAULT CURRENT_TIMESTAMP,
            actor       TEXT
        )",
    )
    .ok();
    conn.execute(
        "INSERT INTO audit_logs (entity_type, entity_id, action, actor)
         VALUES ('kanban_card', ?1, ?2, ?3)",
        rusqlite::params![card_id, format!("{from}->{to} ({result})"), source],
    )
    .ok();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    fn test_db() -> Db {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        Arc::new(Mutex::new(conn))
    }

    fn test_engine(db: &Db) -> PolicyEngine {
        let mut config = crate::config::Config::default();
        config.policies.dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("policies");
        config.policies.hot_reload = false;
        PolicyEngine::new(&config, db.clone()).unwrap()
    }

    fn seed_card(db: &Db, card_id: &str, status: &str) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('agent-1', 'Agent 1', '123', '456')",
            [],
        ).ok(); // ignore if already exists
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, created_at, updated_at)
             VALUES (?1, 'Test Card', ?2, 'agent-1', datetime('now'), datetime('now'))",
            rusqlite::params![card_id, status],
        ).unwrap();
    }

    fn seed_dispatch(db: &Db, card_id: &str, dispatch_status: &str) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at)
             VALUES (?1, ?2, 'agent-1', 'implementation', ?3, 'Test Dispatch', datetime('now'), datetime('now'))",
            rusqlite::params![format!("dispatch-{}-{}", card_id, dispatch_status), card_id, dispatch_status],
        ).unwrap();
    }

    #[test]
    fn completed_dispatch_only_does_not_authorize_transition() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-completed", "requested");
        seed_dispatch(&db, "card-completed", "completed");

        let result = transition_status(&db, &engine, "card-completed", "in_progress");
        assert!(
            result.is_err(),
            "completed dispatch should NOT authorize transition"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("active dispatch"),
            "error should mention active dispatch"
        );
    }

    #[test]
    fn pending_dispatch_authorizes_transition() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-pending", "requested");
        seed_dispatch(&db, "card-pending", "pending");

        let result = transition_status(&db, &engine, "card-pending", "in_progress");
        assert!(
            result.is_ok(),
            "pending dispatch should authorize transition"
        );
    }

    #[test]
    fn dispatched_status_authorizes_transition() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-dispatched", "requested");
        seed_dispatch(&db, "card-dispatched", "dispatched");

        let result = transition_status(&db, &engine, "card-dispatched", "in_progress");
        assert!(
            result.is_ok(),
            "dispatched status should authorize transition"
        );
    }

    #[test]
    fn no_dispatch_blocks_non_free_transition() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-none", "requested");
        // No dispatch at all

        let result = transition_status(&db, &engine, "card-none", "in_progress");
        assert!(result.is_err(), "no dispatch should block transition");
    }

    #[test]
    fn free_transition_works_without_dispatch() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-free", "backlog");

        let result = transition_status(&db, &engine, "card-free", "ready");
        assert!(
            result.is_ok(),
            "backlog → ready should work without dispatch"
        );
    }

    #[test]
    fn force_overrides_dispatch_check() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-force", "requested");
        // No dispatch, but force=true

        let result =
            transition_status_with_opts(&db, &engine, "card-force", "in_progress", "pmd", true);
        assert!(result.is_ok(), "force=true should bypass dispatch check");
    }
}
