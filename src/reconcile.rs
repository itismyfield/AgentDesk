use anyhow::{Result, anyhow};
use libsql_rusqlite::Connection;
use serde_json::json;

use crate::db::agents::load_agent_channel_bindings;
use crate::{db::Db, dispatch, engine::PolicyEngine};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BootReconcileStats {
    pub stale_processing_outbox_reset: usize,
    pub stale_dispatch_reservations_cleared: usize,
    pub missing_notify_outbox_backfilled: usize,
    pub broken_auto_queue_entries_reset: usize,
    pub stale_channel_thread_map_entries_cleared: usize,
    pub missing_review_dispatches_refired: usize,
}

impl BootReconcileStats {
    pub(crate) fn touched(&self) -> bool {
        self.stale_processing_outbox_reset > 0
            || self.stale_dispatch_reservations_cleared > 0
            || self.missing_notify_outbox_backfilled > 0
            || self.broken_auto_queue_entries_reset > 0
            || self.stale_channel_thread_map_entries_cleared > 0
            || self.missing_review_dispatches_refired > 0
    }
}

pub(crate) fn reconcile_boot_db(conn: &Connection) -> Result<BootReconcileStats> {
    let stale_processing_outbox_reset = conn
        .execute(
            "UPDATE dispatch_outbox SET status = 'pending' WHERE status = 'processing'",
            [],
        )
        .unwrap_or(0) as usize;
    let stale_dispatch_reservations_cleared = conn
        .execute(
            "DELETE FROM kv_meta WHERE key LIKE 'dispatch_reserving:%'",
            [],
        )
        .unwrap_or(0) as usize;

    let missing_notify_outbox_backfilled = backfill_missing_notify_outbox(conn)?;
    let broken_auto_queue_entries_reset = reset_broken_auto_queue_entries(conn)?;
    let stale_channel_thread_map_entries_cleared = cleanup_stale_channel_thread_map_entries(conn)?;

    Ok(BootReconcileStats {
        stale_processing_outbox_reset,
        stale_dispatch_reservations_cleared,
        missing_notify_outbox_backfilled,
        broken_auto_queue_entries_reset,
        stale_channel_thread_map_entries_cleared,
        missing_review_dispatches_refired: 0,
    })
}

pub(crate) fn reconcile_boot_runtime(db: &Db, engine: &PolicyEngine) -> Result<BootReconcileStats> {
    let mut stats = {
        let conn = db
            .lock()
            .map_err(|e| anyhow!("boot reconcile DB lock poisoned: {e}"))?;
        reconcile_boot_db(&conn)?
    };

    stats.missing_review_dispatches_refired = refire_missing_review_dispatches(db, engine)?;

    if stats.touched() {
        tracing::info!(
            "[boot-reconcile] reset_processing={} cleared_reservations={} missing_notify={} broken_auto_queue={} cleared_thread_map={} refired_review={}",
            stats.stale_processing_outbox_reset,
            stats.stale_dispatch_reservations_cleared,
            stats.missing_notify_outbox_backfilled,
            stats.broken_auto_queue_entries_reset,
            stats.stale_channel_thread_map_entries_cleared,
            stats.missing_review_dispatches_refired
        );
    }

    Ok(stats)
}

fn backfill_missing_notify_outbox(conn: &Connection) -> Result<usize> {
    let missing_rows: Vec<(String, String, String, String)> = conn
        .prepare(
            "SELECT td.id, td.to_agent_id, td.kanban_card_id, td.title
             FROM task_dispatches td
             WHERE td.status IN ('pending', 'dispatched')
               AND NOT EXISTS (
                 SELECT 1 FROM dispatch_outbox o
                 WHERE o.dispatch_id = td.id AND o.action = 'notify'
               )",
        )?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let mut inserted = 0usize;
    for (dispatch_id, agent_id, card_id, title) in missing_rows {
        if dispatch::ensure_dispatch_notify_outbox_on_conn(
            conn,
            &dispatch_id,
            &agent_id,
            &card_id,
            &title,
        )? {
            inserted += 1;
        }
    }
    Ok(inserted)
}

