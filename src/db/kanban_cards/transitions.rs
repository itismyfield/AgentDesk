use sqlx::{PgPool, Row as SqlxRow};

use super::{
    ActiveTurnTarget, DodStateRecord, PmDecisionCardInfo, RereviewCardInfo, RetryDispatchSpec,
};

pub async fn load_active_turn_targets_for_card_pg(
    pool: &PgPool,
    card_id: &str,
) -> anyhow::Result<Vec<ActiveTurnTarget>> {
    let rows = sqlx::query(
        "SELECT DISTINCT session_key, provider, thread_channel_id
         FROM sessions
         WHERE active_dispatch_id IN (
             SELECT id
             FROM task_dispatches
             WHERE kanban_card_id = $1
               AND status IN ('pending', 'dispatched')
         )",
    )
    .bind(card_id)
    .fetch_all(pool)
    .await
    .map_err(|error| anyhow::anyhow!("load postgres active turn targets for {card_id}: {error}"))?;

    rows.into_iter()
        .map(|row| {
            Ok(ActiveTurnTarget {
                session_key: row.try_get("session_key").map_err(|error| {
                    anyhow::anyhow!("decode session_key for {card_id}: {error}")
                })?,
                provider: row
                    .try_get("provider")
                    .map_err(|error| anyhow::anyhow!("decode provider for {card_id}: {error}"))?,
                thread_channel_id: row.try_get("thread_channel_id").map_err(|error| {
                    anyhow::anyhow!("decode thread_channel_id for {card_id}: {error}")
                })?,
            })
        })
        .collect()
}

pub async fn clear_session_for_turn_target_pg(
    pool: &PgPool,
    session_key: &str,
) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE sessions
         SET status = 'disconnected',
             active_dispatch_id = NULL,
             claude_session_id = NULL
         WHERE session_key = $1",
    )
    .bind(session_key)
    .execute(pool)
    .await
    .map_err(|error| anyhow::anyhow!("clear session for {session_key}: {error}"))?;
    Ok(())
}

pub async fn load_retry_dispatch_spec_pg(
    pool: &PgPool,
    card_id: &str,
) -> Result<Option<RetryDispatchSpec>, String> {
    let Some((card_agent_id, card_title, latest_dispatch_id)) =
        sqlx::query_as::<_, (Option<String>, String, Option<String>)>(
            "SELECT assigned_agent_id, title, latest_dispatch_id
             FROM kanban_cards
             WHERE id = $1",
        )
        .bind(card_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("{error}"))?
    else {
        return Ok(None);
    };

    let latest_dispatch = if let Some(dispatch_id) = latest_dispatch_id.as_deref() {
        sqlx::query_as::<_, (Option<String>, Option<String>, Option<String>)>(
            "SELECT to_agent_id, dispatch_type, title
             FROM task_dispatches
             WHERE id = $1",
        )
        .bind(dispatch_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("{error}"))?
    } else {
        None
    };
    let latest_dispatch = match latest_dispatch {
        Some(row) => Some(row),
        None => sqlx::query_as::<_, (Option<String>, Option<String>, Option<String>)>(
            "SELECT to_agent_id, dispatch_type, title
                 FROM task_dispatches
                 WHERE kanban_card_id = $1
                 ORDER BY created_at DESC, id DESC
                 LIMIT 1",
        )
        .bind(card_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("{error}"))?,
    };

    let (dispatch_agent_id, dispatch_type, dispatch_title) =
        latest_dispatch.unwrap_or((None, None, None));
    Ok(Some(RetryDispatchSpec {
        agent_id: dispatch_agent_id.or(card_agent_id).unwrap_or_default(),
        dispatch_type: dispatch_type
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "implementation".to_string()),
        title: dispatch_title
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(card_title),
    }))
}

pub async fn load_dod_state_pg(
    pool: &PgPool,
    card_id: &str,
) -> Result<Option<DodStateRecord>, String> {
    sqlx::query_as::<_, (Option<String>, String, Option<String>)>(
        "SELECT deferred_dod_json, status, review_status
         FROM kanban_cards
         WHERE id = $1",
    )
    .bind(card_id)
    .fetch_optional(pool)
    .await
    .map(|row| {
        row.map(
            |(deferred_dod_json, status, review_status)| DodStateRecord {
                deferred_dod_json,
                status,
                review_status,
            },
        )
    })
    .map_err(|error| format!("load postgres DoD state: {error}"))
}

pub async fn update_deferred_dod_pg(
    pool: &PgPool,
    card_id: &str,
    dod_json: &str,
) -> Result<(), String> {
    sqlx::query(
        "UPDATE kanban_cards
         SET deferred_dod_json = $1, updated_at = NOW()
         WHERE id = $2",
    )
    .bind(dod_json)
    .bind(card_id)
    .execute(pool)
    .await
    .map_err(|error| format!("update postgres DoD state: {error}"))?;
    Ok(())
}

