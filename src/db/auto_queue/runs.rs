use sqlx::{PgPool, Row as SqlxRow};

use super::entries::{ENTRY_STATUS_DONE, ENTRY_STATUS_USER_CANCELLED};
use super::slots::release_run_slots_on_pg_tx;

pub(crate) async fn queue_run_completion_notify_on_pg(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
) -> Result<(), String> {
    let row = sqlx::query("SELECT repo, agent_id FROM auto_queue_runs WHERE id = $1")
        .bind(run_id)
        .fetch_one(&mut **tx)
        .await
        .map_err(|error| format!("load completion notify targets for run {run_id}: {error}"))?;
    let repo: Option<String> = row
        .try_get("repo")
        .map_err(|error| format!("decode completion notify repo for run {run_id}: {error}"))?;
    let agent_id: Option<String> = row
        .try_get("agent_id")
        .map_err(|error| format!("decode completion notify agent_id for run {run_id}: {error}"))?;
    let targets = completion_notify_targets_on_pg(tx, run_id, agent_id.as_deref()).await?;
    if targets.is_empty() {
        return Ok(());
    }

    let entry_count =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM auto_queue_entries WHERE run_id = $1")
            .bind(run_id)
            .fetch_one(&mut **tx)
            .await
            .map_err(|error| format!("count auto-queue entries for run {run_id}: {error}"))?;
    let repo_label = repo
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("(global)");
    let short_run_id = &run_id[..8.min(run_id.len())];
    let content = format!("자동큐 완료: {repo_label} / run {short_run_id} / {entry_count}개");

    for channel_id in targets {
        let target = format!("channel:{channel_id}");
        crate::services::message_outbox::enqueue_outbox_pg_on_tx(
            tx,
            crate::services::message_outbox::OutboxMessage {
                target: &target,
                content: &content,
                bot: crate::services::discord::bot_role::UtilityBotRole::Notify.alias(),
                source: "system",
                reason_code: None,
                session_key: None,
                attachment: None,
            },
        )
        .await
        .map_err(|error| {
            format!(
                "queue auto-queue completion notify for run {run_id} channel {channel_id}: {error}"
            )
        })?;
    }

    Ok(())
}

async fn completion_notify_targets_on_pg(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
    run_agent_id: Option<&str>,
) -> Result<Vec<String>, String> {
    let mut targets = Vec::new();

    if let Some(agent_id) = run_agent_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let channel_id = sqlx::query("SELECT discord_channel_id FROM agents WHERE id = $1")
            .bind(agent_id)
            .fetch_optional(&mut **tx)
            .await
            .map_err(|error| {
                format!("load completion notify agent channel for run {run_id}: {error}")
            })?
            .map(|row| {
                row.try_get::<Option<String>, _>("discord_channel_id")
                    .map_err(|error| {
                        format!("decode completion notify agent channel for run {run_id}: {error}")
                    })
            })
            .transpose()?
            .flatten();
        if let Some(channel_id) = channel_id
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
        {
            targets.push(channel_id);
        }
    }

    if targets.is_empty() {
        let rows = sqlx::query(
            "SELECT DISTINCT a.discord_channel_id
             FROM auto_queue_entries e
             JOIN agents a ON a.id = e.agent_id
             WHERE e.run_id = $1
               AND a.discord_channel_id IS NOT NULL
               AND TRIM(a.discord_channel_id) != ''",
        )
        .bind(run_id)
        .fetch_all(&mut **tx)
        .await
        .map_err(|error| {
            format!("load completion notify fallback channels for run {run_id}: {error}")
        })?;
        for row in rows {
            let channel_id: String = row.try_get("discord_channel_id").map_err(|error| {
                format!("decode completion notify fallback channel for run {run_id}: {error}")
            })?;
            targets.push(channel_id);
        }
    }

    targets.sort();
    targets.dedup();
    Ok(targets)
}

pub(super) async fn maybe_finalize_run_after_terminal_entry_pg(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
    new_status: &str,
) -> Result<bool, String> {
    if new_status == ENTRY_STATUS_DONE {
        return Ok(false);
    }
    // #815 P1: never finalize on `user_cancelled` — it must leave the run in a
    // resumable state so the operator can flip the entry back to `pending`.
    if new_status == ENTRY_STATUS_USER_CANCELLED {
        return Ok(false);
    }

    maybe_finalize_run_if_ready_pg(tx, run_id).await
}