fn reset_broken_auto_queue_entries(conn: &Connection) -> Result<usize> {
    let broken_ids: Vec<String> = conn
        .prepare(
            "SELECT e.id
             FROM auto_queue_entries e
             LEFT JOIN task_dispatches td ON td.id = e.dispatch_id
             WHERE e.status = 'dispatched'
               AND (
                 e.dispatch_id IS NULL
                 OR TRIM(e.dispatch_id) = ''
                 OR td.id IS NULL
                 OR td.status IN ('cancelled', 'failed', 'completed')
               )",
        )?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let mut reset = 0usize;
    for entry_id in broken_ids {
        if crate::db::auto_queue::update_entry_status_on_conn(
            conn,
            &entry_id,
            crate::db::auto_queue::ENTRY_STATUS_PENDING,
            "boot_reconcile",
            &crate::db::auto_queue::EntryStatusUpdateOptions::default(),
        )
        .map_err(|error| match error {
            crate::db::auto_queue::EntryStatusUpdateError::Sql(sql) => anyhow!(sql),
            other => anyhow!(other.to_string()),
        })?
        .changed
        {
            reset += 1;
        }
    }
    Ok(reset)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThreadMapValidation {
    Valid,
    Stale,
    Unknown,
}

fn cleanup_stale_channel_thread_map_entries(conn: &Connection) -> Result<usize> {
    let token = crate::credential::read_bot_token("announce");
    cleanup_stale_channel_thread_map_entries_with(conn, |channel_id, thread_id| {
        validate_thread_parent_via_discord(token.as_deref(), channel_id, thread_id)
    })
}

fn cleanup_stale_channel_thread_map_entries_with<F>(
    conn: &Connection,
    mut validate_thread: F,
) -> Result<usize>
where
    F: FnMut(&str, &str) -> ThreadMapValidation,
{
    let rows: Vec<(String, Option<String>, Option<String>, String)> = conn
        .prepare(
            "SELECT id, assigned_agent_id, active_thread_id, channel_thread_map
             FROM kanban_cards
             WHERE channel_thread_map IS NOT NULL
               AND TRIM(channel_thread_map) != ''",
        )?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let mut cleared = 0usize;
    for (card_id, assigned_agent_id, active_thread_id, raw_map) in rows {
        let parsed = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&raw_map);
        let Ok(map) = parsed else {
            tracing::warn!(
                "[boot-reconcile] clearing malformed channel_thread_map for card {}",
                card_id
            );
            conn.execute(
                "UPDATE kanban_cards
                 SET channel_thread_map = NULL, active_thread_id = NULL
                 WHERE id = ?1",
                [&card_id],
            )?;
            cleared += 1;
            continue;
        };

        let allowed_channels = assigned_agent_id
            .as_deref()
            .and_then(|agent_id| load_agent_channel_bindings(conn, agent_id).ok().flatten())
            .map(|bindings| bindings.all_channels())
            .unwrap_or_default();
        let enforce_channel_bindings = !allowed_channels.is_empty();

        let mut kept = serde_json::Map::new();
        let mut removed_for_card = 0usize;
        for (channel_id, value) in map {
            let Some(thread_id) = value
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
            else {
                removed_for_card += 1;
                continue;
            };

            if enforce_channel_bindings
                && !allowed_channels.iter().any(|bound| bound == &channel_id)
            {
                removed_for_card += 1;
                continue;
            }

            match validate_thread(&channel_id, &thread_id) {
                ThreadMapValidation::Valid | ThreadMapValidation::Unknown => {
                    kept.insert(channel_id, serde_json::Value::String(thread_id));
                }
                ThreadMapValidation::Stale => {
                    removed_for_card += 1;
                }
            }
        }

        let new_active_thread_id = active_thread_id
            .as_ref()
            .filter(|active| {
                kept.values()
                    .any(|value| value.as_str() == Some(active.as_str()))
            })
            .cloned()
            .or_else(|| {
                kept.values()
                    .find_map(|value| value.as_str().map(str::to_string))
            });
        let new_map_json = if kept.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&kept)?)
        };
        let map_changed = removed_for_card > 0
            || new_map_json.as_deref() != Some(raw_map.as_str())
            || new_active_thread_id != active_thread_id;
        if map_changed {
            conn.execute(
                "UPDATE kanban_cards
                 SET channel_thread_map = ?1, active_thread_id = ?2
                 WHERE id = ?3",
                libsql_rusqlite::params![new_map_json, new_active_thread_id, card_id],
            )?;
        }
        cleared += removed_for_card;
    }

    Ok(cleared)
}

