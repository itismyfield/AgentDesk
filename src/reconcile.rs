use anyhow::{Result, anyhow};
use rusqlite::Connection;
use serde_json::json;

use crate::{db::Db, dispatch, engine::PolicyEngine};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BootReconcileStats {
    pub stale_processing_outbox_reset: usize,
    pub stale_dispatch_reservations_cleared: usize,
    pub missing_notify_outbox_backfilled: usize,
    pub broken_auto_queue_entries_reset: usize,
    pub missing_review_dispatches_refired: usize,
}

impl BootReconcileStats {
    pub(crate) fn touched(&self) -> bool {
        self.stale_processing_outbox_reset > 0
            || self.stale_dispatch_reservations_cleared > 0
            || self.missing_notify_outbox_backfilled > 0
            || self.broken_auto_queue_entries_reset > 0
            || self.missing_review_dispatches_refired > 0
    }
}

pub(crate) fn reconcile_boot_db(conn: &Connection) -> Result<BootReconcileStats> {
    crate::server::routes::auto_queue::ensure_tables(conn);

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

    Ok(BootReconcileStats {
        stale_processing_outbox_reset,
        stale_dispatch_reservations_cleared,
        missing_notify_outbox_backfilled,
        broken_auto_queue_entries_reset,
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
            "[boot-reconcile] reset_processing={} cleared_reservations={} missing_notify={} broken_auto_queue={} refired_review={}",
            stats.stale_processing_outbox_reset,
            stats.stale_dispatch_reservations_cleared,
            stats.missing_notify_outbox_backfilled,
            stats.broken_auto_queue_entries_reset,
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
        reset += conn.execute(
            "UPDATE auto_queue_entries
             SET status = 'pending', dispatch_id = NULL, dispatched_at = NULL
             WHERE id = ?1 AND status = 'dispatched'",
            [&entry_id],
        )? as usize;
    }
    Ok(reset)
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