pub(super) async fn acquire_run_advisory_xact_lock_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
) -> Result<(), String> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('aq_run:' || $1))")
        .bind(run_id)
        .execute(&mut **tx)
        .await
        .map_err(|error| format!("lock auto-queue run {run_id}: {error}"))?;
    Ok(())
}

/// Acquire multiple run tokens in the incumbent force-pause order.
///
/// Loading the ordered ids separately makes lock acquisition order explicit;
/// it does not rely on a planner preserving an `ORDER BY` through a CTE that
/// also invokes `pg_advisory_xact_lock`.
pub(crate) async fn acquire_run_advisory_xact_locks_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_ids: &[String],
) -> Result<Vec<String>, String> {
    let ordered_run_ids = sqlx::query_scalar::<_, String>(
        "SELECT id
         FROM auto_queue_runs
         WHERE id = ANY($1)
         ORDER BY created_at ASC, id ASC",
    )
    .bind(run_ids)
    .fetch_all(&mut **tx)
    .await
    .map_err(|error| format!("order auto-queue runs before advisory locking: {error}"))?;

    for run_id in &ordered_run_ids {
        acquire_run_advisory_xact_lock_on_pg_tx(tx, run_id).await?;
    }

    Ok(ordered_run_ids)
}

async fn try_acquire_run_advisory_xact_lock_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
) -> Result<bool, String> {
    sqlx::query_scalar::<_, bool>("SELECT pg_try_advisory_xact_lock(hashtext('aq_run:' || $1))")
        .bind(run_id)
        .fetch_one(&mut **tx)
        .await
        .map_err(|error| format!("try-lock auto-queue run {run_id}: {error}"))
}

/// Transaction-local opt-out for a failed-sync whose replacement attachment
/// completes later in the same combined transaction, after the intervening
/// dispatch, event, outbox, and card writes. The caller must restore the
/// setting in the same transaction; this helper only changes the
/// transaction-local flag.
pub(crate) async fn set_terminal_entry_finalize_suppressed_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    suppressed: bool,
) -> Result<(), String> {
    sqlx::query("SELECT set_config('agentdesk.suppress_terminal_entry_finalize', $1, true)")
        .bind(if suppressed { "on" } else { "off" })
        .execute(&mut **tx)
        .await
        .map_err(|error| format!("set terminal-entry finalizer suppression: {error}"))?;
    Ok(())
}

async fn remaining_runnable_entry_count_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
) -> Result<i64, String> {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)
         FROM auto_queue_entries
         WHERE run_id = $1
           AND status IN ('pending', 'dispatched')",
    )
    .bind(run_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(|error| format!("count remaining auto-queue entries for run {run_id}: {error}"))
}