fn validate_thread_parent_via_discord(
    token: Option<&str>,
    expected_parent: &str,
    thread_id: &str,
) -> ThreadMapValidation {
    let Some(token) = token.map(str::trim).filter(|s| !s.is_empty()) else {
        return ThreadMapValidation::Unknown;
    };

    let url = format!("https://discord.com/api/v10/channels/{thread_id}");
    let resp = match ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .get(&url)
        .set("Authorization", &format!("Bot {token}"))
        .call()
    {
        Ok(resp) => resp,
        Err(ureq::Error::Status(404, _)) => return ThreadMapValidation::Stale,
        Err(ureq::Error::Status(status, _)) => {
            tracing::warn!(
                "[boot-reconcile] thread validation skipped for {thread_id}: HTTP {status}"
            );
            return ThreadMapValidation::Unknown;
        }
        Err(err) => {
            tracing::warn!(
                "[boot-reconcile] thread validation request failed for {thread_id}: {err}"
            );
            return ThreadMapValidation::Unknown;
        }
    };

    let body: serde_json::Value = match resp.into_json() {
        Ok(body) => body,
        Err(err) => {
            tracing::warn!(
                "[boot-reconcile] thread validation decode failed for {thread_id}: {err}"
            );
            return ThreadMapValidation::Unknown;
        }
    };

    match body.get("parent_id").and_then(|value| value.as_str()) {
        Some(parent_id) if parent_id == expected_parent => ThreadMapValidation::Valid,
        _ => ThreadMapValidation::Stale,
    }
}