pub async fn update_review_clock_after_dod_pg(pool: &PgPool, card_id: &str) -> Result<(), String> {
    sqlx::query(
        "UPDATE kanban_cards
         SET review_entered_at = NOW(), awaiting_dod_at = NULL
         WHERE id = $1",
    )
    .bind(card_id)
    .execute(pool)
    .await
    .map_err(|error| format!("update postgres review clock: {error}"))?;
    Ok(())
}

pub async fn load_pm_decision_card_info_pg(
    pool: &PgPool,
    card_id: &str,
) -> Result<Option<PmDecisionCardInfo>, String> {
    sqlx::query_as::<_, (String, Option<String>, Option<String>, String, String)>(
        "SELECT COALESCE(status, ''), review_status, blocked_reason, COALESCE(assigned_agent_id, ''), title
         FROM kanban_cards
         WHERE id = $1",
    )
    .bind(card_id)
    .fetch_optional(pool)
    .await
    .map(|row| {
        row.map(
            |(status, review_status, blocked_reason, agent_id, title)| PmDecisionCardInfo {
                status,
                review_status,
                blocked_reason,
                agent_id,
                title,
            },
        )
    })
    .map_err(|error| format!("load card for pm decision: {error}"))
}

pub async fn pending_pm_decision_dispatch_ids_pg(
    pool: &PgPool,
    card_id: &str,
) -> Result<Vec<String>, String> {
    sqlx::query_scalar::<_, String>(
        "SELECT id FROM task_dispatches
         WHERE kanban_card_id = $1
           AND dispatch_type = 'pm-decision'
           AND status = 'pending'",
    )
    .bind(card_id)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("load pending pm-decision dispatches: {error}"))
}

pub async fn clear_manual_intervention_marker_pg(
    pool: &PgPool,
    card_id: &str,
) -> Result<(), String> {
    sqlx::query("UPDATE kanban_cards SET blocked_reason = NULL, updated_at = NOW() WHERE id = $1")
        .bind(card_id)
        .execute(pool)
        .await
        .map_err(|error| format!("clear manual intervention marker: {error}"))?;
    Ok(())
}

pub async fn has_live_dispatch_session_pg(pool: &PgPool, card_id: &str) -> Result<bool, String> {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT
         FROM task_dispatches td
         JOIN sessions s ON s.active_dispatch_id = td.id
            AND s.status IN ('turn_active', 'awaiting_bg', 'awaiting_user', 'working', 'idle')
         WHERE td.kanban_card_id = $1
           AND td.status IN ('pending', 'dispatched')",
    )
    .bind(card_id)
    .fetch_one(pool)
    .await
    .map(|count| count > 0)
    .map_err(|error| format!("check live dispatch/session: {error}"))
}

pub async fn load_rereview_card_info_pg(
    pool: &PgPool,
    card_id: &str,
) -> Result<Option<RereviewCardInfo>, String> {
    sqlx::query_as::<_, (String, Option<String>, String, Option<String>)>(
        "SELECT status, assigned_agent_id, title, github_issue_url
         FROM kanban_cards WHERE id = $1",
    )
    .bind(card_id)
    .fetch_optional(pool)
    .await
    .map(|row| {
        row.map(
            |(status, assigned_agent_id, title, github_issue_url)| RereviewCardInfo {
                status,
                assigned_agent_id,
                title,
                github_issue_url,
            },
        )
    })
    .map_err(|error| format!("postgres lookup failed: {error}"))
}

pub async fn stale_review_dispatch_ids_pg(
    pool: &PgPool,
    card_id: &str,
) -> Result<Vec<String>, String> {
    sqlx::query_scalar::<_, String>(
        "SELECT id FROM task_dispatches
         WHERE kanban_card_id = $1
           AND dispatch_type IN ('review', 'review-decision')
           AND status IN ('pending', 'dispatched')",
    )
    .bind(card_id)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("postgres stale dispatch lookup failed: {error}"))
}

pub async fn cleanup_rereview_card_pg(pool: &PgPool, card_id: &str) -> Result<(), String> {
    sqlx::query(
        "UPDATE kanban_cards
         SET review_status = NULL,
             suggestion_pending_at = NULL,
             review_entered_at = NULL,
             awaiting_dod_at = NULL,
             updated_at = NOW()
         WHERE id = $1",
    )
    .bind(card_id)
    .execute(pool)
    .await
    .map_err(|error| format!("postgres rereview cleanup failed: {error}"))?;
    Ok(())
}

