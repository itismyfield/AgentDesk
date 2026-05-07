use anyhow::Result;
use serde_json::json;
use sqlx::{PgPool, Row as SqlxRow};

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
use crate::db::Db;

mod dispatch_channel;
mod dispatch_context;
mod dispatch_create;
mod dispatch_query;
mod dispatch_status;
mod dispatch_summary;
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) mod test_support;
mod types;

pub(crate) use dispatch_channel::dispatch_destination_provider_override;
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
use dispatch_channel::provider_from_channel_suffix;
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) use dispatch_context::resolve_card_worktree_sqlite_test;
#[allow(unused_imports)]
pub(crate) use dispatch_context::{
    DispatchSessionStrategy, REVIEW_QUALITY_CHECKLIST, REVIEW_QUALITY_SCOPE_REMINDER,
    REVIEW_VERDICT_IMPROVE_GUIDANCE, commit_belongs_to_card_issue, commit_belongs_to_card_issue_pg,
    dispatch_session_strategy_from_context, dispatch_type_force_new_session_default,
    dispatch_type_requires_fresh_worktree, dispatch_type_session_strategy_default,
    dispatch_type_uses_thread_routing, ensure_card_worktree, inject_review_dispatch_identifiers,
    resolve_card_worktree,
};
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
use dispatch_context::{
    ReviewTargetTrust, TargetRepoSource, build_review_context_sqlite_test,
    inject_review_merge_base_context,
};
#[allow(unused_imports)]
pub(crate) use dispatch_create::apply_dispatch_attached_intents_on_pg_tx;
#[allow(unused_imports)]
pub use dispatch_create::{
    create_dispatch, create_dispatch_core, create_dispatch_core_with_id,
    create_dispatch_core_with_id_and_options, create_dispatch_core_with_options,
    create_dispatch_pg_only, create_dispatch_with_options, create_dispatch_with_options_pg_only,
};
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) use dispatch_create::{
    create_dispatch_record_sqlite_test, create_dispatch_record_with_id_sqlite_test,
};
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
#[allow(unused_imports)]
pub use dispatch_query::query_dispatch_row;
pub(crate) use dispatch_query::query_dispatch_row_pg;
#[allow(unused_imports)]
pub(crate) use dispatch_status::set_dispatch_status_without_queue_sync_with_backends;
#[allow(unused_imports)]
pub use dispatch_status::{
    complete_dispatch, finalize_dispatch, finalize_dispatch_with_backends,
    load_dispatch_row_pg_first, load_dispatch_row_with_backends, mark_dispatch_completed_pg_first,
    set_dispatch_status_pg_first, set_dispatch_status_with_backends,
};
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
#[allow(unused_imports)]
pub(crate) use dispatch_status::{
    ensure_dispatch_notify_outbox_on_conn, ensure_dispatch_status_reaction_outbox_on_conn,
    record_dispatch_status_event_on_conn, set_dispatch_status_on_conn,
    set_dispatch_status_without_queue_sync_on_conn,
};
pub(crate) use dispatch_summary::{summarize_dispatch_from_text, summarize_dispatch_result};
pub use types::DispatchCreateOptions;

/// Cancel reasons that represent an explicit operator stop, not a system
/// retry or supersession (#815). When we see one of these reasons we
/// preserve the user's intent by moving the linked auto-queue entry to a
/// non-dispatchable terminal status instead of resetting it back to
/// `pending`, which would let the next tick re-dispatch the same work.
const USER_CANCEL_REASONS: &[&str] = &["turn_bridge_cancelled"];

/// Returns true when the supplied cancel reason represents a user /
/// external explicit stop. Matches either an exact reason in
/// [`USER_CANCEL_REASONS`] or any reason with the `user_` prefix so
/// future operator-initiated stops can opt in without editing the
/// whitelist.
pub(crate) fn is_user_cancel_reason(reason: Option<&str>) -> bool {
    let Some(reason) = reason else {
        return false;
    };
    let trimmed = reason.trim();
    if trimmed.is_empty() {
        return false;
    }
    if USER_CANCEL_REASONS
        .iter()
        .any(|candidate| *candidate == trimmed)
    {
        return true;
    }
    trimmed.starts_with("user_")
}

/// Cancel a live dispatch and reset any linked auto-queue entry back to pending.
///
/// The dispatch row remains the canonical source of truth. `auto_queue_entries`
/// is a derived projection that must be cleared whenever the linked dispatch is
/// cancelled so a stale `dispatched` entry cannot block or duplicate work.
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub fn cancel_dispatch_and_reset_auto_queue_on_conn(
    conn: &sqlite_test::Connection,
    dispatch_id: &str,
    reason: Option<&str>,
) -> sqlite_test::Result<usize> {
    let mut stmt = conn.prepare(
        "SELECT id FROM auto_queue_entries
         WHERE dispatch_id = ?1 AND status IN ('pending', 'dispatched')",
    )?;
    let entry_ids: Vec<String> = stmt
        .query_map([dispatch_id], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    drop(stmt);

    let cancel_payload = reason.map(|reason| json!({ "reason": reason }));
    let cancelled = if let Some(payload) = cancel_payload.as_ref() {
        set_dispatch_status_without_queue_sync_on_conn(
            conn,
            dispatch_id,
            "cancelled",
            Some(payload),
            "cancel_dispatch",
            Some(&["pending", "dispatched"]),
            false,
        )
        .map_err(|e| {
            sqlite_test::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(
                e.to_string(),
            )))
        })?
    } else {
        set_dispatch_status_without_queue_sync_on_conn(
            conn,
            dispatch_id,
            "cancelled",
            None,
            "cancel_dispatch",
            Some(&["pending", "dispatched"]),
            false,
        )
        .map_err(|e| {
            sqlite_test::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(
                e.to_string(),
            )))
        })?
    };

    let dispatch_status: Option<String> = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = ?1",
            [dispatch_id],
            |row| row.get(0),
        )
        .ok();
    if matches!(
        dispatch_status.as_deref(),
        Some("cancelled") | Some("failed")
    ) {
        conn.execute(
            "UPDATE sessions
             SET status = CASE
                     WHEN status IN ('turn_active', 'awaiting_bg', 'awaiting_user', 'working') THEN 'idle'
                     ELSE status
                 END,
                 active_dispatch_id = NULL,
                 session_info = ?1
             WHERE active_dispatch_id = ?2",
            sqlite_test::params!["Dispatch cancelled", dispatch_id],
        )?;

        // #815: user / external explicit stops must move the entry to a
        // non-dispatchable terminal status so the next auto-queue tick does
        // not immediately re-dispatch the same entry. System cancels (retry
        // exhausted, supersession, etc.) keep the existing pending reset so
        // re-dispatch proceeds.
        let user_cancel = is_user_cancel_reason(reason);
        let (target_status, trigger_source) = if user_cancel {
            (
                crate::db::auto_queue::ENTRY_STATUS_USER_CANCELLED,
                "dispatch_cancel_user",
            )
        } else {
            (
                crate::db::auto_queue::ENTRY_STATUS_PENDING,
                "dispatch_cancel",
            )
        };

        for entry_id in entry_ids {
            let completed_at_expr = if target_status == crate::db::auto_queue::ENTRY_STATUS_PENDING
            {
                "NULL"
            } else {
                "COALESCE(completed_at, datetime('now'))"
            };
            conn.execute(
                &format!(
                    "UPDATE auto_queue_entries
                     SET status = ?1,
                         dispatch_id = NULL,
                         dispatched_at = NULL,
                         completed_at = {completed_at_expr},
                         updated_at = datetime('now')
                     WHERE id = ?2"
                ),
                sqlite_test::params![target_status, entry_id],
            )?;
            let _ = trigger_source;
        }
    }

    Ok(cancelled)
}

pub async fn cancel_dispatch_and_reset_auto_queue_on_pg(
    pool: &PgPool,
    dispatch_id: &str,
    reason: Option<&str>,
) -> Result<usize, String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin postgres dispatch cancel transaction: {error}"))?;

    // On error the Transaction's Drop runs an implicit rollback, so any
    // partial writes from the helper are discarded automatically.
    let changed =
        cancel_dispatch_and_reset_auto_queue_on_pg_tx(&mut tx, dispatch_id, reason).await?;

    tx.commit()
        .await
        .map_err(|error| format!("commit postgres dispatch cancel {dispatch_id}: {error}"))?;

    Ok(changed)
}

/// Cancel a live dispatch and reset linked auto-queue entries inside a caller-owned
/// PostgreSQL transaction.
///
/// Mirrors `cancel_dispatch_and_reset_auto_queue_on_pg` semantics (stale guard on
/// `pending`/`dispatched`, dispatch_events insert, status_reaction outbox,
/// auto_queue_entries reset to `pending`) but does not begin or commit the
/// transaction. The caller composes this into a wider atomic operation. On
/// stale-guard / missing-row paths this returns `Ok(0)` without writing — the
/// caller decides whether to commit or rollback the surrounding work.
pub async fn cancel_dispatch_and_reset_auto_queue_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    dispatch_id: &str,
    reason: Option<&str>,
) -> Result<usize, String> {
    let cancel_payload = reason.map(|value| json!({ "reason": value }));

    let current = sqlx::query(
        "SELECT status, kanban_card_id, dispatch_type, thread_id
         FROM task_dispatches
         WHERE id = $1",
    )
    .bind(dispatch_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|error| format!("load postgres dispatch {dispatch_id}: {error}"))?;
    let Some(current) = current else {
        return Ok(0);
    };

    let current_status = current
        .try_get::<Option<String>, _>("status")
        .ok()
        .flatten()
        .unwrap_or_default();
    if !matches!(current_status.as_str(), "pending" | "dispatched") {
        return Ok(0);
    }

    let changed = match cancel_payload.as_ref() {
        Some(payload) => sqlx::query(
            "UPDATE task_dispatches
             SET status = 'cancelled',
                 result = $1,
                 updated_at = NOW()
             WHERE id = $2
               AND status = $3",
        )
        .bind(payload.to_string())
        .bind(dispatch_id)
        .bind(&current_status)
        .execute(&mut **tx)
        .await
        .map_err(|error| format!("cancel postgres dispatch {dispatch_id}: {error}"))?
        .rows_affected() as usize,
        None => sqlx::query(
            "UPDATE task_dispatches
             SET status = 'cancelled',
                 updated_at = NOW()
             WHERE id = $1
               AND status = $2",
        )
        .bind(dispatch_id)
        .bind(&current_status)
        .execute(&mut **tx)
        .await
        .map_err(|error| format!("cancel postgres dispatch {dispatch_id}: {error}"))?
        .rows_affected() as usize,
    };

    if changed == 0 {
        return Ok(0);
    }

    let kanban_card_id = current
        .try_get::<Option<String>, _>("kanban_card_id")
        .ok()
        .flatten();
    let dispatch_type = current
        .try_get::<Option<String>, _>("dispatch_type")
        .ok()
        .flatten();
    let thread_id = current
        .try_get::<Option<String>, _>("thread_id")
        .ok()
        .flatten();
    clear_cancelled_dispatch_thread_link_on_pg_tx(
        tx,
        dispatch_id,
        kanban_card_id.as_deref(),
        thread_id.as_deref(),
    )
    .await?;

    sqlx::query(
        "UPDATE sessions
         SET status = CASE
                 WHEN status IN ('turn_active', 'awaiting_bg', 'awaiting_user', 'working') THEN 'idle'
                 ELSE status
             END,
             active_dispatch_id = NULL,
             session_info = $1,
             last_heartbeat = NOW()
         WHERE active_dispatch_id = $2",
    )
    .bind("Dispatch cancelled")
    .bind(dispatch_id)
    .execute(&mut **tx)
    .await
    .map_err(|error| format!("clear postgres session dispatch link {dispatch_id}: {error}"))?;

    let _ = sqlx::query(
        "INSERT INTO dispatch_events (
            dispatch_id,
            kanban_card_id,
            dispatch_type,
            from_status,
            to_status,
            transition_source,
            payload_json
        ) VALUES ($1, $2, $3, $4, 'cancelled', 'cancel_dispatch', $5)",
    )
    .bind(dispatch_id)
    .bind(kanban_card_id)
    .bind(dispatch_type)
    .bind(&current_status)
    .bind(cancel_payload.clone())
    .execute(&mut **tx)
    .await;

    let _ = sqlx::query(
        "INSERT INTO dispatch_outbox (dispatch_id, action)
         SELECT $1, 'status_reaction'
         WHERE NOT EXISTS (
             SELECT 1
             FROM dispatch_outbox
             WHERE dispatch_id = $1
               AND action = 'status_reaction'
               AND status IN ('pending', 'processing')
         )",
    )
    .bind(dispatch_id)
    .execute(&mut **tx)
    .await;

    let entry_rows = sqlx::query(
        "SELECT id, status
         FROM auto_queue_entries
         WHERE dispatch_id = $1
           AND status IN ('pending', 'dispatched')",
    )
    .bind(dispatch_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(|error| format!("load postgres queue entries for dispatch {dispatch_id}: {error}"))?;

    // #815: user / external explicit stops must move the entry to a
    // non-dispatchable terminal status so the next auto-queue tick does
    // not immediately re-dispatch the same entry. System cancels keep
    // the existing pending reset so re-dispatch proceeds.
    //
    // #815 P2: route both branches through the shared
    // `update_entry_status_on_pg_tx` helper so the PG path validates
    // the transition, records `auto_queue_entry_transitions` consistently,
    // and (for system-terminal target statuses) invokes
    // `maybe_finalize_run_after_terminal_entry_pg`. `user_cancelled` is
    // intentionally non-finalizing per P1 — see the helper's comment.
    let user_cancel = is_user_cancel_reason(reason);
    let (target_status, trigger_source) = if user_cancel {
        (
            crate::db::auto_queue::ENTRY_STATUS_USER_CANCELLED,
            "dispatch_cancel_user",
        )
    } else {
        (
            crate::db::auto_queue::ENTRY_STATUS_PENDING,
            "dispatch_cancel",
        )
    };

    for row in entry_rows {
        let entry_id: String = row.try_get("id").map_err(|error| {
            format!("decode postgres queue entry id for {dispatch_id}: {error}")
        })?;
        crate::db::auto_queue::update_entry_status_on_pg_tx(
            tx,
            &entry_id,
            target_status,
            trigger_source,
            &crate::db::auto_queue::EntryStatusUpdateOptions::default(),
        )
        .await?;
    }

    Ok(changed)
}

