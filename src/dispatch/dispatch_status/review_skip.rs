//! Auto-queue `review_mode=disabled`: a finished implementation returns the card to the
//! mainline, and any live review dispatch is cancelled rather than deleted.

use anyhow::Result;
use serde_json::json;
use sqlx::{PgPool, Row};

pub(super) async fn restore_auto_queue_mainline_after_review_skip_on_pg(
    pool: &PgPool,
    card_id: &str,
    dispatch_id: &str,
) -> Result<()> {
    let row = sqlx::query(
        "SELECT repo_id, assigned_agent_id
         FROM kanban_cards
         WHERE id = $1",
    )
    .bind(card_id)
    .fetch_one(pool)
    .await
    .map_err(|error| anyhow::anyhow!("load card scope for {card_id}: {error}"))?;
    let repo_id: Option<String> = row
        .try_get("repo_id")
        .map_err(|error| anyhow::anyhow!("decode repo_id for {card_id}: {error}"))?;
    let agent_id: Option<String> = row
        .try_get("assigned_agent_id")
        .map_err(|error| anyhow::anyhow!("decode assigned_agent_id for {card_id}: {error}"))?;
    let effective =
        crate::pipeline::resolve_for_card_pg(pool, repo_id.as_deref(), agent_id.as_deref()).await;
    let target_status = effective
        .kickoff_for(effective.initial_state())
        .filter(|status| status != "review")
        .unwrap_or_else(|| "in_progress".to_string());

    // One transaction: a crash can no longer leave the card in review with its review
    // dispatch gone, and the cancelled dispatch keeps its history and event rows.
    let db = |what: &'static str| move |error: sqlx::Error| anyhow::anyhow!("{what} for {card_id}: {error}");
    let mut tx = pool.begin().await.map_err(db("begin review skip"))?;
    let live_reviews: Vec<String> = sqlx::query_scalar(
        "SELECT id FROM task_dispatches
         WHERE kanban_card_id = $1
           AND dispatch_type IN ('review', 'review-decision')
           AND status IN ('pending', 'dispatched')",
    )
    .bind(card_id)
    .fetch_all(&mut *tx)
    .await
    .map_err(db("load skipped review dispatches"))?;
    sqlx::query(
        "UPDATE sessions
         SET status = CASE WHEN status IN ('turn_active', 'working') THEN 'idle' ELSE status END,
             active_dispatch_id = NULL
         WHERE active_dispatch_id = ANY($1)",
    )
    .bind(&live_reviews)
    .execute(&mut *tx)
    .await
    .map_err(db("clear skipped review sessions"))?;
    sqlx::query(
        "UPDATE task_dispatches
         SET status = 'cancelled',
             updated_at = NOW(),
             completed_at = COALESCE(completed_at, NOW()),
             result = COALESCE(result, $2)
         WHERE id = ANY($1)
           AND status IN ('pending', 'dispatched')",
    )
    .bind(&live_reviews)
    .bind(json!({ "completion_source": "review_mode_disabled" }).to_string())
    .execute(&mut *tx)
    .await
    .map_err(db("cancel skipped review dispatches"))?;
    sqlx::query("DELETE FROM card_review_state WHERE card_id = $1")
        .bind(card_id)
        .execute(&mut *tx)
        .await
        .map_err(db("clear review state"))?;
    sqlx::query(
        "UPDATE kanban_cards
         SET status = $1,
             latest_dispatch_id = $2,
             review_status = NULL,
             review_round = NULL,
             review_entered_at = NULL,
             awaiting_dod_at = NULL,
             blocked_reason = NULL,
             suggestion_pending_at = NULL,
             deferred_dod_json = NULL,
             updated_at = NOW()
         WHERE id = $3",
    )
    .bind(&target_status)
    .bind(dispatch_id)
    .bind(card_id)
    .execute(&mut *tx)
    .await
    .map_err(db("restore mainline card state"))?;
    tx.commit().await.map_err(db("commit review skip"))?;
    Ok(())
}