pub async fn reset_repeated_finding_rounds_pg(pool: &PgPool, card_id: &str) -> Result<(), String> {
    sqlx::query(
        "UPDATE card_review_state
         SET approach_change_round = NULL,
             session_reset_round = NULL,
             updated_at = NOW()
         WHERE card_id = $1",
    )
    .bind(card_id)
    .execute(pool)
    .await
    .map_err(|error| format!("postgres rereview repeated-finding reset failed: {error}"))?;
    Ok(())
}

pub async fn reset_completed_at_pg(pool: &PgPool, card_id: &str) -> Result<(), String> {
    sqlx::query(
        "UPDATE kanban_cards
         SET completed_at = NULL, updated_at = NOW()
         WHERE id = $1",
    )
    .bind(card_id)
    .execute(pool)
    .await
    .map_err(|error| format!("postgres completed_at reset failed: {error}"))?;
    Ok(())
}

pub async fn active_auto_queue_entry_ids_for_rereview_pg(
    pool: &PgPool,
    card_id: &str,
) -> Result<Vec<String>, String> {
    sqlx::query_scalar::<_, String>(
        "SELECT id FROM auto_queue_entries
         WHERE kanban_card_id = $1
           AND status IN ('pending', 'dispatched', 'done')
           AND run_id IN (
               SELECT id FROM auto_queue_runs WHERE status IN ('active', 'paused')
           )",
    )
    .bind(card_id)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("postgres auto-queue entry lookup failed: {error}"))
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub fn find_active_review_dispatch_id_on_conn(
    conn: &sqlite_test::Connection,
    card_id: &str,
) -> Option<String> {
    conn.query_row(
        "SELECT id FROM task_dispatches
         WHERE kanban_card_id = ?1
           AND dispatch_type = 'review'
           AND status IN ('pending', 'dispatched')
         ORDER BY updated_at DESC, rowid DESC
         LIMIT 1",
        [card_id],
        |row| row.get(0),
    )
    .ok()
}