fn normalized_dispatch_thread_id(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn thread_map_value_matches(value: &serde_json::Value, thread_id: &str) -> bool {
    value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map_or_else(
            || {
                value
                    .as_u64()
                    .is_some_and(|value| value.to_string() == thread_id)
            },
            |value| value == thread_id,
        )
}

async fn clear_cancelled_dispatch_thread_link_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    dispatch_id: &str,
    card_id: Option<&str>,
    thread_id: Option<&str>,
) -> Result<(), String> {
    let Some(card_id) = normalized_dispatch_thread_id(card_id) else {
        return Ok(());
    };
    let Some(thread_id) = normalized_dispatch_thread_id(thread_id) else {
        return Ok(());
    };

    let still_referenced_by_live_dispatch: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1
             FROM task_dispatches
             WHERE kanban_card_id = $1
               AND thread_id = $2
               AND id <> $3
               AND status NOT IN ('cancelled', 'failed', 'expired')
         )",
    )
    .bind(&card_id)
    .bind(&thread_id)
    .bind(dispatch_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(|error| format!("check live thread references for {dispatch_id}: {error}"))?;
    if still_referenced_by_live_dispatch {
        return Ok(());
    }

    let row = sqlx::query(
        "SELECT channel_thread_map::text AS channel_thread_map, active_thread_id
         FROM kanban_cards
         WHERE id = $1
         FOR UPDATE",
    )
    .bind(&card_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|error| {
        format!("load card thread map for cancelled dispatch {dispatch_id}: {error}")
    })?;
    let Some(row) = row else {
        return Ok(());
    };
    let map_json: Option<String> = row.try_get("channel_thread_map").map_err(|error| {
        format!("read card thread map for cancelled dispatch {dispatch_id}: {error}")
    })?;
    let active_thread_id: Option<String> = row.try_get("active_thread_id").map_err(|error| {
        format!("read active thread for cancelled dispatch {dispatch_id}: {error}")
    })?;

    let mut map = map_json
        .as_deref()
        .and_then(|raw| {
            serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(raw).ok()
        })
        .unwrap_or_default();
    let before_len = map.len();
    map.retain(|_, value| !thread_map_value_matches(value, &thread_id));
    let removed_from_map = map.len() != before_len;
    let active_matches = active_thread_id
        .as_deref()
        .map(str::trim)
        .is_some_and(|value| value == thread_id);

    if !removed_from_map && !active_matches {
        return Ok(());
    }

    let new_map = if map.is_empty() {
        None
    } else {
        Some(
            serde_json::to_string(&map)
                .map_err(|error| format!("serialize card thread map for {card_id}: {error}"))?,
        )
    };
    let new_active_thread_id = if active_matches {
        map.values()
            .find_map(|value| value.as_str())
            .map(std::string::ToString::to_string)
    } else {
        active_thread_id
    };

    sqlx::query(
        "UPDATE kanban_cards
         SET channel_thread_map = $1::jsonb,
             active_thread_id = $2,
             updated_at = NOW()
         WHERE id = $3",
    )
    .bind(new_map)
    .bind(new_active_thread_id)
    .bind(&card_id)
    .execute(&mut **tx)
    .await
    .map_err(|error| {
        format!("clear cancelled dispatch thread link for {dispatch_id}/{card_id}: {error}")
    })?;

    Ok(())
}

/// Cancel all live dispatches for a card without resetting auto-queue entries.
///
/// Used when PMD force-transitions a live card back to backlog/ready. In that
/// case the current work should be abandoned rather than re-queued into the
/// same active run.
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub fn cancel_active_dispatches_for_card_on_conn(
    conn: &sqlite_test::Connection,
    card_id: &str,
    reason: Option<&str>,
) -> sqlite_test::Result<usize> {
    let mut stmt = conn.prepare(
        "SELECT id FROM task_dispatches
         WHERE kanban_card_id = ?1 AND status IN ('pending', 'dispatched')",
    )?;
    let live_dispatch_ids: Vec<String> = stmt
        .query_map([card_id], |row| row.get(0))?
        .filter_map(|row| row.ok())
        .collect();
    drop(stmt);

    conn.execute(
        "UPDATE sessions \
         SET status = CASE WHEN status IN ('turn_active', 'working') THEN 'idle' ELSE status END, \
             active_dispatch_id = NULL \
         WHERE active_dispatch_id IN (
             SELECT id FROM task_dispatches
             WHERE kanban_card_id = ?1 AND status IN ('pending', 'dispatched')
        )",
        [card_id],
    )?;

    let cancel_payload =
        reason.map(|reason| json!({ "reason": reason, "completion_source": "force_transition" }));
    let mut cancelled = 0usize;
    for dispatch_id in live_dispatch_ids {
        cancelled += match cancel_payload.as_ref() {
            Some(payload) => set_dispatch_status_without_queue_sync_on_conn(
                conn,
                &dispatch_id,
                "cancelled",
                Some(payload),
                "cancel_dispatch",
                Some(&["pending", "dispatched"]),
                false,
            )
            .map_err(|error| {
                sqlite_test::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(
                    error.to_string(),
                )))
            })?,
            None => set_dispatch_status_without_queue_sync_on_conn(
                conn,
                &dispatch_id,
                "cancelled",
                None,
                "cancel_dispatch",
                Some(&["pending", "dispatched"]),
                false,
            )
            .map_err(|error| {
                sqlite_test::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(
                    error.to_string(),
                )))
            })?,
        };
    }
    Ok(cancelled)
}

pub fn is_unified_thread_channel_active(channel_id: u64) -> bool {
    let _ = channel_id;
    false
}

/// Extract thread channel ID from a channel name's `-t{15+digit}` suffix.
/// Pure parsing — no DB access. Used by both production guards and tests.
#[cfg_attr(not(test), allow(dead_code))]
pub fn extract_thread_channel_id(channel_name: &str) -> Option<u64> {
    let pos = channel_name.rfind("-t")?;
    let suffix = &channel_name[pos + 2..];
    if suffix.len() >= 15 && suffix.chars().all(|c| c.is_ascii_digit()) {
        let id: u64 = suffix.parse().ok()?;
        if id == 0 { None } else { Some(id) }
    } else {
        None
    }
}

/// Check whether a channel name (from tmux session parsing) belongs to an active
/// unified-thread auto-queue run. Extracts the thread channel ID from the
/// `-t{15+digit}` suffix in the channel name.
pub fn is_unified_thread_channel_name_active(channel_name: &str) -> bool {
    let _ = channel_name;
    false
}

