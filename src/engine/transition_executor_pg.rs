use sqlx::{Postgres, Row};

pub(crate) async fn execute_pg_transition_intent(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    intent: &crate::engine::transition::TransitionIntent,
) -> Result<(), String> {
    match intent {
        crate::engine::transition::TransitionIntent::UpdateStatus { card_id, to, .. } => {
            sqlx::query(
                "UPDATE kanban_cards
                 SET status = $1, updated_at = NOW()
                 WHERE id = $2",
            )
            .bind(to)
            .bind(card_id)
            .execute(&mut **tx)
            .await
            .map_err(|error| format!("update status for {card_id}: {error}"))?;
        }
        crate::engine::transition::TransitionIntent::SetLatestDispatchId {
            card_id,
            dispatch_id,
        } => {
            sqlx::query(
                "UPDATE kanban_cards
                 SET latest_dispatch_id = $1, updated_at = NOW()
                 WHERE id = $2",
            )
            .bind(dispatch_id.as_deref())
            .bind(card_id)
            .execute(&mut **tx)
            .await
            .map_err(|error| format!("set latest_dispatch_id for {card_id}: {error}"))?;
        }
        crate::engine::transition::TransitionIntent::SetReviewStatus {
            card_id,
            review_status,
        } => {
            sqlx::query(
                "UPDATE kanban_cards
                 SET review_status = $1, updated_at = NOW()
                 WHERE id = $2",
            )
            .bind(review_status.as_deref())
            .bind(card_id)
            .execute(&mut **tx)
            .await
            .map_err(|error| format!("set review_status for {card_id}: {error}"))?;
        }
        crate::engine::transition::TransitionIntent::ApplyClock { card_id, clock, .. } => {
            if let Some(clock) = clock {
                let sql = if clock.mode.as_deref() == Some("coalesce") {
                    format!(
                        "UPDATE kanban_cards
                         SET {field} = COALESCE({field}, NOW()), updated_at = NOW()
                         WHERE id = $1",
                        field = clock.set
                    )
                } else {
                    format!(
                        "UPDATE kanban_cards
                         SET {field} = NOW(), updated_at = NOW()
                         WHERE id = $1",
                        field = clock.set
                    )
                };
                sqlx::query(&sql)
                    .bind(card_id)
                    .execute(&mut **tx)
                    .await
                    .map_err(|error| format!("apply clock {} for {card_id}: {error}", clock.set))?;
            }
        }
        crate::engine::transition::TransitionIntent::ClearTerminalFields { card_id } => {
            sqlx::query(
                "UPDATE kanban_cards
                 SET review_status = NULL,
                     suggestion_pending_at = NULL,
                     review_entered_at = NULL,
                     awaiting_dod_at = NULL,
                     blocked_reason = NULL,
                     review_round = NULL,
                     deferred_dod_json = NULL,
                     updated_at = NOW()
                 WHERE id = $1",
            )
            .bind(card_id)
            .execute(&mut **tx)
            .await
            .map_err(|error| format!("clear terminal fields for {card_id}: {error}"))?;
        }
        crate::engine::transition::TransitionIntent::SyncAutoQueue { card_id } => {
            crate::github::sync::sync_auto_queue_terminal_on_pg(tx, card_id).await?;
        }
        crate::engine::transition::TransitionIntent::SyncReviewState { card_id, state } => {
            crate::github::sync::sync_review_state_on_pg(tx, card_id, state).await?;
        }
        crate::engine::transition::TransitionIntent::AuditLog {
            card_id,
            from,
            to,
            source,
            message,
        } => {
            sqlx::query(
                "INSERT INTO kanban_audit_logs (
                    card_id, from_status, to_status, source, result
                 )
                 VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(card_id)
            .bind(from)
            .bind(to)
            .bind(source)
            .bind(message)
            .execute(&mut **tx)
            .await
            .map_err(|error| format!("insert audit log for {card_id}: {error}"))?;
        }
        crate::engine::transition::TransitionIntent::CancelDispatch { dispatch_id } => {
            // #2045 Finding 5 (P1): delegate cancellation to the canonical
            // helper so the cleanup pipeline (semaphore release, session
            // active_dispatch_id clear, auto_queue_entries reset,
            // dispatch_events insert, status_reaction outbox enqueue, thread
            // link teardown) runs inside this transaction. The previous raw
            // UPDATE only flipped the row status and left the rest of the
            // graph stale, which is the same class of leak fixed for the JS
            // bridge in Finding 3.
            crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_pg_tx(
                tx,
                dispatch_id,
                Some(crate::dispatch::SYSTEM_CANCEL_REASON_TRANSITION_INTENT),
            )
            .await
            .map_err(|error| format!("cancel dispatch {dispatch_id}: {error}"))?;
        }
        crate::engine::transition::TransitionIntent::ScheduleStageRetry {
            card_id,
            state,
            attempt,
            delay_seconds,
        } => {
            // #3916: record the reducer's backoff-aware retry decision in the
            // audit trail (observable). The card stays in `state` (no status
            // change). NOTE: actually re-issuing dispatch after `delay_seconds`,
            // persisting the due-time, re-arming the timeout clock, and idempotent
            // suppression of stale retries is the deferred live-wiring follow-up.
            sqlx::query(
                "INSERT INTO kanban_audit_logs (
                    card_id, from_status, to_status, source, result
                 )
                 VALUES ($1, $2, $2, 'timeout', $3)",
            )
            .bind(card_id)
            .bind(state)
            .bind(format!(
                "retry-with-backoff: attempt {attempt} scheduled after {delay_seconds}s"
            ))
            .execute(&mut **tx)
            .await
            .map_err(|error| format!("record stage retry for {card_id}: {error}"))?;
        }
    }

    Ok(())
}