pub async fn find_active_review_dispatch_id_pg(pool: &PgPool, card_id: &str) -> Option<String> {
    sqlx::query_scalar::<_, String>(
        "SELECT id FROM task_dispatches
         WHERE kanban_card_id = $1
           AND dispatch_type = 'review'
           AND status IN ('pending', 'dispatched')
         ORDER BY updated_at DESC, id DESC
         LIMIT 1",
    )
    .bind(card_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub fn count_live_auto_queue_entries_for_card_on_conn(
    conn: &sqlite_test::Connection,
    card_id: &str,
) -> anyhow::Result<usize> {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*)
             FROM auto_queue_entries
             WHERE kanban_card_id = ?1
               AND status IN ('pending', 'dispatched')
               AND run_id IN (
                   SELECT id FROM auto_queue_runs WHERE status IN ('active', 'paused')
               )",
            [card_id],
            |row| row.get(0),
        )
        .map_err(|error| anyhow::anyhow!("count live auto-queue entries for {card_id}: {error}"))?;
    Ok(count.max(0) as usize)
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub fn clear_force_transition_terminalized_links_on_conn(
    conn: &sqlite_test::Connection,
    card_id: &str,
) -> anyhow::Result<()> {
    conn.execute(
        "UPDATE auto_queue_entries
         SET dispatch_id = NULL,
             slot_index = NULL,
             dispatched_at = NULL,
             completed_at = COALESCE(completed_at, datetime('now'))
         WHERE kanban_card_id = ?1
           AND status = 'skipped'
           AND dispatch_id IS NOT NULL
           AND run_id IN (
               SELECT id FROM auto_queue_runs WHERE status IN ('active', 'paused')
           )",
        [card_id],
    )
    .map_err(|error| {
        anyhow::anyhow!(
            "clear force-transition terminalized auto-queue links for {card_id}: {error}"
        )
    })?;
    Ok(())
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub fn skip_live_auto_queue_entries_for_card_legacy(
    conn: &sqlite_test::Connection,
    card_id: &str,
) -> sqlite_test::Result<usize> {
    let mut stmt = conn.prepare(
        "SELECT id FROM auto_queue_entries
         WHERE kanban_card_id = ?1
           AND status IN ('pending', 'dispatched')
           AND run_id IN (SELECT id FROM auto_queue_runs WHERE status IN ('active', 'paused'))",
    )?;
    let entry_ids: Vec<String> = stmt
        .query_map([card_id], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    drop(stmt);

    let mut changed = 0usize;
    for entry_id in entry_ids {
        if conn.execute(
            "UPDATE auto_queue_entries
                 SET status = 'skipped',
                     updated_at = datetime('now'),
                     completed_at = COALESCE(completed_at, datetime('now'))
                 WHERE id = ?1 AND status IN ('pending', 'dispatched')",
            [&entry_id],
        )? > 0
        {
            changed += 1;
        }
    }

    Ok(changed)
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub fn cleanup_force_transition_revert_on_conn(
    conn: &sqlite_test::Connection,
    card_id: &str,
    target_status: &str,
) -> anyhow::Result<(usize, usize)> {
    let reason = format!("force-transition to {target_status}");
    let skipped_auto_queue_entries = count_live_auto_queue_entries_for_card_on_conn(conn, card_id)?;
    let cancelled_dispatches =
        crate::dispatch::cancel_active_dispatches_for_card_on_conn(conn, card_id, Some(&reason))?;
    skip_live_auto_queue_entries_for_card_legacy(conn, card_id)?;
    clear_force_transition_terminalized_links_on_conn(conn, card_id)?;
    crate::kanban::cleanup_force_transition_revert_fields_on_conn(conn, card_id)?;

    Ok((cancelled_dispatches, skipped_auto_queue_entries))
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub fn move_auto_queue_entry_to_dispatched_on_conn(
    conn: &sqlite_test::Connection,
    entry_id: &str,
    trigger_source: &str,
    options: &crate::db::auto_queue::EntryStatusUpdateOptions,
) -> sqlite_test::Result<()> {
    conn.execute(
        "UPDATE auto_queue_entries
         SET status = 'dispatched',
             dispatch_id = COALESCE(?2, dispatch_id),
             slot_index = COALESCE(?3, slot_index),
             dispatched_at = COALESCE(dispatched_at, datetime('now')),
             completed_at = NULL,
             updated_at = datetime('now')
         WHERE id = ?1 AND status IN ('pending', 'dispatched', 'done')",
        sqlite_test::params![entry_id, options.dispatch_id, options.slot_index],
    )?;
    let _ = trigger_source;
    Ok(())
}

pub async fn move_auto_queue_entry_to_dispatched_on_pg(
    pool: &PgPool,
    entry_id: &str,
    trigger_source: &str,
    options: &crate::db::auto_queue::EntryStatusUpdateOptions,
) -> Result<(), String> {
    crate::db::auto_queue::reactivate_done_entry_on_pg(pool, entry_id, trigger_source, options)
        .await
        .map(|_| ())
}

pub async fn reactivate_done_auto_queue_entries_pg(
    pool: &PgPool,
    card_id: &str,
) -> anyhow::Result<()> {
    let entry_ids = sqlx::query_scalar::<_, String>(
        "SELECT id
         FROM auto_queue_entries
         WHERE kanban_card_id = $1
           AND status = 'done'",
    )
    .bind(card_id)
    .fetch_all(pool)
    .await
    .map_err(|error| {
        anyhow::anyhow!("load postgres done auto-queue entries for {card_id}: {error}")
    })?;

    for entry_id in entry_ids {
        move_auto_queue_entry_to_dispatched_on_pg(
            pool,
            &entry_id,
            "api_reopen",
            &crate::db::auto_queue::EntryStatusUpdateOptions::default(),
        )
        .await
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    }

    Ok(())
}

pub async fn active_dispatch_ids_for_card_pg(
    pool: &PgPool,
    card_id: &str,
) -> anyhow::Result<Vec<String>> {
    let rows = sqlx::query_scalar::<_, String>(
        "SELECT id
         FROM task_dispatches
         WHERE kanban_card_id = $1
           AND status IN ('pending', 'dispatched')",
    )
    .bind(card_id)
    .fetch_all(pool)
    .await
    .map_err(|error| anyhow::anyhow!("load active dispatches for {card_id}: {error}"))?;
    Ok(rows)
}

pub async fn cancelled_dispatch_ids_among_pg(
    pool: &PgPool,
    dispatch_ids: &[String],
) -> anyhow::Result<Vec<String>> {
    if dispatch_ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query_scalar::<_, String>(
        "SELECT id
         FROM task_dispatches
         WHERE id = ANY($1)
           AND status = 'cancelled'",
    )
    .bind(dispatch_ids)
    .fetch_all(pool)
    .await
    .map_err(|error| anyhow::anyhow!("filter cancelled dispatches: {error}"))?;
    Ok(rows)
}

pub async fn clear_all_threads_pg(pool: &PgPool, card_id: &str) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE kanban_cards
         SET channel_thread_map = NULL,
             active_thread_id = NULL,
             updated_at = NOW()
         WHERE id = $1",
    )
    .bind(card_id)
    .execute(pool)
    .await
    .map_err(|error| anyhow::anyhow!("clear postgres thread state for {card_id}: {error}"))?;
    Ok(())
}