fn refire_missing_review_dispatches(db: &Db, engine: &PolicyEngine) -> Result<usize> {
    crate::pipeline::ensure_loaded();

    let candidates: Vec<String> = {
        let conn = db
            .lock()
            .map_err(|e| anyhow!("boot reconcile DB lock poisoned: {e}"))?;
        let cards: Vec<(String, String, Option<String>, Option<String>)> = conn
            .prepare(
                "SELECT id, status, repo_id, assigned_agent_id
                 FROM kanban_cards
                 WHERE status NOT IN ('done', 'backlog', 'ready')",
            )?
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let mut candidates = Vec::new();
        for (card_id, status, repo_id, agent_id) in cards {
            let effective =
                crate::pipeline::resolve_for_card(&conn, repo_id.as_deref(), agent_id.as_deref());
            let is_review_state = effective.hooks_for_state(&status).map_or(false, |hooks| {
                hooks.on_enter.iter().any(|name| name == "OnReviewEnter")
            });
            if !is_review_state {
                continue;
            }

            let has_review_dispatch: bool = conn
                .query_row(
                    "SELECT COUNT(*) > 0 FROM task_dispatches
                     WHERE kanban_card_id = ?1
                       AND dispatch_type IN ('review', 'review-decision')
                       AND status IN ('pending', 'dispatched')",
                    [&card_id],
                    |row| row.get(0),
                )
                .unwrap_or(false);
            if !has_review_dispatch {
                candidates.push(card_id);
            }
        }
        candidates
    };

    let mut refired = 0usize;
    for card_id in candidates {
        if let Err(e) =
            engine.fire_hook_by_name_blocking("OnReviewEnter", json!({ "card_id": card_id }))
        {
            tracing::warn!(
                "[boot-reconcile] failed to re-fire OnReviewEnter for card {}: {e}",
                card_id
            );
            continue;
        }
        crate::kanban::drain_hook_side_effects(db, engine);

        let has_review_dispatch = db
            .lock()
            .map_err(|e| anyhow!("boot reconcile DB lock poisoned: {e}"))?
            .query_row(
                "SELECT COUNT(*) > 0 FROM task_dispatches
                 WHERE kanban_card_id = ?1
                   AND dispatch_type IN ('review', 'review-decision')
                   AND status IN ('pending', 'dispatched')",
                [&card_id],
                |row| row.get::<_, bool>(0),
            )
            .unwrap_or(false);
        if has_review_dispatch {
            refired += 1;
        } else {
            tracing::warn!(
                "[boot-reconcile] OnReviewEnter re-fired for card {} but no active review dispatch was created",
                card_id
            );
        }
    }

    Ok(refired)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_conn() -> libsql_rusqlite::Connection {
        let conn = libsql_rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        conn
    }

    #[test]
    fn boot_reconcile_thread_map_cleanup_removes_stale_entries_and_normalizes_active_thread() {
        let conn = test_conn();
        conn.execute(
            "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt)
             VALUES ('agent-1', 'Agent 1', '111', '222')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, assigned_agent_id, channel_thread_map, active_thread_id,
                created_at, updated_at
             ) VALUES (
                'card-thread-clean', 'Thread Clean', 'in_progress', 'agent-1',
                '{\"111\":\"thread-ok\",\"222\":\"thread-mismatch\",\"333\":\"thread-offbind\",\"444\":123}',
                'thread-mismatch', datetime('now'), datetime('now')
             )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, assigned_agent_id, channel_thread_map, active_thread_id,
                created_at, updated_at
             ) VALUES (
                'card-thread-gone', 'Thread Gone', 'review', 'agent-1',
                '{\"111\":\"thread-gone\"}', 'thread-gone', datetime('now'), datetime('now')
             )",
            [],
        )
        .unwrap();

        let cleared =
            cleanup_stale_channel_thread_map_entries_with(&conn, |channel_id, thread_id| {
                match (channel_id, thread_id) {
                    ("111", "thread-ok") => ThreadMapValidation::Valid,
                    ("222", "thread-mismatch") => ThreadMapValidation::Stale,
                    ("111", "thread-gone") => ThreadMapValidation::Stale,
                    _ => ThreadMapValidation::Unknown,
                }
            })
            .unwrap();

        assert_eq!(cleared, 4);

        let (map, active_thread_id): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT channel_thread_map, active_thread_id
                 FROM kanban_cards WHERE id = 'card-thread-clean'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(map.as_deref(), Some("{\"111\":\"thread-ok\"}"));
        assert_eq!(active_thread_id.as_deref(), Some("thread-ok"));

        let (gone_map, gone_active): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT channel_thread_map, active_thread_id
                 FROM kanban_cards WHERE id = 'card-thread-gone'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!(gone_map.is_none());
        assert!(gone_active.is_none());
    }

    #[test]
    fn boot_reconcile_thread_map_cleanup_preserves_entries_on_unknown_validation() {
        let conn = test_conn();
        conn.execute(
            "INSERT INTO agents (id, name, discord_channel_id)
             VALUES ('agent-1', 'Agent 1', '111')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, assigned_agent_id, channel_thread_map, active_thread_id,
                created_at, updated_at
             ) VALUES (
                'card-thread-unknown', 'Thread Unknown', 'review', 'agent-1',
                '{\"111\":\"thread-unknown\"}', 'thread-unknown', datetime('now'), datetime('now')
             )",
            [],
        )
        .unwrap();

        let cleared =
            cleanup_stale_channel_thread_map_entries_with(&conn, |_channel_id, _thread_id| {
                ThreadMapValidation::Unknown
            })
            .unwrap();
        assert_eq!(cleared, 0);

        let (map, active_thread_id): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT channel_thread_map, active_thread_id
                 FROM kanban_cards WHERE id = 'card-thread-unknown'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(map.as_deref(), Some("{\"111\":\"thread-unknown\"}"));
        assert_eq!(active_thread_id.as_deref(), Some("thread-unknown"));
    }
}