/// A blocking advisory acquisition here would invert the lock order used by
/// terminal entry writers: `update_entry_status_on_pg_tx` can already hold an
/// entry row while `terminalize_selected_runs_with_pg` acquires the run token
/// before locking entries. The non-blocking acquisition below adds no wait and
/// defers finalization whenever another transaction owns the token.
///
/// When the try-lock succeeds, the token remains held until the caller's
/// transaction commits. Blocking attach, cancel, and explicit-completion
/// participants can therefore wait behind this opportunistic finalizer. That
/// set includes the activate tail through `complete_run_after_activate_on_pg`:
/// JS policy calls are bounded by the bridge deadline, while the HTTP activate
/// path has no corresponding deadline. Avoiding an entry-row/run-token ABBA
/// therefore comes with possible participant delay rather than no waiting in
/// the protocol.
///
/// Moving a blocking acquisition above every entry, card, and run write in the
/// callers of this helper would broaden per-run serialization across the sync,
/// GitHub, phase-gate reconciliation, and dispatch-terminal paths. Those paths
/// therefore retain their existing lock order and use this opportunistic
/// finalizer instead. Its remaining-entry predicate is derived only after the
/// token is acquired, so a previously computed count cannot cross an attach
/// commit protected by the same token.
///
/// Blocking participants take `aq_run:<run_id>` before row locks: cancel and
/// terminalize, force-pause, phase-gate attachment (including its attachment-
/// free branch), consultation attachment, explicit completion, dispatched-
/// entry choke points, done-entry reactivation, and retry attachment. Retry
/// already takes the d1 retry token first and then the run token before its
/// failed-sync and replacement attachment. Later cards/entries/runs/slots
/// ordering is serialized by that first run token for these participants.
/// `maybe_finalize_run_if_ready_pg` is the exception: callers may already hold
/// entry or run rows, so its try-lock is non-blocking and adds no wait edge.
///
/// `lock_phase_gate_state_on_pg_tx` uses PostgreSQL's two-argument advisory
/// key space. It is separate from the one-argument `aq_run:<run_id>` token and
/// does not serialize phase-gate state writes with cancel.
///
/// Known completed writers outside this token protocol are intentionally
/// scoped: `complete_run_if_empty` cleans a genuinely entry-less run during
/// activate, `submit_order_with_pg` completes a newly-created run when no
/// ready card was accepted, `reset_scoped_with_pg`/`reset_global_with_pg`
/// destructively remove queue entries before completing runs, and
/// `update_run_with_pg` is an explicit admin override. They do not inherit the
/// attach-versus-terminal atomicity guaranteed by the participants above.
pub(crate) async fn maybe_finalize_run_if_ready_pg(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
) -> Result<bool, String> {
    if !try_acquire_run_advisory_xact_lock_on_pg_tx(tx, run_id).await? {
        tracing::info!(run_id = %run_id, "run_finalize_deferred_lock_contended");
        return Ok(false);
    }

    let finalize_suppressed = sqlx::query_scalar::<_, bool>(
        "SELECT COALESCE(
             current_setting('agentdesk.suppress_terminal_entry_finalize', true) = 'on',
             false
         )",
    )
    .fetch_one(&mut **tx)
    .await
    .map_err(|error| format!("read terminal-entry finalizer suppression: {error}"))?;
    if finalize_suppressed {
        return Ok(false);
    }

    if super::phase_gates::run_has_blocking_phase_gate_on_pg_tx(tx, run_id).await? {
        return Ok(false);
    }

    let remaining = remaining_runnable_entry_count_on_pg_tx(tx, run_id).await?;
    if remaining > 0 {
        return Ok(false);
    }

    // The status transition is the release authority. In particular, a run in
    // the restore hand-off window must retain its slot until restore finalizes;
    // releasing first and then discovering the status is ineligible creates a
    // restoring-run / unowned-slot split brain.
    let updated = sqlx::query(
        "UPDATE auto_queue_runs
         SET status = 'completed',
             completed_at = NOW()
         WHERE id = $1
           AND status IN ('active', 'paused', 'generated', 'pending')",
    )
    .bind(run_id)
    .execute(&mut **tx)
    .await
    .map_err(|error| format!("complete auto-queue run {run_id}: {error}"))?
    .rows_affected();
    if updated == 0 {
        return Ok(false);
    }

    release_run_slots_on_pg_tx(tx, run_id)
        .await
        .map_err(|error| format!("release auto-queue slots for run {run_id}: {error}"))?;
    queue_run_completion_notify_on_pg(tx, run_id).await?;
    Ok(true)
}

pub(super) async fn auto_queue_run_review_disabled_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
) -> Result<bool, String> {
    let review_mode = sqlx::query_scalar::<_, Option<String>>(
        "SELECT review_mode FROM auto_queue_runs WHERE id = $1",
    )
    .bind(run_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|error| format!("load auto-queue review mode for run {run_id}: {error}"))?
    .flatten();

    Ok(review_mode.as_deref().unwrap_or("enabled") == "disabled")
}

pub async fn pause_run_on_pg(pool: &PgPool, run_id: &str) -> Result<bool, String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin postgres pause auto-queue run {run_id}: {error}"))?;
    let updated = sqlx::query(
        "UPDATE auto_queue_runs
         SET status = 'paused',
             completed_at = NULL
         WHERE id = $1
           AND status = 'active'",
    )
    .bind(run_id)
    .execute(&mut *tx)
    .await
    .map_err(|error| format!("pause postgres auto-queue run {run_id}: {error}"))?
    .rows_affected();
    if updated > 0 {
        release_run_slots_on_pg_tx(&mut tx, run_id)
            .await
            .map_err(|error| {
                format!("release postgres auto-queue slots for paused run {run_id}: {error}")
            })?;
    }
    tx.commit()
        .await
        .map_err(|error| format!("commit postgres pause auto-queue run {run_id}: {error}"))?;
    Ok(updated > 0)
}

