//! Central kanban state machine.
//!
//! ALL card status transitions MUST go through `transition_status()`.
//! This ensures hooks fire, auto-queue syncs, and notifications are sent.

use crate::db::Db;
use crate::engine::hooks::Hook;
use crate::engine::PolicyEngine;
use anyhow::Result;
use serde_json::json;

/// Transition a kanban card to a new status.
///
/// This is the ONLY correct way to change a card's status.
/// It handles:
/// 1. DB UPDATE with appropriate timestamp fields
/// 2. OnCardTransition hook
/// 3. OnReviewEnter hook (when → review)
/// 4. OnCardTerminal hook (when → done)
/// 5. auto_queue_entries sync (when → done)
pub fn transition_status(
    db: &Db,
    engine: &PolicyEngine,
    card_id: &str,
    new_status: &str,
) -> Result<TransitionResult> {
    let conn = db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

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

pub struct TransitionResult {
    pub changed: bool,
    pub from: String,
    pub to: String,
}

/// Fire hooks for a status transition that already happened in the DB.
/// Use this when the DB UPDATE was done elsewhere (e.g., update_card with mixed fields).
pub fn fire_transition_hooks(
    db: &Db,
    engine: &PolicyEngine,
    card_id: &str,
    from: &str,
    to: &str,
) {
    if from == to {
        return;
    }

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
                    "issue", "comment", &num.to_string(),
                    "--repo", &repo,
                    "--body", comment,
                ])
                .output();
        }
        _ => {}
    }
}