pub fn drain_unified_thread_kill_signals() -> Vec<String> {
    Vec::new()
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::build_review_context_sqlite_test as build_review_context;
    use super::create_dispatch_record_sqlite_test as create_dispatch_record_test;
    use super::resolve_card_worktree_sqlite_test as resolve_card_worktree;
    use super::test_support::*;
    use super::*;
    use crate::services::git::GitCommand;

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

        // Card should be updated — #255: ready→requested is free, so kickoff_for("ready")
        // falls back to first dispatchable state target = "in_progress"
        let conn = db.separate_conn().unwrap();
        let (card_status, latest_dispatch_id): (String, String) = conn
            .query_row(
                "SELECT status, latest_dispatch_id FROM kanban_cards WHERE id = 'card-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(card_status, "in_progress");
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
        seed_assistant_response_for_dispatch(&db, &dispatch_id, "implemented");

        let completed =
            complete_dispatch(&db, &engine, &dispatch_id, &json!({"output": "done"})).unwrap();

        assert_eq!(completed["status"], "completed");
    }

    #[test]
    fn complete_dispatch_records_completed_at() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-2-ts", "ready");

        let dispatch = create_dispatch(
            &db,
            &engine,
            "card-2-ts",
            "agent-1",
            "implementation",
            "title",
            &json!({}),
        )
        .unwrap();
        let dispatch_id = dispatch["id"].as_str().unwrap().to_string();
        seed_assistant_response_for_dispatch(&db, &dispatch_id, "implemented");

        let completed =
            complete_dispatch(&db, &engine, &dispatch_id, &json!({"output": "done"})).unwrap();

        assert!(
            completed["completed_at"].as_str().is_some(),
            "completion result must expose completed_at"
        );

        let conn = db.separate_conn().unwrap();
        let stored_completed_at: Option<String> = conn
            .query_row(
                "SELECT completed_at FROM task_dispatches WHERE id = ?1",
                [&dispatch_id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            stored_completed_at.is_some(),
            "task_dispatches.completed_at must be stored for completed rows"
        );
    }

    #[test]
    fn complete_dispatch_nonexistent_fails() {
        let db = test_db();
        let engine = test_engine(&db);

        let result = complete_dispatch(&db, &engine, "nonexistent", &json!({}));
        assert!(result.is_err());
    }

    #[test]
    fn complete_dispatch_rejects_work_without_execution_evidence() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-no-evidence", "ready");

        let dispatch = create_dispatch(
            &db,
            &engine,
            "card-no-evidence",
            "agent-1",
            "implementation",
            "title",
            &json!({}),
        )
        .unwrap();
        let dispatch_id = dispatch["id"].as_str().unwrap().to_string();

        let result = complete_dispatch(
            &db,
            &engine,
            &dispatch_id,
            &json!({"completion_source": "test_harness"}),
        );
        let err = format!("{}", result.unwrap_err());
        assert!(err.contains("no agent execution evidence"));
        assert_eq!(dispatch["status"], "pending");

        let conn = db.separate_conn().unwrap();
        let status: String = conn
            .query_row(
                "SELECT status FROM task_dispatches WHERE id = ?1",
                [&dispatch_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "pending");
    }

    #[test]
    fn create_dispatch_records_origin_main_baseline_commit() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-baseline", "ready");
        let (repo, _origin, _override_guard) = setup_test_repo_with_origin();
        let repo_dir = repo.path().to_str().unwrap();

        run_git(
            repo_dir,
            &["commit", "--allow-empty", "-m", "local main only"],
        );
        let expected_baseline =
            crate::services::platform::shell::git_dispatch_baseline_commit(repo_dir)
                .expect("origin/main baseline");

        let dispatch = create_dispatch(
            &db,
            &engine,
            "card-baseline",
            "agent-1",
            "implementation",
            "title",
            &json!({}),
        )
        .unwrap();

        assert_eq!(
            dispatch["context"]["baseline_commit"].as_str(),
            Some(expected_baseline.as_str())
        );
    }

    #[test]
    fn complete_dispatch_skips_cancelled() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-cancel", "review");

        let dispatch = create_dispatch(
            &db,
            &engine,
            "card-cancel",
            "agent-1",
            "review-decision",
            "Decision",
            &json!({}),
        )
        .unwrap();
        let dispatch_id = dispatch["id"].as_str().unwrap().to_string();

        // Simulate dismiss: cancel the dispatch
        {
            let conn = db.separate_conn().unwrap();
            conn.execute(
                "UPDATE task_dispatches SET status = 'cancelled' WHERE id = ?1",
                [&dispatch_id],
            )
            .unwrap();
        }

        // Delayed completion attempt should NOT re-complete the cancelled dispatch
        let result = complete_dispatch(&db, &engine, &dispatch_id, &json!({"verdict": "pass"}));
        // Should return Ok (dispatch found) but status should remain cancelled
        assert!(result.is_ok());
        let returned = result.unwrap();
        assert_eq!(
            returned["status"], "cancelled",
            "cancelled dispatch must not be re-completed"
        );
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
            cancel_dispatch_and_reset_auto_queue_on_conn(&conn, "dispatch-aq", Some("test"))
                .unwrap();
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
    fn terminal_dispatch_status_clears_linked_session_active_dispatch() {
        let db = test_db();
        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, created_at, updated_at) \
             VALUES ('card-terminal-session', 'Terminal Session Card', 'in_progress', 'agent-1', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at) \
             VALUES ('dispatch-terminal-session', 'card-terminal-session', 'agent-1', 'implementation', 'dispatched', 'Terminal Session', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (session_key, agent_id, provider, status, active_dispatch_id, session_info, created_at) \
             VALUES ('session-terminal-session', 'agent-1', 'codex', 'turn_active', 'dispatch-terminal-session', 'live dispatch', datetime('now'))",
            [],
        )
        .unwrap();

        let changed = set_dispatch_status_on_conn(
            &conn,
            "dispatch-terminal-session",
            "completed",
            Some(&serde_json::json!({"completed_commit": "abc123"})),
            "api",
            Some(&["dispatched"]),
            true,
        )
        .unwrap();
        assert_eq!(changed, 1);

        let (session_status, active_dispatch_id, session_info): (
            String,
            Option<String>,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT status, active_dispatch_id, session_info FROM sessions WHERE session_key = 'session-terminal-session'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(session_status, "idle");
        assert!(active_dispatch_id.is_none());
        assert_eq!(session_info.as_deref(), Some("Dispatch completed"));
    }

    // ── #815 regression: user reaction-stop must not re-dispatch ──────
    //
    // Exercises the full user-cancel flow against the canonical sqlite
    // schema (the real `migrate()` tables are created by `test_db()`):
    //
    //   (a) auto-queue activates a dispatch for an entry;
    //   (b) the user cancels the dispatch with
    //       `turn_cancel_reason = turn_bridge_cancelled`, which must move
    //       the linked entry to `user_cancelled` (non-dispatchable) rather
    //       than resetting to `pending`, and must NOT mark the card `done`;
    //   (c) a subsequent auto-queue tick-style lookup for `pending` entries
    //       must not find this entry — i.e. it is NOT re-dispatchable;
    //   (d) the system-cancel path (reason = `superseded_by_reseed`)
    //       keeps the old behaviour: entry resets to `pending` and the
    //       next tick can re-pick it up.
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

        let (entry_status, entry_dispatch_id, completed_at): (String, Option<String>, Option<String>) = conn
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

        // (c) simulate the auto-queue tick query — a `pending` entry in an
        // active run would be re-dispatched. `user_cancelled` must NOT be.
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

        // Card must NOT have been force-transitioned to done by the cancel
        // path. It stays in its prior status (in_progress) so the user can
        // restart work deliberately.
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

        // #815 P1: the run must NOT be auto-finalized when the last live
        // entry transitions to `user_cancelled`. If it were `completed`,
        // there is no API path that could re-open it (`restore` only takes
        // `cancelled`/`restoring`, `resume` only reopens `paused`,
        // `activate()` only promotes `generated`/`pending`), so flipping the
        // entry back to `pending` would strand it inside a completed run.
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

    // #815 P1: after a user cancel, the operator must be able to restart the
    // entry by flipping it back to `pending`. The next auto-queue tick must
    // then see it as re-dispatchable.
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

        // Confirm the precondition: entry is `user_cancelled`, run is still active.
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

        // Operator restart: flip the entry back to `pending`.
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

        // The next auto-queue tick (modeled here as the same JOIN the tick
        // uses to find dispatchable work) must now see the entry again.
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

        // The entry must remain re-dispatchable by the tick.
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
            "system cancels must leave the entry visible to the next tick"
        );
    }

    #[test]
    fn provider_from_channel_suffix_supports_registry_providers() {
        assert_eq!(provider_from_channel_suffix("agent-cc"), Some("claude"));
        assert_eq!(provider_from_channel_suffix("agent-cdx"), Some("codex"));
        assert_eq!(provider_from_channel_suffix("agent-gm"), Some("gemini"));
        assert_eq!(provider_from_channel_suffix("agent-oc"), Some("opencode"));
        assert_eq!(provider_from_channel_suffix("agent-qw"), Some("qwen"));
        assert_eq!(provider_from_channel_suffix("agent"), None);
    }

    #[test]
    fn create_review_dispatch_for_done_card_rejected() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-done", "done");

        for dispatch_type in &["review", "review-decision", "rework"] {
            let result = create_dispatch(
                &db,
                &engine,
                "card-done",
                "agent-1",
                dispatch_type,
                "Should fail",
                &json!({}),
            );
            assert!(
                result.is_err(),
                "{} dispatch should not be created for done card",
                dispatch_type
            );
        }

        // All dispatch types for done cards should be rejected
        let result = create_dispatch(
            &db,
            &engine,
            "card-done",
            "agent-1",
            "implementation",
            "Reopen work",
            &json!({}),
        );
        assert!(
            result.is_err(),
            "implementation dispatch should be rejected for done card"
        );
    }

    #[test]
    fn create_sidecar_phase_gate_for_terminal_card_preserves_card_state() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-phase-gate", "done");

        let dispatch = create_dispatch(
            &db,
            &engine,
            "card-phase-gate",
            "agent-1",
            "phase-gate",
            "Phase Gate",
            &json!({
                "phase_gate": {
                    "run_id": "run-sidecar",
                    "batch_phase": 2,
                    "pass_verdict": "phase_gate_passed",
                }
            }),
        )
        .expect("phase gate sidecar dispatch should be allowed for terminal cards");

        let dispatch_id = dispatch["id"].as_str().unwrap().to_string();
        let conn = db.separate_conn().unwrap();
        let (card_status, latest_dispatch_id): (String, Option<String>) = conn
            .query_row(
                "SELECT status, latest_dispatch_id FROM kanban_cards WHERE id = 'card-phase-gate'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(card_status, "done");
        assert!(
            latest_dispatch_id.is_none(),
            "sidecar phase gate must not replace latest_dispatch_id"
        );
        assert_eq!(
            count_notify_outbox(&conn, &dispatch_id),
            1,
            "sidecar phase gate must still enqueue a notify outbox row"
        );
        assert_eq!(
            load_dispatch_events(&conn, &dispatch_id),
            vec![(None, "pending".to_string(), "create_dispatch".to_string())],
            "sidecar dispatch creation should still be audited"
        );
    }

    #[test]
    fn create_sidecar_phase_gate_skips_repo_lookup_without_explicit_worktree() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-phase-gate-repo", "done");
        set_card_issue_number(&db, "card-phase-gate-repo", 685);
        set_card_repo_id(&db, "card-phase-gate-repo", "test/repo");

        let dispatch = create_dispatch(
            &db,
            &engine,
            "card-phase-gate-repo",
            "agent-1",
            "phase-gate",
            "Phase Gate Repo",
            &json!({
                "phase_gate": {
                    "run_id": "run-sidecar-repo",
                    "batch_phase": 0,
                    "pass_verdict": "phase_gate_passed"
                }
            }),
        )
        .expect("phase gate sidecar should not require repo_dirs mapping");

        let dispatch_id = dispatch["id"].as_str().unwrap().to_string();
        let conn = db.separate_conn().unwrap();
        let context: String = conn
            .query_row(
                "SELECT context FROM task_dispatches WHERE id = ?1",
                [&dispatch_id],
                |row| row.get(0),
            )
            .unwrap();
        let context_json: serde_json::Value = serde_json::from_str(&context).unwrap();
        assert!(
            context_json.get("worktree_path").is_none(),
            "phase gate sidecar should not synthesize a repo-derived worktree path"
        );
        assert_eq!(
            context_json["phase_gate"]["run_id"], "run-sidecar-repo",
            "phase gate payload must remain intact"
        );
    }

    #[test]
    fn dispatch_type_force_new_session_defaults_split_by_dispatch_type() {
        assert_eq!(
            dispatch_type_force_new_session_default(Some("implementation")),
            Some(true)
        );
        assert_eq!(
            dispatch_type_force_new_session_default(Some("review")),
            Some(true)
        );
        assert_eq!(
            dispatch_type_force_new_session_default(Some("rework")),
            Some(true)
        );
        assert_eq!(
            dispatch_type_force_new_session_default(Some("review-decision")),
            Some(false)
        );
        assert_eq!(
            dispatch_type_force_new_session_default(Some("consultation")),
            None
        );
        assert_eq!(dispatch_type_force_new_session_default(None), None);
    }

    #[test]
    fn dispatch_type_session_strategy_defaults_split_by_dispatch_type() {
        assert_eq!(
            dispatch_type_session_strategy_default(Some("implementation")),
            Some(DispatchSessionStrategy {
                reset_provider_state: true,
                recreate_tmux: false,
            })
        );
        assert_eq!(
            dispatch_type_session_strategy_default(Some("review")),
            Some(DispatchSessionStrategy {
                reset_provider_state: true,
                recreate_tmux: false,
            })
        );
        assert_eq!(
            dispatch_type_session_strategy_default(Some("rework")),
            Some(DispatchSessionStrategy {
                reset_provider_state: true,
                recreate_tmux: false,
            })
        );
        assert_eq!(
            dispatch_type_session_strategy_default(Some("review-decision")),
            Some(DispatchSessionStrategy::default())
        );
        assert_eq!(
            dispatch_type_session_strategy_default(Some("consultation")),
            None
        );
        assert_eq!(dispatch_type_session_strategy_default(None), None);
    }

    #[test]
    fn dispatch_type_thread_routing_includes_phase_gate() {
        assert!(dispatch_type_uses_thread_routing(Some("implementation")));
        assert!(dispatch_type_uses_thread_routing(Some("review")));
        assert!(dispatch_type_uses_thread_routing(Some("rework")));
        assert!(dispatch_type_uses_thread_routing(Some("phase-gate")));
        assert!(dispatch_type_uses_thread_routing(Some("review-decision")));
        assert!(dispatch_type_uses_thread_routing(None));
    }

    #[test]
    fn ensure_dispatch_notify_outbox_skips_completed_dispatch() {
        let db = test_db();
        seed_card(&db, "card-completed-notify", "done");
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at, completed_at)
                 VALUES ('dispatch-completed-notify', 'card-completed-notify', 'agent-1', 'review', 'completed', 'Completed review', datetime('now'), datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
        }

        let conn = db.separate_conn().unwrap();
        let inserted = ensure_dispatch_notify_outbox_on_conn(
            &conn,
            "dispatch-completed-notify",
            "agent-1",
            "card-completed-notify",
            "Completed review",
        )
        .unwrap();

        assert!(
            !inserted,
            "completed dispatches must not enqueue new notify outbox rows"
        );
        assert_eq!(
            count_notify_outbox(&conn, "dispatch-completed-notify"),
            0,
            "completed dispatches must not retain notify outbox rows"
        );
    }

    #[test]
    fn concurrent_dispatches_for_different_cards_have_distinct_ids() {
        // Regression: concurrent dispatches from different cards must not share
        // dispatch IDs or card state — each must be independently routable.
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-a", "ready");
        seed_card(&db, "card-b", "ready");

        let dispatch_a = create_dispatch(
            &db,
            &engine,
            "card-a",
            "agent-1",
            "implementation",
            "Task A",
            &json!({}),
        )
        .unwrap();

        let dispatch_b = create_dispatch(
            &db,
            &engine,
            "card-b",
            "agent-2",
            "implementation",
            "Task B",
            &json!({}),
        )
        .unwrap();

        let id_a = dispatch_a["id"].as_str().unwrap();
        let id_b = dispatch_b["id"].as_str().unwrap();
        assert_ne!(id_a, id_b, "dispatch IDs must be unique");
        assert_eq!(dispatch_a["kanban_card_id"], "card-a");
        assert_eq!(dispatch_b["kanban_card_id"], "card-b");

        // Each card's latest_dispatch_id points to its own dispatch
        let conn = db.separate_conn().unwrap();
        let latest_a: String = conn
            .query_row(
                "SELECT latest_dispatch_id FROM kanban_cards WHERE id = 'card-a'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let latest_b: String = conn
            .query_row(
                "SELECT latest_dispatch_id FROM kanban_cards WHERE id = 'card-b'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(latest_a, id_a);
        assert_eq!(latest_b, id_b);
        assert_ne!(latest_a, latest_b, "card dispatch IDs must not cross");
    }

    #[test]
    fn finalize_dispatch_sets_completion_source() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-fin", "ready");

        let dispatch = create_dispatch(
            &db,
            &engine,
            "card-fin",
            "agent-1",
            "implementation",
            "Finalize test",
            &json!({}),
        )
        .unwrap();
        let dispatch_id = dispatch["id"].as_str().unwrap().to_string();
        seed_assistant_response_for_dispatch(&db, &dispatch_id, "implemented");

        let completed =
            finalize_dispatch(&db, &engine, &dispatch_id, "turn_bridge_explicit", None).unwrap();

        assert_eq!(completed["status"], "completed");
        // result is parsed JSON (query_dispatch_row parses it)
        assert_eq!(
            completed["result"]["completion_source"],
            "turn_bridge_explicit"
        );
    }

    #[test]
    fn finalize_dispatch_merges_context() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-ctx", "ready");

        let dispatch = create_dispatch(
            &db,
            &engine,
            "card-ctx",
            "agent-1",
            "implementation",
            "Context test",
            &json!({}),
        )
        .unwrap();
        let dispatch_id = dispatch["id"].as_str().unwrap().to_string();

        let completed = finalize_dispatch(
            &db,
            &engine,
            &dispatch_id,
            "session_idle",
            Some(&json!({ "auto_completed": true, "agent_response_present": true })),
        )
        .unwrap();

        assert_eq!(completed["status"], "completed");
        assert_eq!(completed["result"]["completion_source"], "session_idle");
        assert_eq!(completed["result"]["auto_completed"], true);
    }

    // #699 — phase-gate completion with all checks passing but no explicit
    // `verdict` must inject `verdict = context.phase_gate.pass_verdict` into
    // the persisted result so auto-queue does not pause the run.
    #[test]
    fn finalize_phase_gate_injects_verdict_when_all_checks_pass() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-pg-pass", "in_progress");

        let context = json!({
            "auto_queue": true,
            "sidecar_dispatch": true,
            "phase_gate": {
                "run_id": "run-699",
                "batch_phase": 1,
                "next_phase": 2,
                "final_phase": false,
                "pass_verdict": "phase_gate_passed",
                "checks": ["merge_verified", "issue_closed", "build_passed"],
            }
        });
        let dispatch = create_dispatch(
            &db,
            &engine,
            "card-pg-pass",
            "agent-1",
            "phase-gate",
            "Phase gate test",
            &context,
        )
        .unwrap();
        let dispatch_id = dispatch["id"].as_str().unwrap().to_string();

        // Simulate a caller that produced all-pass checks + summary but
        // omitted the explicit verdict field entirely.
        let result = json!({
            "summary": "Phase gate passed",
            "checks": {
                "merge_verified": { "status": "pass" },
                "issue_closed": { "status": "pass" },
                "build_passed": { "status": "pass" },
            }
        });
        let completed =
            finalize_dispatch(&db, &engine, &dispatch_id, "api", Some(&result)).unwrap();

        assert_eq!(completed["status"], "completed");
        assert_eq!(
            completed["result"]["verdict"], "phase_gate_passed",
            "server must inject phase_gate_passed when verdict absent and checks all pass",
        );
        assert_eq!(completed["result"]["verdict_inferred"], true);
    }

    // #699 — never infer pass when any check fails. The verdict must remain
    // absent so auto-queue can classify the gate as failed.
    #[test]
    fn finalize_phase_gate_preserves_absent_verdict_when_check_fails() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-pg-fail", "in_progress");

        let context = json!({
            "auto_queue": true,
            "sidecar_dispatch": true,
            "phase_gate": {
                "run_id": "run-699b",
                "batch_phase": 1,
                "pass_verdict": "phase_gate_passed",
                "checks": ["merge_verified", "issue_closed"],
            }
        });
        let dispatch = create_dispatch(
            &db,
            &engine,
            "card-pg-fail",
            "agent-1",
            "phase-gate",
            "Phase gate test (fail)",
            &context,
        )
        .unwrap();
        let dispatch_id = dispatch["id"].as_str().unwrap().to_string();

        let result = json!({
            "checks": {
                "merge_verified": { "status": "pass" },
                "issue_closed": { "status": "fail" },
            }
        });
        let completed =
            finalize_dispatch(&db, &engine, &dispatch_id, "api", Some(&result)).unwrap();

        assert_eq!(completed["status"], "completed");
        assert!(
            completed["result"].get("verdict").is_none()
                || completed["result"]["verdict"].is_null(),
            "verdict must not be inferred when any check is fail"
        );
        assert!(
            completed["result"].get("verdict_inferred").is_none()
                || completed["result"]["verdict_inferred"].is_null(),
            "verdict_inferred flag must not be set on failed checks"
        );
    }

    // #699 (round 2) — non-default phase-gate dispatch types (e.g. "qa-gate")
    // must also receive verdict injection. Detection goes by
    // context.phase_gate presence, not dispatch_type string.
    #[test]
    fn finalize_phase_gate_injects_verdict_for_custom_dispatch_type() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-pg-qa", "in_progress");

        let context = json!({
            "auto_queue": true,
            "sidecar_dispatch": true,
            "phase_gate": {
                "run_id": "run-699qa",
                "batch_phase": 1,
                "pass_verdict": "qa_passed",
                "dispatch_type": "qa-gate",
                "checks": ["merge_verified", "qa_passed"],
            }
        });
        let dispatch = create_dispatch(
            &db,
            &engine,
            "card-pg-qa",
            "agent-1",
            "qa-gate", // non-default dispatch type
            "QA gate test",
            &context,
        )
        .unwrap();
        let dispatch_id = dispatch["id"].as_str().unwrap().to_string();

        let result = json!({
            "summary": "QA gate passed",
            "checks": {
                "merge_verified": { "status": "pass" },
                "qa_passed": { "status": "pass" },
            }
        });
        let completed =
            finalize_dispatch(&db, &engine, &dispatch_id, "api", Some(&result)).unwrap();

        assert_eq!(
            completed["result"]["verdict"], "qa_passed",
            "server must inject the configured pass_verdict (qa_passed) for non-default dispatch type",
        );
        assert_eq!(completed["result"]["verdict_inferred"], true);
    }

    // #699 (round 2) — when `result.checks` is missing a required check key
    // declared in `context.phase_gate.checks`, verdict MUST NOT be inferred.
    // A partial payload cannot advance the gate.
    #[test]
    fn finalize_phase_gate_rejects_inference_when_required_check_absent() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-pg-partial", "in_progress");

        let context = json!({
            "auto_queue": true,
            "sidecar_dispatch": true,
            "phase_gate": {
                "run_id": "run-699partial",
                "batch_phase": 1,
                "pass_verdict": "phase_gate_passed",
                "checks": ["merge_verified", "issue_closed", "build_passed"],
            }
        });
        let dispatch = create_dispatch(
            &db,
            &engine,
            "card-pg-partial",
            "agent-1",
            "phase-gate",
            "Partial checks test",
            &context,
        )
        .unwrap();
        let dispatch_id = dispatch["id"].as_str().unwrap().to_string();

        // Only 2 of 3 declared checks are reported; the third is silently missing.
        let result = json!({
            "checks": {
                "merge_verified": { "status": "pass" },
                "issue_closed": { "status": "pass" },
            }
        });
        let completed =
            finalize_dispatch(&db, &engine, &dispatch_id, "api", Some(&result)).unwrap();

        assert!(
            completed["result"].get("verdict").is_none()
                || completed["result"]["verdict"].is_null(),
            "verdict must not be inferred when a declared required check key is absent",
        );
    }

    // #699 — explicit verdict="fail" must survive verbatim even when every
    // check status happens to be "pass" in the same payload.
    #[test]
    fn finalize_phase_gate_preserves_explicit_verdict_fail() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-pg-explicit", "in_progress");

        let context = json!({
            "auto_queue": true,
            "sidecar_dispatch": true,
            "phase_gate": {
                "run_id": "run-699c",
                "batch_phase": 1,
                "pass_verdict": "phase_gate_passed",
            }
        });
        let dispatch = create_dispatch(
            &db,
            &engine,
            "card-pg-explicit",
            "agent-1",
            "phase-gate",
            "Phase gate test (explicit fail)",
            &context,
        )
        .unwrap();
        let dispatch_id = dispatch["id"].as_str().unwrap().to_string();

        let result = json!({
            "verdict": "fail",
            "summary": "Operator-forced fail",
            "checks": {
                "merge_verified": { "status": "pass" },
            }
        });
        let completed =
            finalize_dispatch(&db, &engine, &dispatch_id, "api", Some(&result)).unwrap();

        assert_eq!(completed["result"]["verdict"], "fail");
        assert!(
            completed["result"].get("verdict_inferred").is_none()
                || completed["result"]["verdict_inferred"].is_null(),
            "explicit verdict must not be flagged as inferred"
        );
    }

    #[test]
    fn dispatch_events_capture_dispatched_and_completed_transitions() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-events", "ready");

        let dispatch = create_dispatch(
            &db,
            &engine,
            "card-events",
            "agent-1",
            "implementation",
            "Event trail",
            &json!({}),
        )
        .unwrap();
        let dispatch_id = dispatch["id"].as_str().unwrap().to_string();

        {
            let conn = db.separate_conn().unwrap();
            set_dispatch_status_on_conn(
                &conn,
                &dispatch_id,
                "dispatched",
                None,
                "test_dispatch_outbox",
                Some(&["pending"]),
                false,
            )
            .unwrap();
        }
        seed_assistant_response_for_dispatch(&db, &dispatch_id, "implemented");

        finalize_dispatch(&db, &engine, &dispatch_id, "test_complete", None).unwrap();

        let conn = db.separate_conn().unwrap();
        assert_eq!(
            load_dispatch_events(&conn, &dispatch_id),
            vec![
                (None, "pending".to_string(), "create_dispatch".to_string()),
                (
                    Some("pending".to_string()),
                    "dispatched".to_string(),
                    "test_dispatch_outbox".to_string()
                ),
                (
                    Some("dispatched".to_string()),
                    "completed".to_string(),
                    "test_complete".to_string()
                ),
            ],
            "dispatch event log must preserve ordered status transitions"
        );
    }

    /// #750: narrowed enqueue policy.
    /// - pending → dispatched: no enqueue (command bot's ⏳ is the source).
    /// - dispatched → completed from live command-bot paths
    ///   (`transition_source` starts with "turn_bridge" or "watcher"): no
    ///   enqueue. Command bot already added ✅ on response delivery.
    /// - dispatched → completed from non-live paths (api, recovery,
    ///   supervisor, test_*): enqueue. Announce bot's ✅ is the only
    ///   terminal success signal on the original message.
    /// - any → failed / cancelled: enqueue. Announce bot must clean
    ///   command bot's stale ✅ and add ❌ to avoid false green checks.
    #[test]
    fn dispatch_status_transitions_enqueue_narrowed_on_non_live_paths() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-outbox-turn-bridge", "ready");

        let live = create_dispatch(
            &db,
            &engine,
            "card-outbox-turn-bridge",
            "agent-1",
            "implementation",
            "Live trail",
            &json!({}),
        )
        .unwrap();
        let live_id = live["id"].as_str().unwrap().to_string();

        let conn = db.separate_conn().unwrap();
        set_dispatch_status_on_conn(
            &conn,
            &live_id,
            "dispatched",
            None,
            "turn_bridge_notify",
            Some(&["pending"]),
            false,
        )
        .unwrap();
        assert_eq!(
            count_status_reaction_outbox(&conn, &live_id),
            0,
            "#750: pending→dispatched must never enqueue (command bot owns ⏳)"
        );

        set_dispatch_status_on_conn(
            &conn,
            &live_id,
            "completed",
            Some(&json!({"completion_source":"turn_bridge_explicit"})),
            "turn_bridge_explicit",
            Some(&["dispatched"]),
            true,
        )
        .unwrap();
        assert_eq!(
            count_status_reaction_outbox(&conn, &live_id),
            0,
            "#750: completed via turn_bridge must not enqueue (command bot already added ✅)"
        );

        seed_card(&db, "card-outbox-api", "ready");
        let api = create_dispatch(
            &db,
            &engine,
            "card-outbox-api",
            "agent-1",
            "implementation",
            "API trail",
            &json!({}),
        )
        .unwrap();
        let api_id = api["id"].as_str().unwrap().to_string();
        set_dispatch_status_on_conn(
            &conn,
            &api_id,
            "dispatched",
            None,
            "turn_bridge_notify",
            Some(&["pending"]),
            false,
        )
        .unwrap();
        set_dispatch_status_on_conn(
            &conn,
            &api_id,
            "completed",
            Some(&json!({"completion_source":"api"})),
            "api",
            Some(&["dispatched"]),
            true,
        )
        .unwrap();
        assert_eq!(
            count_status_reaction_outbox(&conn, &api_id),
            1,
            "#750: completed via api/recovery/etc. must enqueue (no command-bot ✅ on message)"
        );

        seed_card(&db, "card-outbox-failed", "ready");
        let failed = create_dispatch(
            &db,
            &engine,
            "card-outbox-failed",
            "agent-1",
            "implementation",
            "Fail trail",
            &json!({}),
        )
        .unwrap();
        let failed_id = failed["id"].as_str().unwrap().to_string();
        set_dispatch_status_on_conn(
            &conn,
            &failed_id,
            "dispatched",
            None,
            "turn_bridge_notify",
            Some(&["pending"]),
            false,
        )
        .unwrap();
        set_dispatch_status_on_conn(
            &conn,
            &failed_id,
            "failed",
            Some(&json!({"completion_source":"turn_bridge_explicit"})),
            "turn_bridge_explicit",
            Some(&["dispatched"]),
            true,
        )
        .unwrap();
        assert_eq!(
            count_status_reaction_outbox(&conn, &failed_id),
            1,
            "#750: failed ALWAYS enqueues regardless of source (announce bot cleans ✅ and adds ❌)"
        );

        seed_card(&db, "card-outbox-cancelled", "ready");
        let cancelled = create_dispatch(
            &db,
            &engine,
            "card-outbox-cancelled",
            "agent-1",
            "implementation",
            "Cancel trail",
            &json!({}),
        )
        .unwrap();
        let cancelled_id = cancelled["id"].as_str().unwrap().to_string();
        set_dispatch_status_on_conn(
            &conn,
            &cancelled_id,
            "cancelled",
            Some(&json!({"completion_source":"cli"})),
            "cli",
            Some(&["pending"]),
            true,
        )
        .unwrap();
        assert_eq!(
            count_status_reaction_outbox(&conn, &cancelled_id),
            1,
            "#750: cancelled ALWAYS enqueues regardless of source"
        );
    }

    // ── #173 Dedup tests ─────────────────────────────────────────────

    #[test]
    fn dedup_same_card_same_type_returns_existing_dispatch() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-dup", "ready");

        let d1 = create_dispatch(
            &db,
            &engine,
            "card-dup",
            "agent-1",
            "implementation",
            "First",
            &json!({}),
        )
        .unwrap();
        let id1 = d1["id"].as_str().unwrap();

        // Second call with same card + same type → should return existing
        let d2 = create_dispatch(
            &db,
            &engine,
            "card-dup",
            "agent-1",
            "implementation",
            "Second",
            &json!({}),
        )
        .unwrap();
        let id2 = d2["id"].as_str().unwrap();

        assert_eq!(id1, id2, "dedup must return existing dispatch_id");
        assert_eq!(d2["status"], "pending");

        // Only 1 row in DB
        let conn = db.separate_conn().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_dispatches \
                 WHERE kanban_card_id = 'card-dup' AND dispatch_type = 'implementation' \
                 AND status IN ('pending', 'dispatched')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "only one pending dispatch must exist");
    }

    #[test]
    fn dedup_same_review_card_returns_existing_dispatch() {
        let (_repo, _override_guard) = setup_test_repo();
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-review-dup", "review");

        let d1 = create_dispatch(
            &db,
            &engine,
            "card-review-dup",
            "agent-1",
            "review",
            "First review",
            &json!({}),
        )
        .unwrap();
        let id1 = d1["id"].as_str().unwrap();

        let d2 = create_dispatch(
            &db,
            &engine,
            "card-review-dup",
            "agent-1",
            "review",
            "Second review",
            &json!({}),
        )
        .unwrap();
        let id2 = d2["id"].as_str().unwrap();

        assert_eq!(id1, id2, "review dedup must return existing dispatch_id");
        assert_eq!(d2["status"], "pending");

        let conn = db.separate_conn().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_dispatches \
                 WHERE kanban_card_id = 'card-review-dup' AND dispatch_type = 'review' \
                 AND status IN ('pending', 'dispatched')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "only one active review dispatch must exist");
    }

    #[test]
    fn dedup_same_card_different_type_allows_creation() {
        let (_repo, _override_guard) = setup_test_repo();
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-diff", "review");

        // Create review dispatch
        let d1 = create_dispatch(
            &db,
            &engine,
            "card-diff",
            "agent-1",
            "review",
            "Review",
            &json!({}),
        )
        .unwrap();

        // Create review-decision for same card → different type, should succeed
        let d2 = create_dispatch(
            &db,
            &engine,
            "card-diff",
            "agent-1",
            "review-decision",
            "Decision",
            &json!({}),
        )
        .unwrap();

        assert_ne!(
            d1["id"].as_str().unwrap(),
            d2["id"].as_str().unwrap(),
            "different types must create distinct dispatches"
        );
    }

    #[test]
    fn dedup_completed_dispatch_allows_new_creation() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-reopen", "ready");

        let d1 = create_dispatch(
            &db,
            &engine,
            "card-reopen",
            "agent-1",
            "implementation",
            "First",
            &json!({}),
        )
        .unwrap();
        let id1 = d1["id"].as_str().unwrap().to_string();

        // Complete the first dispatch
        seed_assistant_response_for_dispatch(&db, &id1, "implemented first attempt");
        complete_dispatch(&db, &engine, &id1, &json!({"output": "done"})).unwrap();

        // New dispatch of same type → should succeed (old one is completed)
        let d2 = create_dispatch(
            &db,
            &engine,
            "card-reopen",
            "agent-1",
            "implementation",
            "Second",
            &json!({}),
        )
        .unwrap();

        assert_ne!(
            id1,
            d2["id"].as_str().unwrap(),
            "completed dispatch must not block new creation"
        );
    }

    #[test]
    fn dedup_core_returns_reused_flag() {
        let db = test_db();
        seed_card(&db, "card-flag", "ready");

        let (id1, _, reused1) = create_dispatch_record_test(
            &db,
            "card-flag",
            "agent-1",
            "implementation",
            "First",
            &json!({}),
            DispatchCreateOptions::default(),
        )
        .unwrap();
        assert!(!reused1, "first creation must not be reused");

        let (id2, _, reused2) = create_dispatch_record_test(
            &db,
            "card-flag",
            "agent-1",
            "implementation",
            "Second",
            &json!({}),
            DispatchCreateOptions::default(),
        )
        .unwrap();
        assert!(reused2, "duplicate must be flagged as reused");
        assert_eq!(id1, id2);

        let conn = db.separate_conn().unwrap();
        assert_eq!(
            count_notify_outbox(&conn, &id1),
            1,
            "reused dispatch must not create a second notify outbox row"
        );
    }

    #[test]
    fn resolve_card_worktree_returns_none_without_issue_number() {
        let db = test_db();
        seed_card(&db, "card-no-issue", "ready");
        // Card has no github_issue_number → should return None
        let result = resolve_card_worktree(&db, "card-no-issue", None).unwrap();
        assert!(
            result.is_none(),
            "card without issue number should return None"
        );
    }

    #[test]
    fn resolve_card_worktree_ignores_target_repo_from_card_description() {
        let default_repo = init_test_repo();
        let default_repo_dir = default_repo.path().to_str().unwrap();
        let _env = DispatchEnvOverride::new(Some(default_repo_dir), None);

        let external_repo = init_test_repo();
        let external_repo_dir = external_repo.path().to_str().unwrap();
        let external_wt_dir = external_repo.path().join("wt-external-627");
        let external_wt_path = external_wt_dir.to_str().unwrap();
        run_git(
            external_repo_dir,
            &["worktree", "add", external_wt_path, "-b", "wt/external-627"],
        );
        git_commit(external_wt_path, "fix: external target repo (#627)");

        let db = test_db();
        seed_card(&db, "card-desc-target-repo", "ready");
        set_card_issue_number(&db, "card-desc-target-repo", 627);
        set_card_repo_id(&db, "card-desc-target-repo", "owner/missing");
        set_card_description(
            &db,
            "card-desc-target-repo",
            &format!("target_repo: {}", external_repo_dir),
        );

        let err = resolve_card_worktree(&db, "card-desc-target-repo", None)
            .expect_err("description target_repo must not bypass missing repo mapping");
        assert!(
            err.to_string()
                .contains("No local repo mapping for 'owner/missing'"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn non_review_dispatch_uses_card_repo_mapping_instead_of_default_repo() {
        let default_repo = init_test_repo();
        let default_repo_dir = default_repo.path().to_str().unwrap();
        let default_wt_dir = default_repo.path().join("wt-wrong-414");
        let default_wt_path = default_wt_dir.to_str().unwrap();
        run_git(
            default_repo_dir,
            &["worktree", "add", default_wt_path, "-b", "wt/wrong-414"],
        );
        std::fs::write(default_wt_dir.join("wrong.txt"), "wrong repo\n").unwrap();
        run_git(default_wt_path, &["add", "wrong.txt"]);
        run_git(default_wt_path, &["commit", "-m", "fix: wrong repo (#414)"]);

        let mapped_repo = init_test_repo();
        let mapped_repo_dir = mapped_repo.path().to_str().unwrap();
        let mapped_wt_dir = mapped_repo.path().join("wt-right-414");
        let mapped_wt_path = mapped_wt_dir.to_str().unwrap();
        run_git(
            mapped_repo_dir,
            &["worktree", "add", mapped_wt_path, "-b", "wt/right-414"],
        );
        std::fs::write(mapped_wt_dir.join("right.txt"), "right repo\n").unwrap();
        run_git(mapped_wt_path, &["add", "right.txt"]);
        run_git(mapped_wt_path, &["commit", "-m", "fix: mapped repo (#414)"]);

        let config_dir = write_repo_mapping_config(&[("owner/repo-b", mapped_repo_dir)]);
        let config_path = config_dir.path().join("agentdesk.yaml");
        let _env =
            DispatchEnvOverride::new(Some(default_repo_dir), Some(config_path.to_str().unwrap()));

        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-mapped", "ready");
        set_card_issue_number(&db, "card-mapped", 414);
        set_card_repo_id(&db, "card-mapped", "owner/repo-b");

        let dispatch = create_dispatch(
            &db,
            &engine,
            "card-mapped",
            "agent-1",
            "implementation",
            "Impl task",
            &json!({}),
        )
        .unwrap();

        let ctx = &dispatch["context"];
        let actual_wt_path = ctx["worktree_path"].as_str().unwrap();
        let canonical_actual = std::fs::canonicalize(actual_wt_path)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let canonical_expected = std::fs::canonicalize(mapped_wt_path)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let canonical_default = std::fs::canonicalize(default_wt_path)
            .unwrap()
            .to_string_lossy()
            .into_owned();

        assert_eq!(canonical_actual, canonical_expected);
        assert_eq!(ctx["worktree_branch"], "wt/right-414");
        assert_ne!(canonical_actual, canonical_default);
    }

    #[test]
    fn create_dispatch_rejects_missing_card_repo_mapping() {
        let default_repo = init_test_repo();
        let default_repo_dir = default_repo.path().to_str().unwrap();
        let _env = DispatchEnvOverride::new(Some(default_repo_dir), None);

        let db = test_db();
        seed_card(&db, "card-missing-mapping", "ready");
        set_card_issue_number(&db, "card-missing-mapping", 515);
        set_card_repo_id(&db, "card-missing-mapping", "owner/missing");

        let err = create_dispatch_record_test(
            &db,
            "card-missing-mapping",
            "agent-1",
            "implementation",
            "Should fail",
            &json!({}),
            DispatchCreateOptions::default(),
        )
        .expect_err("dispatch should fail when repo mapping is missing");

        assert!(
            err.to_string()
                .contains("No local repo mapping for 'owner/missing'"),
            "unexpected error: {err:#}"
        );

        let conn = db.separate_conn().unwrap();
        let dispatch_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = 'card-missing-mapping'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            dispatch_count, 0,
            "missing repo mapping must fail before INSERT"
        );
    }

    #[test]
    fn create_dispatch_uses_explicit_worktree_context_without_repo_mapping() {
        let (repo, _repo_override) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap();
        run_git(repo_dir, &["checkout", "-b", "wt/explicit-515"]);

        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-explicit-worktree", "review");
        set_card_issue_number(&db, "card-explicit-worktree", 515);
        set_card_repo_id(&db, "card-explicit-worktree", "owner/missing");

        let dispatch = create_dispatch(
            &db,
            &engine,
            "card-explicit-worktree",
            "agent-1",
            "create-pr",
            "Create PR",
            &json!({
                "worktree_path": repo_dir,
                "worktree_branch": "wt/explicit-515",
                "branch": "wt/explicit-515",
            }),
        )
        .expect("explicit worktree context should bypass repo mapping lookup");

        let ctx = &dispatch["context"];
        assert_eq!(ctx["worktree_path"], repo_dir);
        assert_eq!(ctx["worktree_branch"], "wt/explicit-515");
        assert_eq!(ctx["branch"], "wt/explicit-515");
    }

    #[test]
    fn create_dispatch_rejects_target_repo_from_card_description() {
        let default_repo = init_test_repo();
        let default_repo_dir = default_repo.path().to_str().unwrap();
        let _env = DispatchEnvOverride::new(Some(default_repo_dir), None);

        let external_repo = init_test_repo();
        let external_repo_dir = external_repo.path().to_str().unwrap();
        let external_wt_dir = external_repo.path().join("wt-target-627");
        let external_wt_path = external_wt_dir.to_str().unwrap();
        run_git(
            external_repo_dir,
            &["worktree", "add", external_wt_path, "-b", "wt/target-627"],
        );
        git_commit(external_wt_path, "fix: dispatch target repo (#627)");

        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-dispatch-target-repo", "ready");
        set_card_issue_number(&db, "card-dispatch-target-repo", 627);
        set_card_repo_id(&db, "card-dispatch-target-repo", "owner/missing");
        set_card_description(
            &db,
            "card-dispatch-target-repo",
            &format!("external repo path: {}", external_repo_dir),
        );

        let err = create_dispatch(
            &db,
            &engine,
            "card-dispatch-target-repo",
            "agent-1",
            "implementation",
            "Implement external repo task",
            &json!({}),
        )
        .expect_err("description target_repo must not bypass missing repo mapping");
        assert!(
            err.to_string()
                .contains("No local repo mapping for 'owner/missing'"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn non_review_dispatch_injects_worktree_context() {
        // When resolve_card_worktree returns None (no issue), the context
        // should pass through unchanged (no worktree_path/worktree_branch).
        let db = test_db();
        seed_card(&db, "card-ctx", "ready");
        let engine = test_engine(&db);

        let dispatch = create_dispatch(
            &db,
            &engine,
            "card-ctx",
            "agent-1",
            "implementation",
            "Impl task",
            &json!({"custom_key": "custom_value"}),
        )
        .unwrap();

        // context is returned as parsed JSON by query_dispatch_row
        let ctx = &dispatch["context"];
        assert_eq!(ctx["custom_key"], "custom_value");
        // No issue number → no worktree injection
        assert!(
            ctx.get("worktree_path").is_none(),
            "no worktree_path without issue"
        );
        assert!(
            ctx.get("worktree_branch").is_none(),
            "no worktree_branch without issue"
        );
    }

    #[test]
    fn review_context_reuses_latest_completed_work_dispatch_target() {
        let db = test_db();
        seed_card(&db, "card-review-target", "review");

        // #682: Use a dedicated test repo instead of resolve_repo_dir() to
        // avoid picking up another test's leaked RepoDirOverride (a tempdir
        // that may have been dropped, which would fail the new exact-HEAD
        // check in refresh_review_target_worktree). The test is exercising
        // the "recorded worktree still exists with matching HEAD" reuse path.
        let (repo, _repo_override) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap().to_string();
        let completed_commit = crate::services::platform::git_head_commit(&repo_dir).unwrap();
        let completed_branch = crate::services::platform::shell::git_branch_name(&repo_dir);

        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
             ) VALUES (
                'dispatch-review-target', 'card-review-target', 'agent-1', 'implementation', 'completed',
                'Done', ?1, ?2, datetime('now'), datetime('now')
             )",
            sqlite_test::params![
                serde_json::json!({}).to_string(),
                serde_json::json!({
                    "completed_worktree_path": repo_dir.clone(),
                    "completed_branch": completed_branch.clone(),
                    "completed_commit": completed_commit.clone(),
                })
                .to_string(),
            ],
        )
        .unwrap();
        drop(conn);

        let context = build_review_context(
            &db,
            "card-review-target",
            "agent-1",
            &json!({}),
            ReviewTargetTrust::Untrusted,
            TargetRepoSource::CardScopeDefault,
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&context).unwrap();

        assert_eq!(parsed["reviewed_commit"], completed_commit);
        assert_eq!(parsed["worktree_path"], repo_dir);
        if let Some(branch) = completed_branch {
            assert_eq!(parsed["branch"], branch);
        }
    }

    #[test]
    fn review_context_refreshes_deleted_completed_worktree_to_active_issue_worktree() {
        let db = test_db();
        seed_card(&db, "card-review-stale-worktree", "review");
        set_card_issue_number(&db, "card-review-stale-worktree", 682);

        let (repo, _repo_override) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap();
        let stale_wt_dir = repo.path().join("wt-682-stale");
        let stale_wt_path = stale_wt_dir.to_str().unwrap();

        run_git(
            repo_dir,
            &["worktree", "add", "-b", "wt/682-stale", stale_wt_path],
        );
        let reviewed_commit = git_commit(stale_wt_path, "fix: stale review target (#682)");
        run_git(repo_dir, &["worktree", "remove", "--force", stale_wt_path]);
        run_git(repo_dir, &["branch", "-D", "wt/682-stale"]);

        let live_wt_dir = repo.path().join("wt-682-live");
        let live_wt_path = live_wt_dir.to_str().unwrap();
        run_git(repo_dir, &["branch", "wt/682-live", &reviewed_commit]);
        run_git(repo_dir, &["worktree", "add", live_wt_path, "wt/682-live"]);

        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
             ) VALUES (
                'dispatch-review-stale-worktree', 'card-review-stale-worktree', 'agent-1', 'implementation', 'completed',
                'Done', ?1, ?2, datetime('now'), datetime('now')
             )",
            sqlite_test::params![
                serde_json::json!({}).to_string(),
                serde_json::json!({
                    "completed_worktree_path": stale_wt_path,
                    "completed_branch": "wt/682-stale",
                    "completed_commit": reviewed_commit.clone(),
                })
                .to_string(),
            ],
        )
        .unwrap();
        drop(conn);

        let context = build_review_context(
            &db,
            "card-review-stale-worktree",
            "agent-1",
            &json!({}),
            ReviewTargetTrust::Untrusted,
            TargetRepoSource::CardScopeDefault,
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&context).unwrap();
        let actual_worktree = std::fs::canonicalize(parsed["worktree_path"].as_str().unwrap())
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let expected_worktree = std::fs::canonicalize(live_wt_path)
            .unwrap()
            .to_string_lossy()
            .into_owned();

        assert_eq!(parsed["reviewed_commit"], reviewed_commit);
        assert_eq!(actual_worktree, expected_worktree);
        assert_eq!(parsed["branch"], "wt/682-live");
    }

    #[test]
    fn review_context_falls_back_to_repo_dir_when_completed_worktree_was_deleted() {
        let db = test_db();
        seed_card(&db, "card-review-stale-repo", "review");
        set_card_issue_number(&db, "card-review-stale-repo", 683);

        let (repo, _repo_override) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap();
        let reviewed_commit = git_commit(repo_dir, "fix: repo fallback review target (#683)");
        let stale_wt_path = repo.path().join("wt-683-missing");

        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
             ) VALUES (
                'dispatch-review-stale-repo', 'card-review-stale-repo', 'agent-1', 'implementation', 'completed',
                'Done', ?1, ?2, datetime('now'), datetime('now')
             )",
            sqlite_test::params![
                serde_json::json!({}).to_string(),
                serde_json::json!({
                    "completed_worktree_path": stale_wt_path,
                    "completed_branch": "wt/683-missing",
                    "completed_commit": reviewed_commit.clone(),
                })
                .to_string(),
            ],
        )
        .unwrap();
        drop(conn);

        let context = build_review_context(
            &db,
            "card-review-stale-repo",
            "agent-1",
            &json!({}),
            ReviewTargetTrust::Untrusted,
            TargetRepoSource::CardScopeDefault,
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&context).unwrap();

        assert_eq!(parsed["reviewed_commit"], reviewed_commit);
        assert_eq!(parsed["worktree_path"], repo_dir);
        assert_eq!(parsed["branch"], "main");
    }

    /// #682: An issue-less card (no github_issue_number) whose completed-work
    /// dispatch points at a worktree that has since been cleaned up must NOT
    /// leak the stale path into the review dispatch context. The refresh path
    /// should fall back to the card's repo_dir when the reviewed commit still
    /// lives there — matching the behavior already covered for issue-bearing
    /// cards (see review_context_falls_back_to_repo_dir_when_completed_worktree_was_deleted).
    ///
    /// Regression guard for the kunkunGames port (commit bad35a191) which
    /// bypassed refresh_review_target_worktree for issue-less cards and
    /// returned the recorded (stale) target unchanged.
    #[test]
    fn review_context_refreshes_stale_worktree_for_issueless_card() {
        let db = test_db();
        seed_card(&db, "card-review-no-issue", "review");
        // Deliberately do NOT set_card_issue_number — this is the edge case.

        let (repo, _repo_override) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap();
        let reviewed_commit = git_commit(repo_dir, "fix: issueless repo fallback (#682)");
        let stale_wt_path = repo.path().join("wt-682-deleted-no-issue");

        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
             ) VALUES (
                'dispatch-review-no-issue', 'card-review-no-issue', 'agent-1', 'implementation', 'completed',
                'Done', ?1, ?2, datetime('now'), datetime('now')
             )",
            sqlite_test::params![
                serde_json::json!({}).to_string(),
                serde_json::json!({
                    "completed_worktree_path": stale_wt_path,
                    "completed_branch": "wt/682-deleted-no-issue",
                    "completed_commit": reviewed_commit.clone(),
                })
                .to_string(),
            ],
        )
        .unwrap();
        drop(conn);

        let context = build_review_context(
            &db,
            "card-review-no-issue",
            "agent-1",
            &json!({}),
            ReviewTargetTrust::Untrusted,
            TargetRepoSource::CardScopeDefault,
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&context).unwrap();

        assert_eq!(parsed["reviewed_commit"], reviewed_commit);
        // Must NOT be the stale path — refresh should have dropped it in favor
        // of the repo_dir fallback (where the reviewed_commit lives).
        assert_ne!(
            parsed["worktree_path"].as_str(),
            Some(stale_wt_path.to_str().unwrap()),
            "issue-less card must not propagate stale worktree_path into review context"
        );
        assert_eq!(parsed["worktree_path"], repo_dir);
    }

    /// #682 (Codex review, [high]): An issue-less card whose completed-work
    /// dispatch recorded a `target_repo` pointing at an external repo must
    /// recover via that repo (not the card-scoped default) when its worktree
    /// is cleaned up. Prior refresh logic consulted only card-scoped repo
    /// resolution, so issue-less external-repo runs would lose their
    /// reviewed_commit after stale-worktree cleanup.
    #[test]
    fn review_context_refreshes_stale_worktree_for_issueless_card_via_target_repo() {
        let db = test_db();
        seed_card(&db, "card-review-no-issue-tr", "review");

        // Two repos: the default (setup_test_repo) repo and a separate
        // "external" repo that holds the reviewed commit. We deliberately do
        // NOT commit the reviewed commit into the default repo so that the
        // card-scoped fallback can't find it — only the target_repo path can.
        let (_default_repo, _repo_override) = setup_test_repo();
        let external_repo = tempfile::tempdir().unwrap();
        let external_repo_dir = external_repo.path().to_str().unwrap();
        run_git(external_repo_dir, &["init", "-b", "main"]);
        run_git(
            external_repo_dir,
            &["config", "user.email", "test@test.com"],
        );
        run_git(external_repo_dir, &["config", "user.name", "Test"]);
        run_git(
            external_repo_dir,
            &["commit", "--allow-empty", "-m", "initial"],
        );
        let reviewed_commit =
            git_commit(external_repo_dir, "fix: external repo review target (#682)");
        let stale_wt_path = external_repo.path().join("wt-682-external-deleted");

        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
             ) VALUES (
                'dispatch-review-no-issue-tr', 'card-review-no-issue-tr', 'agent-1', 'implementation', 'completed',
                'Done', ?1, ?2, datetime('now'), datetime('now')
             )",
            sqlite_test::params![
                serde_json::json!({ "target_repo": external_repo_dir }).to_string(),
                serde_json::json!({
                    "completed_worktree_path": stale_wt_path,
                    "completed_branch": "wt/682-external-deleted",
                    "completed_commit": reviewed_commit.clone(),
                })
                .to_string(),
            ],
        )
        .unwrap();
        drop(conn);

        let context = build_review_context(
            &db,
            "card-review-no-issue-tr",
            "agent-1",
            &json!({}),
            ReviewTargetTrust::Untrusted,
            TargetRepoSource::CardScopeDefault,
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&context).unwrap();

        assert_eq!(parsed["reviewed_commit"], reviewed_commit);
        // Must resolve via target_repo, not the card-scoped default.
        // Compare after canonicalization — macOS canonicalizes /var/folders
        // temp dirs to /private/var/folders.
        let actual_wt = parsed["worktree_path"].as_str().unwrap();
        let canonical_external = std::fs::canonicalize(external_repo_dir)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let canonical_actual = std::fs::canonicalize(actual_wt)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_eq!(canonical_actual, canonical_external);
    }

    /// #682 (Codex review, [medium]): If the recorded worktree_path still
    /// exists as a directory but has been recycled for a different checkout
    /// (so it no longer contains the reviewed_commit), refresh must drop it
    /// and fall through to recovery. Prior code accepted any existing
    /// directory without verifying the commit.
    #[test]
    fn review_context_drops_recycled_worktree_path_without_reviewed_commit() {
        let db = test_db();
        seed_card(&db, "card-review-recycled-wt", "review");
        set_card_issue_number(&db, "card-review-recycled-wt", 684);

        let (repo, _repo_override) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap();

        // Build the "recycled" worktree path: it exists as a directory but
        // tracks an unrelated branch (no reviewed_commit reachable from it).
        let recycled_wt_dir = repo.path().join("wt-684-recycled");
        let recycled_wt_path = recycled_wt_dir.to_str().unwrap();
        run_git(
            repo_dir,
            &["worktree", "add", "-b", "wt/684-recycled", recycled_wt_path],
        );
        let _unrelated_commit = git_commit(recycled_wt_path, "feat: unrelated recycled tree work");

        // The reviewed commit for *our* card only lives on the main repo dir
        // (not in the recycled worktree's branch).
        let reviewed_commit = git_commit(
            repo_dir,
            "fix: reviewed commit not in recycled worktree (#684)",
        );

        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
             ) VALUES (
                'dispatch-review-recycled', 'card-review-recycled-wt', 'agent-1', 'implementation', 'completed',
                'Done', ?1, ?2, datetime('now'), datetime('now')
             )",
            sqlite_test::params![
                serde_json::json!({}).to_string(),
                serde_json::json!({
                    "completed_worktree_path": recycled_wt_path,
                    "completed_branch": "wt/684-obsolete",
                    "completed_commit": reviewed_commit.clone(),
                })
                .to_string(),
            ],
        )
        .unwrap();
        drop(conn);

        let context = build_review_context(
            &db,
            "card-review-recycled-wt",
            "agent-1",
            &json!({}),
            ReviewTargetTrust::Untrusted,
            TargetRepoSource::CardScopeDefault,
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&context).unwrap();

        assert_eq!(parsed["reviewed_commit"], reviewed_commit);
        // Must NOT accept the recycled worktree_path — it exists but does
        // not contain the reviewed_commit.
        assert_ne!(
            parsed["worktree_path"].as_str(),
            Some(recycled_wt_path),
            "recycled worktree path (exists but missing reviewed_commit) must be dropped"
        );
        // Falls back to repo_dir where the reviewed_commit actually lives.
        assert_eq!(parsed["worktree_path"], repo_dir);
    }

    /// #682 (Codex round 2+3, [high]): An issue-bearing card whose recorded
    /// target_repo differs from the card's canonical repo must recover its
    /// worktree via target_repo, not card-scoped repo resolution. This test
    /// specifically exercises the resolve_card_worktree path (not the
    /// repo_dir fallback) by creating a LIVE issue worktree in the external
    /// repo with reviewed_commit as HEAD. If resolve_card_worktree failed
    /// to honor target_repo, recovery would fall through to the repo_dir
    /// branch and the worktree-path + HEAD assertions would catch it.
    #[test]
    fn review_context_refreshes_issue_bearing_external_target_repo_stale_worktree() {
        let db = test_db();
        seed_card(&db, "card-review-external-tr", "review");
        set_card_issue_number(&db, "card-review-external-tr", 685);

        let (_card_default_repo, _repo_override) = setup_test_repo();
        // Separate external repo — the completion actually ran here.
        let external_repo = tempfile::tempdir().unwrap();
        let external_repo_dir = external_repo.path().to_str().unwrap();
        run_git(external_repo_dir, &["init", "-b", "main"]);
        run_git(
            external_repo_dir,
            &["config", "user.email", "test@test.com"],
        );
        run_git(external_repo_dir, &["config", "user.name", "Test"]);
        run_git(
            external_repo_dir,
            &["commit", "--allow-empty", "-m", "initial"],
        );

        // Live issue worktree in the external repo whose branch name
        // encodes the issue (685) so find_worktree_for_issue picks it up.
        let live_wt_dir = external_repo.path().join("wt-685-live");
        let live_wt_path = live_wt_dir.to_str().unwrap();
        run_git(
            external_repo_dir,
            &["worktree", "add", "-b", "wt/685-live", live_wt_path],
        );
        let reviewed_commit = git_commit(
            live_wt_path,
            "fix: external issue target_repo refresh (#685)",
        );

        // Stale (deleted) worktree that the completion dispatch originally
        // ran on — must NOT be returned.
        let stale_wt_path = external_repo.path().join("wt-685-external-deleted");

        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
             ) VALUES (
                'dispatch-review-external-tr', 'card-review-external-tr', 'agent-1', 'implementation', 'completed',
                'Done', ?1, ?2, datetime('now'), datetime('now')
             )",
            sqlite_test::params![
                serde_json::json!({ "target_repo": external_repo_dir }).to_string(),
                serde_json::json!({
                    "completed_worktree_path": stale_wt_path,
                    "completed_branch": "wt/685-external-deleted",
                    "completed_commit": reviewed_commit.clone(),
                })
                .to_string(),
            ],
        )
        .unwrap();
        drop(conn);

        let context = build_review_context(
            &db,
            "card-review-external-tr",
            "agent-1",
            &json!({}),
            ReviewTargetTrust::Untrusted,
            TargetRepoSource::CardScopeDefault,
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&context).unwrap();

        assert_eq!(parsed["reviewed_commit"], reviewed_commit);
        let actual_wt = parsed["worktree_path"].as_str().unwrap();
        let canonical_live = std::fs::canonicalize(live_wt_path)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let canonical_actual = std::fs::canonicalize(actual_wt)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            canonical_actual, canonical_live,
            "issue-bearing external-repo review must resolve to the live issue worktree via target_repo (not the repo root fallback)"
        );
        // Verify the returned path actually has reviewed_commit as HEAD —
        // this is what makes the test bite even if target_repo injection
        // silently misrouted to repo_dir (repo_dir HEAD is just the
        // initial empty commit, not reviewed_commit).
        let head_output = GitCommand::new()
            .repo(actual_wt)
            .args(["rev-parse", "HEAD"])
            .run_output()
            .unwrap();
        let head = String::from_utf8_lossy(&head_output.stdout)
            .trim()
            .to_string();
        assert_eq!(
            head, reviewed_commit,
            "returned worktree must have reviewed_commit as HEAD"
        );
    }

    /// #682 (Codex round 2, [high]): A recorded worktree path that still
    /// exists but whose HEAD has advanced past reviewed_commit (follow-up
    /// work on the same branch) must NOT be reused as-is. The reviewer
    /// would otherwise see the descendant filesystem state, not the
    /// reviewed state. git_commit_exists and merge-base --is-ancestor both
    /// accept this case — only exact HEAD match is safe.
    #[test]
    fn review_context_rejects_recorded_worktree_with_descendant_head() {
        let db = test_db();
        seed_card(&db, "card-review-descendant", "review");
        set_card_issue_number(&db, "card-review-descendant", 686);

        let (repo, _repo_override) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap();

        let wt_dir = repo.path().join("wt-686-descendant");
        let wt_path = wt_dir.to_str().unwrap();
        run_git(
            repo_dir,
            &["worktree", "add", "-b", "wt/686-descendant", wt_path],
        );
        let reviewed_commit = git_commit(wt_path, "fix: reviewed commit on descendant wt (#686)");
        // HEAD advances past the reviewed commit — follow-up commit on the
        // same branch in the same worktree.
        let _descendant_commit = git_commit(wt_path, "chore: follow-up work beyond reviewed");

        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
             ) VALUES (
                'dispatch-review-descendant', 'card-review-descendant', 'agent-1', 'implementation', 'completed',
                'Done', ?1, ?2, datetime('now'), datetime('now')
             )",
            sqlite_test::params![
                serde_json::json!({}).to_string(),
                serde_json::json!({
                    "completed_worktree_path": wt_path,
                    "completed_branch": "wt/686-descendant",
                    "completed_commit": reviewed_commit.clone(),
                })
                .to_string(),
            ],
        )
        .unwrap();
        drop(conn);

        let context = build_review_context(
            &db,
            "card-review-descendant",
            "agent-1",
            &json!({}),
            ReviewTargetTrust::Untrusted,
            TargetRepoSource::CardScopeDefault,
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&context).unwrap();

        assert_eq!(parsed["reviewed_commit"], reviewed_commit);
        // Recorded path must NOT be reused — HEAD advanced past the reviewed
        // commit.
        assert_ne!(
            parsed["worktree_path"].as_str(),
            Some(wt_path),
            "recorded worktree with advanced HEAD must be rejected"
        );
        // #682 (Codex round 3, [high]): when a worktree_path IS emitted, it
        // must have HEAD==reviewed_commit. Otherwise the reviewer sees the
        // wrong filesystem state. Acceptable outcomes:
        //   (a) worktree_path is None (reviewer falls back to default repo)
        //   (b) worktree_path is a path with HEAD exactly at reviewed_commit
        // (c) worktree_path is the recorded wt_path — which is the failure
        //     this test guards against.
        if let Some(emitted) = parsed["worktree_path"].as_str() {
            let head_output = GitCommand::new()
                .repo(emitted)
                .args(["rev-parse", "HEAD"])
                .run_output()
                .unwrap();
            let head = String::from_utf8_lossy(&head_output.stdout)
                .trim()
                .to_string();
            assert_eq!(
                head, reviewed_commit,
                "if worktree_path is emitted after rejecting the recorded path, HEAD must be exactly reviewed_commit (got {} at {})",
                head, emitted
            );
        }
        _ = repo_dir; // silence unused warning when worktree_path is None
    }

    #[test]
    fn review_context_includes_merge_base_for_branch_review() {
        let db = test_db();
        seed_card(&db, "card-review-merge-base", "review");

        let (repo, _repo_override) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap();
        let fork_point = crate::services::platform::git_head_commit(repo_dir).unwrap();
        let wt_dir = repo.path().join("wt-542");
        let wt_path = wt_dir.to_str().unwrap();

        run_git(repo_dir, &["worktree", "add", wt_path, "-b", "wt/fix-542"]);
        let reviewed_commit = git_commit(wt_path, "fix: branch-only review target");
        let main_commit = git_commit(repo_dir, "chore: main advanced after fork");
        assert_ne!(fork_point, main_commit);

        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
             ) VALUES (
                'dispatch-review-merge-base', 'card-review-merge-base', 'agent-1', 'implementation', 'completed',
                'Done', ?1, ?2, datetime('now'), datetime('now')
             )",
            sqlite_test::params![
                serde_json::json!({}).to_string(),
                serde_json::json!({
                    "completed_worktree_path": wt_path,
                    "completed_branch": "wt/fix-542",
                    "completed_commit": reviewed_commit.clone(),
                })
                .to_string(),
            ],
        )
        .unwrap();
        drop(conn);

        let context = build_review_context(
            &db,
            "card-review-merge-base",
            "agent-1",
            &json!({}),
            ReviewTargetTrust::Untrusted,
            TargetRepoSource::CardScopeDefault,
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&context).unwrap();

        assert_eq!(parsed["reviewed_commit"], reviewed_commit);
        assert_eq!(parsed["branch"], "wt/fix-542");
        assert_eq!(parsed["merge_base"], fork_point);
    }

    #[test]
    fn review_context_skips_missing_merge_base_for_unknown_branch() {
        let mut obj = serde_json::Map::new();
        let (repo, _repo_override) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap();
        let reviewed_commit = crate::services::platform::git_head_commit(repo_dir).unwrap();
        obj.insert("worktree_path".to_string(), json!(repo_dir));
        obj.insert("branch".to_string(), json!("missing-branch"));
        obj.insert("reviewed_commit".to_string(), json!(reviewed_commit));

        inject_review_merge_base_context(&mut obj);

        assert!(
            !obj.contains_key("merge_base"),
            "missing git merge-base must leave merge_base absent"
        );
    }

    #[test]
    fn review_context_accepts_latest_work_dispatch_commit_for_same_issue() {
        let db = test_db();
        seed_card(&db, "card-review-match", "review");
        set_card_issue_number(&db, "card-review-match", 305);

        let (repo, _repo_override) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap();
        let matching_commit = git_commit(repo_dir, "fix: target commit (#305)");

        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
             ) VALUES (
                'dispatch-review-match', 'card-review-match', 'agent-1', 'implementation', 'completed',
                'Done', ?1, ?2, datetime('now'), datetime('now')
             )",
            sqlite_test::params![
                serde_json::json!({}).to_string(),
                serde_json::json!({
                    "completed_worktree_path": repo_dir,
                    "completed_branch": "main",
                    "completed_commit": matching_commit.clone(),
                })
                .to_string(),
            ],
        )
        .unwrap();
        drop(conn);

        let context = build_review_context(
            &db,
            "card-review-match",
            "agent-1",
            &json!({}),
            ReviewTargetTrust::Untrusted,
            TargetRepoSource::CardScopeDefault,
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&context).unwrap();

        assert_eq!(parsed["reviewed_commit"], matching_commit);
        assert_eq!(parsed["worktree_path"], repo_dir);
        assert_eq!(parsed["branch"], "main");
        assert!(
            parsed.get("merge_base").is_none(),
            "main branch reviews must not inject an empty merge-base diff"
        );
    }

    #[test]
    fn review_context_rejects_latest_work_dispatch_commit_from_other_issue() {
        let db = test_db();
        seed_card(&db, "card-review-mismatch", "review");
        set_card_issue_number(&db, "card-review-mismatch", 305);

        let (repo, _repo_override) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap();
        let expected_commit = git_commit(repo_dir, "fix: target commit (#305)");
        let poisoned_commit = git_commit(repo_dir, "chore: unrelated (#999)");

        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
             ) VALUES (
                'dispatch-review-mismatch', 'card-review-mismatch', 'agent-1', 'implementation', 'completed',
                'Done', ?1, ?2, datetime('now'), datetime('now')
             )",
            sqlite_test::params![
                serde_json::json!({}).to_string(),
                serde_json::json!({
                    "completed_worktree_path": repo_dir,
                    "completed_branch": "main",
                    "completed_commit": poisoned_commit.clone(),
                })
                .to_string(),
            ],
        )
        .unwrap();
        drop(conn);

        let context = build_review_context(
            &db,
            "card-review-mismatch",
            "agent-1",
            &json!({}),
            ReviewTargetTrust::Untrusted,
            TargetRepoSource::CardScopeDefault,
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&context).unwrap();

        assert_eq!(parsed["reviewed_commit"], expected_commit);
        assert_ne!(parsed["reviewed_commit"], poisoned_commit);
        assert_eq!(parsed["worktree_path"], repo_dir);
        assert_eq!(parsed["branch"], "main");
    }

    #[test]
    fn review_context_skips_poisoned_non_main_worktree_when_latest_commit_does_not_match_issue() {
        let db = test_db();
        seed_card(&db, "card-review-worktree-fallback", "review");
        set_card_issue_number(&db, "card-review-worktree-fallback", 320);

        let (repo, _repo_override) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap();
        let expected_commit = git_commit(repo_dir, "fix: target commit (#320)");
        let wt_dir = repo.path().join("wt-320");
        let wt_path = wt_dir.to_str().unwrap();

        run_git(
            repo_dir,
            &["worktree", "add", wt_path, "-b", "wt/320-phase6"],
        );
        let poisoned_commit = git_commit(wt_path, "chore: unrelated worktree drift (#999)");
        assert_ne!(
            expected_commit, poisoned_commit,
            "poisoned worktree head must differ from the issue commit fallback"
        );

        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
             ) VALUES (
                'dispatch-review-worktree-fallback', 'card-review-worktree-fallback', 'agent-1', 'implementation', 'completed',
                'Done', ?1, ?2, datetime('now'), datetime('now')
             )",
            sqlite_test::params![
                serde_json::json!({}).to_string(),
                serde_json::json!({
                    "completed_worktree_path": wt_path,
                    "completed_branch": "wt/320-phase6",
                    "completed_commit": poisoned_commit.clone(),
                })
                .to_string(),
            ],
        )
        .unwrap();
        drop(conn);

        let context = build_review_context(
            &db,
            "card-review-worktree-fallback",
            "agent-1",
            &json!({}),
            ReviewTargetTrust::Untrusted,
            TargetRepoSource::CardScopeDefault,
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&context).unwrap();

        assert_eq!(parsed["worktree_path"], repo_dir);
        assert_eq!(parsed["branch"], "main");
        assert_eq!(parsed["reviewed_commit"], expected_commit);
        assert_ne!(parsed["reviewed_commit"], poisoned_commit);
    }

    #[test]
    fn review_context_rejects_repo_head_fallback_when_repo_root_is_dirty() {
        let db = test_db();
        seed_card(&db, "card-review-dirty-root", "review");

        let (repo, _repo_override) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap();
        std::fs::write(repo.path().join("tracked.txt"), "baseline\n").unwrap();
        run_git(repo_dir, &["add", "tracked.txt"]);
        run_git(repo_dir, &["commit", "-m", "feat: add tracked file"]);
        std::fs::write(repo.path().join("tracked.txt"), "dirty\n").unwrap();

        let err = build_review_context(
            &db,
            "card-review-dirty-root",
            "agent-1",
            &json!({}),
            ReviewTargetTrust::Untrusted,
            TargetRepoSource::CardScopeDefault,
        )
        .expect_err("dirty repo root must block repo HEAD fallback");

        assert!(
            err.to_string()
                .contains("repo-root HEAD fallback is unsafe while tracked changes exist"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn review_context_rejects_commitless_completed_work_when_repo_root_is_dirty() {
        let db = test_db();
        seed_card(&db, "card-review-dirty-completion", "review");

        let (repo, _repo_override) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap();
        std::fs::write(repo.path().join("tracked.txt"), "baseline\n").unwrap();
        run_git(repo_dir, &["add", "tracked.txt"]);
        run_git(repo_dir, &["commit", "-m", "feat: add tracked file"]);
        std::fs::write(repo.path().join("tracked.txt"), "dirty\n").unwrap();

        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
             ) VALUES (
                'dispatch-review-dirty-completion', 'card-review-dirty-completion', 'agent-1', 'implementation', 'completed',
                'Implemented without commit', ?1, ?2, datetime('now'), datetime('now')
             )",
            sqlite_test::params![
                serde_json::json!({}).to_string(),
                serde_json::json!({}).to_string(),
            ],
        )
        .unwrap();
        drop(conn);

        let err = build_review_context(
            &db,
            "card-review-dirty-completion",
            "agent-1",
            &json!({}),
            ReviewTargetTrust::Untrusted,
            TargetRepoSource::CardScopeDefault,
        )
        .expect_err("dirty repo root must block fallback after commitless completion");

        assert!(
            err.to_string()
                .contains("repo-root HEAD fallback is unsafe while tracked changes exist"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn review_context_skips_repo_head_fallback_after_rejected_external_work_target() {
        let db = test_db();
        seed_card(&db, "card-review-external-reject", "review");
        set_card_issue_number(&db, "card-review-external-reject", 595);

        let (repo, _repo_override) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap();
        let default_head = crate::services::platform::git_head_commit(repo_dir).unwrap();

        let external_repo = tempfile::tempdir().unwrap();
        let external_dir = external_repo.path().to_str().unwrap();
        run_git(external_dir, &["init", "-b", "main"]);
        run_git(external_dir, &["config", "user.email", "test@test.com"]);
        run_git(external_dir, &["config", "user.name", "Test"]);
        run_git(external_dir, &["commit", "--allow-empty", "-m", "initial"]);
        run_git(
            external_dir,
            &["checkout", "-b", "codex/595-agentdesk-aiinstructions"],
        );
        let external_commit = git_commit(
            external_dir,
            "fix: shrink aiInstructions in external repo (#595)",
        );

        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
             ) VALUES (
                'dispatch-review-external-reject', 'card-review-external-reject', 'agent-1', 'implementation', 'completed',
                'Done', ?1, ?2, datetime('now'), datetime('now')
             )",
            sqlite_test::params![
                serde_json::json!({}).to_string(),
                serde_json::json!({
                    "completed_worktree_path": external_dir,
                    "completed_branch": "codex/595-agentdesk-aiinstructions",
                    "completed_commit": external_commit.clone(),
                })
                .to_string(),
            ],
        )
        .unwrap();
        drop(conn);

        let context = build_review_context(
            &db,
            "card-review-external-reject",
            "agent-1",
            &json!({}),
            ReviewTargetTrust::Untrusted,
            TargetRepoSource::CardScopeDefault,
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&context).unwrap();

        assert!(
            parsed.get("reviewed_commit").is_none(),
            "rejected external work target must not fall back to repo HEAD"
        );
        assert!(
            parsed.get("branch").is_none(),
            "rejected external work target must not inject a fake branch"
        );
        assert!(
            parsed.get("worktree_path").is_none(),
            "rejected external work target must not inject the default repo path"
        );
        assert!(
            parsed.get("merge_base").is_none(),
            "rejected external work target must not emit a fake diff range"
        );
        assert_eq!(
            parsed["review_target_reject_reason"],
            "latest_work_target_issue_mismatch"
        );
        assert!(
            parsed["review_target_warning"]
                .as_str()
                .unwrap_or_default()
                .contains("브랜치 정보 없음"),
            "warning must tell reviewers that manual lookup is required"
        );
        assert_ne!(
            parsed["reviewed_commit"],
            json!(default_head),
            "default repo HEAD must not be injected after rejection"
        );
    }

    #[test]
    fn review_context_accepts_external_work_target_when_target_repo_is_in_context() {
        let db = test_db();
        seed_card(&db, "card-review-external-accept", "review");
        set_card_issue_number(&db, "card-review-external-accept", 627);
        set_card_repo_id(&db, "card-review-external-accept", "owner/missing");

        let default_repo = init_test_repo();
        let default_repo_dir = default_repo.path().to_str().unwrap();
        let _env = DispatchEnvOverride::new(Some(default_repo_dir), None);

        let external_repo = init_test_repo();
        let external_dir = external_repo.path().to_str().unwrap();
        run_git(external_dir, &["checkout", "-b", "codex/627-target-repo"]);
        let external_commit = git_commit(external_dir, "fix: cross repo review target (#627)");
        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
             ) VALUES (
                'dispatch-review-external-accept', 'card-review-external-accept', 'agent-1', 'implementation', 'completed',
                'Done', ?1, ?2, datetime('now'), datetime('now')
             )",
            sqlite_test::params![
                serde_json::json!({}).to_string(),
                serde_json::json!({
                    "completed_worktree_path": external_dir,
                    "completed_branch": "codex/627-target-repo",
                    "completed_commit": external_commit.clone(),
                })
                .to_string(),
            ],
        )
        .unwrap();
        drop(conn);

        // #761 (Codex round-2): Trusted internal callers may pre-seed
        // `target_repo` to steer review at an external repo. Public API
        // callers cannot — the trust signal is an out-of-band enum on
        // `build_review_context`, NOT a JSON field on the context payload.
        // The API-sourced path (`POST /api/dispatches` →
        // `create_dispatch_core_internal` → `build_review_context` with
        // `ReviewTargetTrust::Untrusted`) always strips review-target fields
        // regardless of what the client sent. See
        // `dispatch_create_review_strips_untrusted_review_target_fields_from_context`
        // in `server/routes/routes_tests.rs` for the API-level negative case.
        let context = build_review_context(
            &db,
            "card-review-external-accept",
            "agent-1",
            &json!({ "target_repo": external_dir }),
            ReviewTargetTrust::Trusted,
            TargetRepoSource::CallerSupplied,
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&context).unwrap();
        let actual_worktree = std::fs::canonicalize(parsed["worktree_path"].as_str().unwrap())
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let actual_target_repo = std::fs::canonicalize(parsed["target_repo"].as_str().unwrap())
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let expected_external_dir = std::fs::canonicalize(external_dir)
            .unwrap()
            .to_string_lossy()
            .into_owned();

        assert_eq!(parsed["reviewed_commit"], external_commit);
        assert_eq!(parsed["branch"], "codex/627-target-repo");
        assert_eq!(actual_worktree, expected_external_dir);
        assert_eq!(actual_target_repo, expected_external_dir);
        // #761 (Codex round-2): Even though trust is now an out-of-band Rust
        // parameter, defensively confirm no legacy `_trusted_review_target`
        // JSON key slips through if some upstream caller ever attached one.
        assert!(
            parsed.get("_trusted_review_target").is_none(),
            "legacy trusted sentinel must not appear in the persisted dispatch context"
        );
    }

    /// #762 (A): If the historical work dispatch ran against an external
    /// `target_repo` whose reviewed commit can no longer be recovered, the
    /// review must NOT silently fall back to the card's canonical worktree.
    /// Prior behavior consulted `resolve_card_worktree`/
    /// `resolve_card_issue_commit_target` with `ctx_snapshot` (card-scoped),
    /// which silently redirected the reviewer to unrelated code whenever the
    /// card had its own live issue worktree. Fail closed instead.
    #[test]
    fn review_context_fails_closed_when_external_target_repo_is_unrecoverable() {
        let db = test_db();
        seed_card(&db, "card-review-762-external-fail", "review");
        set_card_issue_number(&db, "card-review-762-external-fail", 762);

        // Card's canonical repo: this is where the silent-redirect bug would
        // have sent the reviewer. It has a LIVE worktree for issue 762.
        let (card_repo, _repo_override) = setup_test_repo();
        let card_repo_dir = card_repo.path().to_str().unwrap();
        set_card_repo_id(&db, "card-review-762-external-fail", card_repo_dir);
        let card_live_wt_dir = card_repo.path().join("wt-762-card-live");
        let card_live_wt_path = card_live_wt_dir.to_str().unwrap();
        run_git(
            card_repo_dir,
            &[
                "worktree",
                "add",
                "-b",
                "wt/762-card-live",
                card_live_wt_path,
            ],
        );
        let _card_live_commit = git_commit(
            card_live_wt_path,
            "feat: unrelated ongoing work on card issue (#762)",
        );

        // External repo where the historical work ran. We create the
        // reviewed_commit here (subject references #762 so the validity
        // check passes) but then blow the whole directory away — this is
        // the "external repo unrecoverable" scenario.
        let external_repo = tempfile::tempdir().unwrap();
        let external_repo_dir = external_repo.path().to_str().unwrap();
        run_git(external_repo_dir, &["init", "-b", "main"]);
        run_git(
            external_repo_dir,
            &["config", "user.email", "test@test.com"],
        );
        run_git(external_repo_dir, &["config", "user.name", "Test"]);
        run_git(
            external_repo_dir,
            &["commit", "--allow-empty", "-m", "initial"],
        );
        let reviewed_commit = git_commit(
            external_repo_dir,
            "fix: external unrecoverable commit (#762)",
        );

        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
             ) VALUES (
                'dispatch-review-762-external-fail', 'card-review-762-external-fail', 'agent-1', 'implementation', 'completed',
                'Done', ?1, ?2, datetime('now'), datetime('now')
             )",
            sqlite_test::params![
                serde_json::json!({ "target_repo": external_repo_dir }).to_string(),
                serde_json::json!({
                    "completed_worktree_path":
                        external_repo.path().join("wt-762-external-deleted"),
                    "completed_branch": "wt/762-external-deleted",
                    "completed_commit": reviewed_commit.clone(),
                })
                .to_string(),
            ],
        )
        .unwrap();
        drop(conn);

        // Make the external repo genuinely unrecoverable. After this, the
        // path exists but is not a git repo, so resolve_repo_dir_for_target
        // errors and refresh cannot locate reviewed_commit via target_repo
        // or via the card repo (card repo never had that commit).
        std::fs::remove_dir_all(external_repo_dir).unwrap();

        let context = build_review_context(
            &db,
            "card-review-762-external-fail",
            "agent-1",
            &json!({}),
            ReviewTargetTrust::Trusted,
            TargetRepoSource::CardScopeDefault,
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&context).unwrap();

        assert!(
            parsed.get("reviewed_commit").is_none(),
            "unrecoverable external target_repo must not emit a reviewed_commit from card scope"
        );
        assert!(
            parsed.get("worktree_path").is_none(),
            "unrecoverable external target_repo must not redirect to card's live issue worktree: got {:?}",
            parsed.get("worktree_path")
        );
        assert!(
            parsed.get("branch").is_none(),
            "unrecoverable external target_repo must not inject a card-scoped branch"
        );
        assert_eq!(
            parsed["review_target_reject_reason"],
            "external_target_repo_unrecoverable"
        );
        assert!(
            parsed["review_target_warning"]
                .as_str()
                .unwrap_or_default()
                .contains("target_repo"),
            "warning must mention target_repo so operators can investigate"
        );
        // The original external target_repo is preserved on the context so
        // downstream prompt builders can surface it to the reviewer even
        // when the commit itself cannot be located.
        assert_eq!(parsed["target_repo"], external_repo_dir);
    }

    /// #762 round-2 (A): when the dispatch-core path pre-injects the card's
    /// `target_repo` into the context before calling `build_review_context`,
    /// the fail-closed filter for unrecoverable external target_repos must
    /// STILL engage. Previous behavior snapshotted `context["target_repo"]`
    /// after the pre-injection and treated every dispatch as
    /// caller-supplied — silently disabling the filter and letting
    /// card-scoped fallbacks redirect the reviewer to unrelated code.
    #[test]
    fn create_dispatch_core_review_path_still_fails_closed_on_unrecoverable_external_target_repo() {
        let db = test_db();
        seed_card(&db, "card-review-762-a-core", "review");
        set_card_issue_number(&db, "card-review-762-a-core", 762);

        // Card's canonical repo — carries a LIVE worktree for the same
        // issue. A silent redirect would point the reviewer here.
        let (card_repo, _repo_override) = setup_test_repo();
        let card_repo_dir = card_repo.path().to_str().unwrap();
        set_card_repo_id(&db, "card-review-762-a-core", card_repo_dir);
        let card_live_wt = card_repo.path().join("wt-762-a-core-live");
        let card_live_wt_path = card_live_wt.to_str().unwrap();
        run_git(
            card_repo_dir,
            &[
                "worktree",
                "add",
                "-b",
                "wt/762-a-core-live",
                card_live_wt_path,
            ],
        );
        let _ = git_commit(card_live_wt_path, "feat: unrelated live card work (#762)");

        // External repo where the historical work ran — then deleted to
        // simulate the unrecoverable case.
        let external_repo = tempfile::tempdir().unwrap();
        let external_repo_dir = external_repo.path().to_str().unwrap();
        run_git(external_repo_dir, &["init", "-b", "main"]);
        run_git(
            external_repo_dir,
            &["config", "user.email", "test@test.com"],
        );
        run_git(external_repo_dir, &["config", "user.name", "Test"]);
        run_git(
            external_repo_dir,
            &["commit", "--allow-empty", "-m", "initial"],
        );
        let reviewed_commit = git_commit(
            external_repo_dir,
            "fix: external unrecoverable from core path (#762)",
        );

        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
             ) VALUES (
                'dispatch-review-762-a-core', 'card-review-762-a-core', 'agent-1', 'implementation', 'completed',
                'Done', ?1, ?2, datetime('now'), datetime('now')
             )",
            sqlite_test::params![
                serde_json::json!({ "target_repo": external_repo_dir }).to_string(),
                serde_json::json!({
                    "completed_worktree_path":
                        external_repo.path().join("wt-762-a-core-deleted"),
                    "completed_branch": "wt/762-a-core-deleted",
                    "completed_commit": reviewed_commit.clone(),
                })
                .to_string(),
            ],
        )
        .unwrap();
        drop(conn);

        std::fs::remove_dir_all(external_repo_dir).unwrap();

        // Invoke the real production path. The caller passes NO target_repo
        // override — `dispatch_create` will inject `card.repo_id`
        // (`card_repo_dir`) before calling `build_review_context`.
        let (dispatch_id, _, _) = create_dispatch_record_test(
            &db,
            "card-review-762-a-core",
            "agent-1",
            "review",
            "Review dispatch for 762-a",
            &json!({}),
            DispatchCreateOptions::default(),
        )
        .unwrap();

        let conn = db.separate_conn().unwrap();
        let context_str: String = conn
            .query_row(
                "SELECT context FROM task_dispatches WHERE id = ?1",
                [&dispatch_id],
                |row| row.get(0),
            )
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&context_str).unwrap();

        assert_eq!(
            parsed["review_target_reject_reason"], "external_target_repo_unrecoverable",
            "full dispatch_create → build_review_context path must fail closed even though upstream injects card.repo_id as target_repo; got context: {parsed:#?}"
        );
        assert!(
            parsed.get("worktree_path").is_none(),
            "must not silently redirect to card's live worktree"
        );
        assert!(
            parsed.get("reviewed_commit").is_none(),
            "must not emit a reviewed_commit from card scope after rejection"
        );
        assert_eq!(
            parsed["target_repo"], external_repo_dir,
            "historical external target_repo must be preserved (not replaced with card.repo_id)"
        );
    }

    /// #762 round-2 (A) positive case: when a TRUSTED internal caller supplies a
    /// `target_repo`, `build_review_context` must honor it and use card-scoped
    /// fallbacks against that repo. The provenance marker
    /// (`TargetRepoSource::CallerSupplied`) is what distinguishes this from the
    /// auto-injection case and bypasses the unrecoverable-external fail-closed
    /// filter.
    ///
    /// #761 merge note: under the merged design, the production
    /// the dispatch-core → `build_review_context` path always passes
    /// `ReviewTargetTrust::Untrusted`, which strips caller-supplied
    /// `target_repo` regardless of provenance. Trusted internal callers that
    /// legitimately pre-seed `target_repo` must therefore bypass
    /// the dispatch-core helper and invoke `build_review_context` directly with
    /// `ReviewTargetTrust::Trusted` + `TargetRepoSource::CallerSupplied`, which
    /// is exactly what this test now exercises.
    #[test]
    fn create_dispatch_core_review_path_honors_caller_supplied_target_repo() {
        let db = test_db();
        seed_card(&db, "card-review-762-a-caller", "review");
        set_card_issue_number(&db, "card-review-762-a-caller", 627);
        set_card_repo_id(&db, "card-review-762-a-caller", "owner/missing");

        let default_repo = init_test_repo();
        let default_repo_dir = default_repo.path().to_str().unwrap();
        let _env = DispatchEnvOverride::new(Some(default_repo_dir), None);

        let external_repo = init_test_repo();
        let external_dir = external_repo.path().to_str().unwrap();
        run_git(
            external_dir,
            &["checkout", "-b", "codex/627-caller-supplied"],
        );
        let external_commit = git_commit(external_dir, "fix: caller-supplied target repo (#627)");
        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
             ) VALUES (
                'dispatch-review-762-a-caller', 'card-review-762-a-caller', 'agent-1', 'implementation', 'completed',
                'Done', ?1, ?2, datetime('now'), datetime('now')
             )",
            sqlite_test::params![
                serde_json::json!({}).to_string(),
                serde_json::json!({
                    "completed_worktree_path": external_dir,
                    "completed_branch": "codex/627-caller-supplied",
                    "completed_commit": external_commit.clone(),
                })
                .to_string(),
            ],
        )
        .unwrap();
        drop(conn);

        // Trusted internal invocation — simulates an in-process Rust caller
        // that legitimately pre-pins `target_repo`. Public API clients cannot
        // reach this path (see #761: dispatch_create_core_internal always
        // passes ReviewTargetTrust::Untrusted).
        let context_str = build_review_context(
            &db,
            "card-review-762-a-caller",
            "agent-1",
            &json!({ "target_repo": external_dir }),
            ReviewTargetTrust::Trusted,
            TargetRepoSource::CallerSupplied,
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&context_str).unwrap();

        assert_eq!(parsed["reviewed_commit"], external_commit);
        assert_eq!(parsed["branch"], "codex/627-caller-supplied");
        assert!(
            parsed.get("review_target_reject_reason").is_none(),
            "caller-supplied target_repo must not trigger the unrecoverable filter: {parsed:#?}"
        );
    }

    /// #762 round-2 (C): when the historical work dispatch recorded a
    /// `target_repo` we cannot resolve AND the card has no resolvable
    /// `repo_id`, `historical_target_repo_differs_from_card` must treat the
    /// situation as divergent. Previous behavior returned `false` (not
    /// divergent), which let `resolve_repo_dir_for_target(None)` redirect to
    /// the default repo — silent external redirect.
    #[test]
    fn review_context_fails_closed_when_both_work_and_card_target_repos_are_unresolvable() {
        let db = test_db();
        seed_card(&db, "card-review-762-c-none-none", "review");
        set_card_issue_number(&db, "card-review-762-c-none-none", 762);
        // NOTE: intentionally DO NOT set_card_repo_id — card has no
        // resolvable repo_id, so `card_repo_id` side of the comparison is
        // `None`.

        // Set the default repo so card-scoped fallback would resolve into
        // an unrelated repo if the bug triggers.
        let default_repo = init_test_repo();
        let default_repo_dir = default_repo.path().to_str().unwrap();
        let _env = DispatchEnvOverride::new(Some(default_repo_dir), None);
        // Seed an unrelated commit in the default repo — if the silent
        // redirect happens, reviewed_commit would be this unrelated HEAD.
        let default_head = git_commit(default_repo_dir, "chore: unrelated default repo work");

        // Historical dispatch recorded a `target_repo` pointing at a
        // directory that does NOT resolve to any known repo (doesn't
        // exist). This makes `normalized_target_repo_path(work)` return
        // None, and card is None → (None, None).
        let bogus_external = "/tmp/agentdesk-762-nonexistent-external-xyz";
        let reviewed_commit = default_head.clone(); // any sha; won't be used

        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
             ) VALUES (
                'dispatch-review-762-c-none-none', 'card-review-762-c-none-none', 'agent-1', 'implementation', 'completed',
                'Done', ?1, ?2, datetime('now'), datetime('now')
             )",
            sqlite_test::params![
                serde_json::json!({ "target_repo": bogus_external }).to_string(),
                serde_json::json!({
                    "completed_worktree_path": format!("{bogus_external}/wt-gone"),
                    "completed_branch": "wt/762-c-gone",
                    "completed_commit": reviewed_commit.clone(),
                })
                .to_string(),
            ],
        )
        .unwrap();
        drop(conn);

        let context = build_review_context(
            &db,
            "card-review-762-c-none-none",
            "agent-1",
            &json!({}),
            ReviewTargetTrust::Trusted,
            TargetRepoSource::CardScopeDefault,
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&context).unwrap();

        assert_eq!(
            parsed["review_target_reject_reason"], "external_target_repo_unrecoverable",
            "when work_target_repo is unresolvable AND card has no resolvable repo_id, must fail closed instead of redirecting to default repo: {parsed:#?}"
        );
        assert!(
            parsed.get("reviewed_commit").is_none(),
            "must not redirect to default repo HEAD"
        );
        assert_ne!(
            parsed.get("reviewed_commit").and_then(|v| v.as_str()),
            Some(default_head.as_str()),
            "default repo HEAD must never be injected when both sides unresolvable"
        );
    }

    #[test]
    fn review_context_allows_explicit_noop_latest_work_dispatch_when_review_mode_is_noop_verification()
     {
        let db = test_db();
        seed_card(&db, "card-review-noop", "review");

        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
             ) VALUES (
                'dispatch-review-noop', 'card-review-noop', 'agent-1', 'implementation', 'completed',
                'No changes needed', ?1, ?2, datetime('now'), datetime('now')
             )",
            sqlite_test::params![
                serde_json::json!({}).to_string(),
                serde_json::json!({
                    "work_outcome": "noop",
                    "completed_without_changes": true,
                    "notes": "spec already satisfied",
                })
                .to_string(),
            ],
        )
        .unwrap();
        drop(conn);

        let context = build_review_context(
            &db,
            "card-review-noop",
            "agent-1",
            &json!({
                "review_mode": "noop_verification",
                "noop_reason": "spec already satisfied"
            }),
            ReviewTargetTrust::Untrusted,
            TargetRepoSource::CardScopeDefault,
        )
        .expect("explicit noop work should still create a noop_verification review dispatch");
        let parsed: serde_json::Value =
            serde_json::from_str(&context).expect("review context must parse");
        assert_eq!(parsed["review_mode"], "noop_verification");
        assert_eq!(parsed["noop_reason"], "spec already satisfied");
    }

    #[test]
    fn review_context_recovers_issue_branch_from_reviewed_commit_membership() {
        let db = test_db();
        seed_card(&db, "card-review-contains-branch", "review");
        set_card_issue_number(&db, "card-review-contains-branch", 610);

        let (repo, _repo_override) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap();
        run_git(repo_dir, &["checkout", "-b", "feat/610-review"]);
        let reviewed_commit = git_commit(repo_dir, "fix: recover branch from commit (#610)");
        let fork_point = run_git(repo_dir, &["rev-parse", "HEAD^"]);
        let fork_point = String::from_utf8_lossy(&fork_point.stdout)
            .trim()
            .to_string();
        run_git(repo_dir, &["checkout", "main"]);

        let context = build_review_context(
            &db,
            "card-review-contains-branch",
            "agent-1",
            &json!({}),
            ReviewTargetTrust::Untrusted,
            TargetRepoSource::CardScopeDefault,
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&context).unwrap();

        assert_eq!(parsed["reviewed_commit"], reviewed_commit);
        assert_eq!(parsed["worktree_path"], repo_dir);
        assert_eq!(parsed["branch"], "feat/610-review");
        assert_eq!(parsed["merge_base"], fork_point);
    }

    #[test]
    fn review_context_includes_quality_checklist_and_verdict_guidance() {
        let db = test_db();
        seed_card(&db, "card-review-quality", "review");

        let (repo, _repo_override) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap();
        let completed_commit = crate::services::platform::git_head_commit(repo_dir).unwrap();

        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
             ) VALUES (
                'dispatch-review-quality', 'card-review-quality', 'agent-1', 'implementation', 'completed',
                'Done', ?1, ?2, datetime('now'), datetime('now')
             )",
            sqlite_test::params![
                serde_json::json!({}).to_string(),
                serde_json::json!({
                    "completed_worktree_path": repo_dir,
                    "completed_branch": "main",
                    "completed_commit": completed_commit,
                })
                .to_string(),
            ],
        )
        .unwrap();
        drop(conn);

        let context = build_review_context(
            &db,
            "card-review-quality",
            "agent-1",
            &json!({}),
            ReviewTargetTrust::Untrusted,
            TargetRepoSource::CardScopeDefault,
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&context).unwrap();
        let checklist = parsed["review_quality_checklist"]
            .as_array()
            .expect("checklist array must exist");

        assert_eq!(
            parsed["review_quality_scope_reminder"],
            REVIEW_QUALITY_SCOPE_REMINDER
        );
        assert_eq!(
            parsed["review_verdict_guidance"],
            REVIEW_VERDICT_IMPROVE_GUIDANCE
        );
        assert_eq!(checklist.len(), REVIEW_QUALITY_CHECKLIST.len());
        assert!(checklist.iter().any(|item| {
            item.as_str()
                .unwrap_or_default()
                .contains("race condition / 동시성 이슈")
        }));
        assert!(checklist.iter().any(|item| {
            item.as_str()
                .unwrap_or_default()
                .contains("에러 핸들링 누락")
        }));
    }

    // ── #821 invariants: cancel / terminal / done / reactivate ───────────
    //
    // These tests lock the runtime invariants protected by #815 (user cancel
    // intent preservation) and related prior work. They are intentionally
    // narrow regression guards that can be audited quickly rather than full
    // integration tests. See `docs/FEATURES.md` for the broader state flow.

    /// #821 (1): A user stop (reason `turn_bridge_cancelled`) must move the
    /// linked auto-queue entry to `user_cancelled`, NOT reset it to `pending`.
    /// The next auto-queue tick query (active run + pending entry) must not
    /// find this entry, so no re-dispatch can fire.
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

        // Model the auto-queue tick's pick query: `active` run + `pending`
        // entry. A re-dispatch would require the entry to surface here.
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

    /// #821 (2): A user stop must leave the card's kanban status as-is
    /// (`in_progress`). The cancel path must NOT force-transition the card
    /// into `done` — that would bypass review and strand the user's explicit
    /// stop as a silent auto-completion.
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

    /// #821 (3): When a PMD/admin force-transitions a card back to a
    /// non-terminal state (or terminal, via a different path), live dispatches
    /// for that card must be cancelled WITHOUT resetting the linked auto-queue
    /// entry to `pending`. `cancel_active_dispatches_for_card_on_conn` is the
    /// helper that enforces this: it cancels dispatches in bulk but never
    /// touches `auto_queue_entries`. A re-queue on terminal transition would
    /// cause the run to redispatch work the operator just abandoned.
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

        // Live dispatch moves to cancelled.
        let dispatch_status: String = conn
            .query_row(
                "SELECT status FROM task_dispatches WHERE id = 'dispatch-821-tc'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(dispatch_status, "cancelled");

        // The auto-queue entry must NOT have been reset to `pending` by the
        // terminal-cancel helper. It remains in its prior state (`dispatched`)
        // with its dispatch_id pointer intact. Re-queueing here would let the
        // next auto-queue tick re-pick the abandoned work.
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

        // Modeled tick query: the run is still active but no pending entry
        // surfaces, so nothing re-dispatches.
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
}
