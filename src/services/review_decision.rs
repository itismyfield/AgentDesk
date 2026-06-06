//! Review-decision domain logic extracted from the
//! `/api/review-decision` HTTP route (#3038 god-function decomposition,
//! A1 / route_srp).
//!
//! These are the self-contained, single-transaction cleanup/state-machine
//! operations that the `submit_review_decision` handler and its phase helpers
//! invoke after a decision is resolved. They were lifted verbatim out of
//! `src/server/routes/review_verdict/decision_route.rs`; the only change is
//! that the Postgres pool is now threaded **explicitly** as a parameter rather
//! than reached through HTTP request state (`AppState::pg_pool_ref`). The thin
//! `*_pg_first` wrappers that remain in the route module perform the pool
//! extraction and preserve the exact "postgres pool unavailable" error
//! emission, so transaction boundaries, statement ordering, and behavior are
//! preserved 1:1.

use sqlx::PgPool;

/// Open a card's review-decision dispute review-entry: flip review status to
/// `reviewing`, sync the canonical review state, and stamp `review_entered_at`.
///
/// Single transaction; statement order preserved exactly from the original
/// `prepare_dispute_review_entry_pg_first` body.
pub async fn prepare_dispute_review_entry(pool: &PgPool, card_id: &str) -> Result<(), String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin dispute review-entry tx for {card_id}: {error}"))?;
    let dispute_intents = [
        crate::engine::transition::TransitionIntent::SetReviewStatus {
            card_id: card_id.to_string(),
            review_status: Some("reviewing".to_string()),
        },
        crate::engine::transition::TransitionIntent::SyncReviewState {
            card_id: card_id.to_string(),
            state: "reviewing".to_string(),
        },
    ];
    for intent in &dispute_intents {
        crate::engine::transition_executor_pg::execute_pg_transition_intent(&mut tx, intent)
            .await?;
    }
    sqlx::query("UPDATE kanban_cards SET review_entered_at = NOW() WHERE id = $1")
        .bind(card_id)
        .execute(&mut *tx)
        .await
        .map_err(|error| format!("set review_entered_at for {card_id}: {error}"))?;
    tx.commit()
        .await
        .map_err(|error| format!("commit dispute review-entry tx for {card_id}: {error}"))?;
    Ok(())
}

/// Accept cleanup: optionally clear the card's review status and always clear
/// `suggestion_pending_at`.
///
/// Single transaction; statement order preserved exactly from the original
/// `finalize_accept_cleanup_pg_first` body.
pub async fn finalize_accept_cleanup(
    pool: &PgPool,
    card_id: &str,
    clear_review_status: bool,
) -> Result<(), String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin accept cleanup tx for {card_id}: {error}"))?;
    if clear_review_status {
        crate::engine::transition_executor_pg::execute_pg_transition_intent(
            &mut tx,
            &crate::engine::transition::TransitionIntent::SetReviewStatus {
                card_id: card_id.to_string(),
                review_status: None,
            },
        )
        .await?;
    }
    sqlx::query("UPDATE kanban_cards SET suggestion_pending_at = NULL WHERE id = $1")
        .bind(card_id)
        .execute(&mut *tx)
        .await
        .map_err(|error| format!("clear suggestion_pending_at for {card_id}: {error}"))?;
    tx.commit()
        .await
        .map_err(|error| format!("commit accept cleanup tx for {card_id}: {error}"))?;
    Ok(())
}

/// Dismiss cleanup: cancel live review / review-decision dispatches, clear the
/// card's review status, and drop its thread mappings — all atomically.
///
/// Single transaction; statement order preserved exactly from the original
/// `dismiss_review_cleanup_pg_first` body.
pub async fn dismiss_review_cleanup(pool: &PgPool, card_id: &str) -> Result<(), String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin dismiss cleanup tx for {card_id}: {error}"))?;

    let dispatch_ids: Vec<String> = sqlx::query_scalar(
        "SELECT id FROM task_dispatches
         WHERE kanban_card_id = $1
           AND status IN ('pending', 'dispatched')
           AND dispatch_type IN ('review', 'review-decision')",
    )
    .bind(card_id)
    .fetch_all(&mut *tx)
    .await
    .map_err(|error| format!("load dismiss cleanup dispatches for {card_id}: {error}"))?;

    for dispatch_id in &dispatch_ids {
        crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_pg_tx(&mut tx, dispatch_id, None)
            .await?;
    }

    let clear_review_status = crate::engine::transition::TransitionIntent::SetReviewStatus {
        card_id: card_id.to_string(),
        review_status: None,
    };
    crate::engine::transition_executor_pg::execute_pg_transition_intent(
        &mut tx,
        &clear_review_status,
    )
    .await?;

    sqlx::query(
        "UPDATE kanban_cards
         SET channel_thread_map = NULL,
             active_thread_id = NULL
         WHERE id = $1",
    )
    .bind(card_id)
    .execute(&mut *tx)
    .await
    .map_err(|error| format!("clear dismiss thread mappings for {card_id}: {error}"))?;

    tx.commit()
        .await
        .map_err(|error| format!("commit dismiss cleanup tx for {card_id}: {error}"))?;
    Ok(())
}