pub async fn resume_run_on_pg(pool: &PgPool, run_id: &str) -> Result<bool, String> {
    // Keep scoped Resume on the same canonical gate predicate as the global
    // Resume route. A pending/failed gate must leave the paused run untouched.
    if super::phase_gates::run_has_blocking_phase_gate_pg(pool, run_id)
        .await
        .map_err(|error| format!("check blocking phase gates for run {run_id}: {error}"))?
    {
        return Ok(false);
    }

    let updated = sqlx::query(
        "UPDATE auto_queue_runs
         SET status = 'active',
             completed_at = NULL
         WHERE id = $1
           AND status = 'paused'",
    )
    .bind(run_id)
    .execute(pool)
    .await
    .map_err(|error| format!("resume postgres auto-queue run {run_id}: {error}"))?
    .rows_affected();
    Ok(updated > 0)
}

/// Explicit completion begins with the same blocking run token used by attach
/// and cancel writers. Unlike the opportunistic finalizer, this function owns
/// its transaction and holds no row locks before acquiring the token. It then
/// derives the remaining-entry predicate under that token before changing phase
/// gates, the run, slots, or completion notifications.
async fn complete_run_on_pg_inner(
    pool: &PgPool,
    run_id: &str,
    queue_completion_notification: bool,
) -> Result<bool, String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin postgres complete auto-queue run {run_id}: {error}"))?;
    acquire_run_advisory_xact_lock_on_pg_tx(&mut tx, run_id).await?;

    // `user_cancelled` is intentionally not runnable: it is an operator-held
    // state whose dispatch link has already been cleared. The same predicate is
    // used by `maybe_finalize_run_if_ready_pg`.
    let remaining = remaining_runnable_entry_count_on_pg_tx(&mut tx, run_id).await?;
    if remaining > 0 {
        tracing::info!(
            run_id = %run_id,
            remaining,
            "complete_run_refused_live_entries"
        );
        tx.rollback().await.map_err(|error| {
            format!("rollback refused postgres complete auto-queue run {run_id}: {error}")
        })?;
        return Ok(false);
    }

    // #2048 F17: even an explicit "manual complete" call must drop any
    // pending/failed phase-gate rows AND release the run's slot bindings.
    // Otherwise a completed run leaves stale phase_gate rows that next
    // restore/audit treats as still-pending, plus zombie slot assignments
    // that block other runs from picking up the slot. We perform the
    // delete + release inside the same transaction so the operation is
    // atomic with the status flip.
    sqlx::query("DELETE FROM auto_queue_phase_gates WHERE run_id = $1")
        .bind(run_id)
        .execute(&mut *tx)
        .await
        .map_err(|error| format!("delete phase gates for completed run {run_id}: {error}"))?;
    let updated = sqlx::query(
        "UPDATE auto_queue_runs
         SET status = 'completed',
             completed_at = NOW()
         WHERE id = $1
           AND status IN ('active', 'paused', 'generated', 'pending')",
    )
    .bind(run_id)
    .execute(&mut *tx)
    .await
    .map_err(|error| format!("complete postgres auto-queue run {run_id}: {error}"))?
    .rows_affected();
    if updated == 0 {
        tx.rollback().await.map_err(|error| {
            format!("rollback stale postgres complete auto-queue run {run_id}: {error}")
        })?;
        return Ok(false);
    }

    release_run_slots_on_pg_tx(&mut tx, run_id)
        .await
        .map_err(|error| format!("release slots for completed run {run_id}: {error}"))?;

    if queue_completion_notification {
        queue_run_completion_notify_on_pg(&mut tx, run_id).await?;
    }
    tx.commit()
        .await
        .map_err(|error| format!("commit postgres complete auto-queue run {run_id}: {error}"))?;
    Ok(true)
}

pub async fn complete_run_on_pg(pool: &PgPool, run_id: &str) -> Result<bool, String> {
    complete_run_on_pg_inner(pool, run_id, true).await
}

/// Adapter for the drained-run tail of activate. It deliberately omits the
/// completion notification, deletes phase-gate rows on successful completion,
/// and releases slots only inside that successful transaction; activate no
/// longer performs an unconditional slot pre-release. The activate caller
/// treats a refused completion as informational and demotes database errors to
/// a warning instead of turning the otherwise successful activate response
/// into HTTP 500.
pub(crate) async fn complete_run_after_activate_on_pg(
    pool: &PgPool,
    run_id: &str,
) -> Result<bool, String> {
    complete_run_on_pg_inner(pool, run_id, false).await
}
