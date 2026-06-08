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

use serde::{Deserialize, Serialize};
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

// ── Review loopback request DTOs ───────────────────────────────
//
// #3037: `ReviewDecisionBody` (POST /api/reviews/decision) and
// `SubmitVerdictBody` / `VerdictItem` (POST /api/reviews/verdict) are the JSON
// request bodies for the review endpoints. They are consumed both by the axum
// route handlers (`crate::server::routes::review_verdict::{submit_review_decision,
// submit_verdict}`) and by service-layer callers driving the same endpoints over
// the internal-HTTP loopback (`turn_bridge::completion_guard`,
// `discord::internal_api`, `cli::direct`). They were relocated here from
// `crate::server::routes::review_verdict::{decision_route, verdict_route}` so the
// dependency direction is server → services. axum `Json<T>` extraction is
// location-independent, so the route handlers now reference these services paths.
// serde attributes / fields / derives are byte-identical to the originals.

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[allow(dead_code)]
pub struct ReviewDecisionBody {
    pub card_id: String,
    pub decision: String, // "accept", "dispute", "dismiss"
    pub comment: Option<String>,
    /// Optional current implementation commit. When accept is submitted after
    /// the agent has already committed fixes during review-decision, this takes
    /// precedence over worktree inference for #246 skip_rework detection.
    pub commit_sha: Option<String>,
    /// #109: dispatch-scoped targeting — when provided, the server validates
    /// that this dispatch_id matches the pending review-decision dispatch for
    /// the card. Prevents replayed/stale decisions from consuming the wrong
    /// dispatch.
    pub dispatch_id: Option<String>,
    /// #2341 / #2200 sub-3: when the agent disputes a review because the
    /// finding lies outside the current card's scope (e.g. a stacked-branch
    /// leftover), set this to true. The server closes the pending
    /// review-decision dispatch with outcome `scope_mismatch_closed` and
    /// routes the card to terminal state instead of requiring an in-issue
    /// re-review target. Only meaningful when `decision == "dispute"`.
    ///
    /// The close path binds to the latest **completed** review dispatch
    /// (which is what is available at decision time in production flow), and
    /// fail-closes on Unknown scope verification (transient PG/git failure)
    /// or a card lifecycle generation mismatch (card re-opened since the
    /// review completed).
    #[serde(default)]
    pub out_of_scope: Option<bool>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct VerdictItem {
    pub category: Option<String>,
    pub summary: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct SubmitVerdictBody {
    pub dispatch_id: String,
    pub overall: String,
    pub items: Option<Vec<VerdictItem>>,
    pub notes: Option<String>,
    pub feedback: Option<String>,
    /// The commit SHA that was actually reviewed. When provided, the
    /// review-passed marker stamps this commit instead of the current HEAD.
    pub commit: Option<String>,
    /// Provider identifier (e.g. "claude", "codex", "gemini", "opencode") of the verdict submitter.
    /// Used for cross-provider validation in counter-model reviews.
    pub provider: Option<String>,
}