pub(crate) async fn execute_activate_transition_intent_pg(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    intent: &crate::engine::transition::TransitionIntent,
) -> Result<(), String> {
    execute_pg_transition_intent(tx, intent).await
}

fn review_result_has_verdict(result: Option<&str>) -> bool {
    let Some(raw) = result.map(str::trim).filter(|value| !value.is_empty()) else {
        return false;
    };
    serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|value| {
            value
                .get("verdict")
                .or_else(|| value.get("decision"))
                .and_then(|field| field.as_str())
                .map(str::trim)
                .filter(|field| !field.is_empty())
                .map(str::to_string)
        })
        .is_some()
}

pub(crate) async fn cancel_live_dispatches_for_terminal_card_pg(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    card_id: &str,
) -> Result<usize, String> {
    let rows = sqlx::query(
        "SELECT id, dispatch_type, result
         FROM task_dispatches
         WHERE kanban_card_id = $1 AND status IN ('pending', 'dispatched')",
    )
    .bind(card_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(|error| format!("load live dispatches for {card_id}: {error}"))?;

    let mut cancelled = 0usize;
    let mut preserved_review_dispatches = Vec::new();

    for row in rows {
        let dispatch_id: String = row
            .try_get("id")
            .map_err(|error| format!("read live dispatch id for {card_id}: {error}"))?;
        let dispatch_type: Option<String> = row
            .try_get("dispatch_type")
            .map_err(|error| format!("read live dispatch type for {dispatch_id}: {error}"))?;
        let result: Option<String> = row
            .try_get("result")
            .map_err(|error| format!("read live dispatch result for {dispatch_id}: {error}"))?;
        if dispatch_type.as_deref() == Some("review")
            && !review_result_has_verdict(result.as_deref())
        {
            preserved_review_dispatches.push(dispatch_id);
            continue;
        }
        // #2045 Finding 5 (P1): route cancellation through the canonical
        // helper so semaphore release, auto_queue_entries reset,
        // dispatch_events audit, status_reaction outbox enqueue, and thread
        // link teardown all happen inside the same terminal-card transaction.
        // The old raw UPDATE only flipped task_dispatches.status and cleared
        // sessions, leaving the rest of the dispatch graph stale (Finding
        // 3 / 4 class).
        let changed = crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_pg_tx(
            tx,
            &dispatch_id,
            Some(crate::dispatch::SYSTEM_CANCEL_REASON_TERMINAL_CARD),
        )
        .await
        .map_err(|error| format!("cancel live dispatch {dispatch_id}: {error}"))?;
        if changed > 0 {
            cancelled += 1;
        }
    }

    if let Some(first_review_dispatch_id) = preserved_review_dispatches.first() {
        let reason = format!(
            "terminal cleanup preserved review dispatch without verdict: {}",
            preserved_review_dispatches.join(",")
        );
        sqlx::query(
            "UPDATE kanban_cards
             SET review_status = 'review_recovery_needed',
                 blocked_reason = $2,
                 latest_dispatch_id = $3,
                 updated_at = NOW()
             WHERE id = $1",
        )
        .bind(card_id)
        .bind(&reason)
        .bind(first_review_dispatch_id)
        .execute(&mut **tx)
        .await
        .map_err(|error| format!("mark review recovery needed for {card_id}: {error}"))?;
    }

    sqlx::query(
        "UPDATE kanban_cards
         SET latest_dispatch_id = NULL,
             updated_at = NOW()
         WHERE id = $1
           AND latest_dispatch_id IS NOT NULL
           AND latest_dispatch_id <> ALL($2::text[])",
    )
    .bind(card_id)
    .bind(&preserved_review_dispatches)
    .execute(&mut **tx)
    .await
    .map_err(|error| format!("clear latest_dispatch_id for {card_id}: {error}"))?;

    Ok(cancelled)
}
