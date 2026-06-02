use anyhow::Result;
use serde_json::json;
use sqlx::{PgPool, Row as SqlxRow};

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
use super::dispatch_status::set_dispatch_status_without_queue_sync_on_conn;

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
    let outcome =
        cancel_dispatch_and_reset_auto_queue_on_pg_tx_inner(&mut tx, dispatch_id, reason).await?;

    tx.commit()
        .await
        .map_err(|error| format!("commit postgres dispatch cancel {dispatch_id}: {error}"))?;
    if outcome.changed > 0 {
        // #3039: emit the observability + quality events the canonical writer
        // (`set_dispatch_status_on_pg_with_sync`) guarantees for every other
        // terminal transition. The raw cancel writer used to reproduce the
        // `dispatch_events` row and `status_reaction` outbox but silently
        // dropped these two emits, so operator/queue cancels never reached the
        // observability + quality pipelines. We emit only after a successful
        // commit so a rolled-back cancel does not surface a phantom event; the
        // tx-composing variant intentionally leaves emission to the caller that
        // owns the surrounding commit boundary.
        if let Some(meta) = outcome.transition.as_ref() {
            meta.emit();
        }
        crate::services::dispatches::wait_queue::spawn_cached_constraint_release_wake(
            pool.clone(),
            "constraint_release",
            dispatch_id.to_string(),
            "cancel_dispatch",
        );
    }

    Ok(outcome.changed)
}

/// Observability metadata captured while cancelling a dispatch so the
/// commit-owning caller can fire the canonical writer's emits after the
/// transaction durably lands (#3039).
struct CancelTransitionMeta {
    dispatch_id: String,
    kanban_card_id: Option<String>,
    to_agent_id: Option<String>,
    dispatch_type: Option<String>,
    from_status: String,
    payload: Option<serde_json::Value>,
}

impl CancelTransitionMeta {
    fn emit(&self) {
        crate::services::observability::emit_dispatch_result(
            &self.dispatch_id,
            self.kanban_card_id.as_deref(),
            self.dispatch_type.as_deref(),
            Some(&self.from_status),
            "cancelled",
            "cancel_dispatch",
            self.payload.as_ref(),
        );
        super::dispatch_status::emit_dispatch_quality_event(
            &self.dispatch_id,
            self.to_agent_id.as_deref(),
            self.kanban_card_id.as_deref(),
            self.dispatch_type.as_deref(),
            Some(&self.from_status),
            "cancelled",
            "cancel_dispatch",
            self.payload.as_ref(),
        );
    }
}

/// Result of the cancel/reset helper: how many dispatch rows changed plus the
/// observability metadata for the transition (present only when a row actually
/// moved to `cancelled`).
struct CancelOutcome {
    changed: usize,
    transition: Option<CancelTransitionMeta>,
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
    // tx-composing callers own the surrounding commit boundary, so they also
    // own observability emission for the wider unit of work; we hand back only
    // the row count and discard the captured transition metadata (#3039).
    Ok(
        cancel_dispatch_and_reset_auto_queue_on_pg_tx_inner(tx, dispatch_id, reason)
            .await?
            .changed,
    )
}

async fn cancel_dispatch_and_reset_auto_queue_on_pg_tx_inner(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    dispatch_id: &str,
    reason: Option<&str>,
) -> Result<CancelOutcome, String> {
    let cancel_payload = reason.map(|value| json!({ "reason": value }));

    let current = sqlx::query(
        "SELECT status, kanban_card_id, to_agent_id, dispatch_type, thread_id
         FROM task_dispatches
         WHERE id = $1",
    )
    .bind(dispatch_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|error| format!("load postgres dispatch {dispatch_id}: {error}"))?;
    let Some(current) = current else {
        return Ok(CancelOutcome {
            changed: 0,
            transition: None,
        });
    };

    let current_status = current
        .try_get::<Option<String>, _>("status")
        .ok()
        .flatten()
        .unwrap_or_default();
    if !matches!(current_status.as_str(), "pending" | "dispatched") {
        return Ok(CancelOutcome {
            changed: 0,
            transition: None,
        });
    }

    let changed = match cancel_payload.as_ref() {
        Some(payload) => sqlx::query(
            "UPDATE task_dispatches
             SET status = 'cancelled',
                 result = $1,
                 updated_at = NOW(),
                 last_stuck_alert_at = NULL
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
                 updated_at = NOW(),
                 last_stuck_alert_at = NULL
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
        return Ok(CancelOutcome {
            changed: 0,
            transition: None,
        });
    }

    let kanban_card_id = current
        .try_get::<Option<String>, _>("kanban_card_id")
        .ok()
        .flatten();
    let to_agent_id = current
        .try_get::<Option<String>, _>("to_agent_id")
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

    crate::db::dispatch_semaphores::release_dispatch_semaphores_on_pg_tx(tx, dispatch_id)
        .await
        .map_err(|error| format!("release postgres dispatch semaphores {dispatch_id}: {error}"))?;

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
    .bind(&kanban_card_id)
    .bind(&dispatch_type)
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

    Ok(CancelOutcome {
        changed,
        transition: Some(CancelTransitionMeta {
            dispatch_id: dispatch_id.to_string(),
            kanban_card_id,
            to_agent_id,
            dispatch_type,
            from_status: current_status,
            payload: cancel_payload,
        }),
    })
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

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
#[path = "dispatch_cancel_tests.rs"]
mod tests;

#[cfg(test)]
mod pg_observability_tests {
    use super::*;

    async fn create_test_pg_db() -> crate::dispatch::test_support::DispatchPostgresTestDb {
        crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "agentdesk_dispatch_cancel_obs",
            "dispatch cancel observability tests",
        )
        .await
    }

    /// #3039 regression guard: the raw cancel writer used to reproduce the
    /// `dispatch_events` row but never fired `emit_dispatch_result`, so
    /// API/queue-driven cancels were invisible to the observability pipeline.
    /// Cancelling a live dispatch must now record a `dispatch_result` event
    /// with `to_status = cancelled`, exactly as the canonical writer does for
    /// every other terminal transition.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_emits_dispatch_result_observability_event() {
        let pg_db = create_test_pg_db().await;
        let pool = pg_db.connect_and_migrate().await;

        let dispatch_id = "dispatch-cancel-obs-3039";
        crate::dispatch::test_support::seed_pg_dispatch(&pool, dispatch_id, "Cancel obs test").await;

        let changed = cancel_dispatch_and_reset_auto_queue_on_pg(
            &pool,
            dispatch_id,
            Some("user_cancelled_by_operator"),
        )
        .await
        .expect("cancel dispatch on pg");
        assert_eq!(changed, 1, "cancel should mark exactly one dispatch");

        let event = crate::services::observability::events::recent(64)
            .into_iter()
            .find(|event| {
                event.event_type == "dispatch_result"
                    && event.payload["dispatch_id"] == dispatch_id
            })
            .expect("cancel must record a dispatch_result observability event (#3039)");
        assert_eq!(event.payload["to_status"], "cancelled");
        assert_eq!(event.payload["from_status"], "pending");
        assert_eq!(event.payload["transition_source"], "cancel_dispatch");

        pool.close().await;
        pg_db.drop().await;
    }
}
