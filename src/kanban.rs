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

    // Validate transition: done requires passing through review first
    // (unless transitioning from review itself or from blocked/pending_decision)
    if new_status == "done"
        && !matches!(
            old_status.as_str(),
            "review" | "blocked" | "pending_decision" | "done"
        )
    {
        tracing::warn!(
            "[kanban] Blocked invalid transition {} → done for card {} (must go through review)",
            old_status, card_id
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

    // Capture pre-hook dispatch ID to detect new dispatches created by hooks
    let pre_dispatch_id: Option<String> = db
        .lock()
        .ok()
        .and_then(|conn| {
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

    // Also check new dispatches created for OTHER cards (e.g., auto-queue dispatching next card)
    // We look for any pending dispatches created in the last few seconds
    let pending_dispatches: Vec<(String, String, String, String, Option<i64>)> = db
        .lock()
        .ok()
        .map(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT td.id, td.to_agent_id, kc.title, COALESCE(kc.github_issue_url, ''), kc.github_issue_number \
                     FROM task_dispatches td \
                     JOIN kanban_cards kc ON td.kanban_card_id = kc.id \
                     WHERE td.status = 'pending' AND td.created_at > datetime('now', '-5 seconds')"
                )
                .ok();
            stmt.as_mut()
                .and_then(|s| {
                    s.query_map([], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))
                    })
                    .ok()
                })
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
                .unwrap_or_default()
        })
        .unwrap_or_default();

    for (dispatch_id, agent_id, title, issue_url, issue_num) in pending_dispatches {
        // Send Discord notification via announce bot
        let config = crate::config::load_graceful();
        let token = match config
            .discord
            .bots
            .get("announce")
            .or_else(|| config.discord.bots.get("command"))
        {
            Some(bot) => bot.token.clone(),
            None => continue,
        };

        // Look up agent's channel
        let channel_id: Option<String> = db
            .lock()
            .ok()
            .and_then(|conn| {
                conn.query_row(
                    "SELECT discord_channel_id FROM agents WHERE id = ?1",
                    [&agent_id],
                    |row| row.get(0),
                )
                .ok()
            });

        let Some(channel_id) = channel_id else { continue };

        // Resolve channel name to ID
        let channel_num: Option<u64> = channel_id
            .parse()
            .ok()
            .or_else(|| crate::server::routes::dispatches::resolve_channel_alias_pub(&channel_id));

        let Some(ch) = channel_num else { continue };

        // Format message
        let issue_link = if let (Some(num), false) = (issue_num, issue_url.is_empty()) {
            format!("\n[{title} #{num}](<{issue_url}>)")
        } else {
            String::new()
        };
        let message = format!("DISPATCH:{dispatch_id} - {title}{issue_link}");

        // Synchronous HTTP send (we're not in async context)
        let _ = std::thread::spawn(move || {
            let rt = tokio::runtime::Handle::try_current();
            if let Ok(handle) = rt {
                handle.spawn(async move {
                    let client = reqwest::Client::new();
                    let _ = client
                        .post(format!("https://discord.com/api/v10/channels/{ch}/messages"))
                        .header("Authorization", format!("Bot {token}"))
                        .json(&serde_json::json!({"content": message}))
                        .send()
                        .await;
                });
            }
        });
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
