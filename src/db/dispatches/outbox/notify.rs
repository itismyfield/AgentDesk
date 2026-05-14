use sqlx::{PgPool, Row as SqlxRow};

/// Re-arm (or insert, if missing) the `notify` outbox row for `dispatch_id`.
/// Returns `Ok(true)` when an outbox row ends up in a fresh `pending` state,
/// `Ok(false)` when the dispatch is already terminal or no row exists to
/// rearm.
pub(crate) async fn requeue_dispatch_notify_pg(
    pool: &PgPool,
    dispatch_id: &str,
) -> Result<bool, String> {
    let dispatch = sqlx::query(
        "SELECT status, to_agent_id, kanban_card_id, title
           FROM task_dispatches
          WHERE id = $1",
    )
    .bind(dispatch_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres dispatch {dispatch_id} for notify requeue: {error}"))?;

    let Some(dispatch) = dispatch else {
        return Ok(false);
    };

    let status = dispatch
        .try_get::<String, _>("status")
        .map_err(|error| format!("read postgres dispatch status for {dispatch_id}: {error}"))?;
    if matches!(status.as_str(), "completed" | "failed" | "cancelled") {
        return Ok(false);
    }

    let agent_id = dispatch
        .try_get::<Option<String>, _>("to_agent_id")
        .map_err(|error| format!("read postgres dispatch agent for {dispatch_id}: {error}"))?
        .ok_or_else(|| format!("postgres dispatch {dispatch_id} missing to_agent_id"))?;
    let card_id = dispatch
        .try_get::<Option<String>, _>("kanban_card_id")
        .map_err(|error| format!("read postgres dispatch card for {dispatch_id}: {error}"))?
        .ok_or_else(|| format!("postgres dispatch {dispatch_id} missing kanban_card_id"))?;
    let title = dispatch
        .try_get::<Option<String>, _>("title")
        .map_err(|error| format!("read postgres dispatch title for {dispatch_id}: {error}"))?
        .ok_or_else(|| format!("postgres dispatch {dispatch_id} missing title"))?;

    let rearmed = sqlx::query(
        "INSERT INTO dispatch_outbox (
            dispatch_id, action, agent_id, card_id, title, status, retry_count
         ) VALUES ($1, 'notify', $2, $3, $4, 'pending', 0)
         ON CONFLICT (dispatch_id, action) WHERE action IN ('notify', 'followup')
         DO UPDATE SET
            agent_id = EXCLUDED.agent_id,
            card_id = EXCLUDED.card_id,
            title = EXCLUDED.title,
            status = 'pending',
            retry_count = 0,
            next_attempt_at = NULL,
            processed_at = NULL,
            error = NULL,
            delivery_status = NULL,
            delivery_result = NULL,
            claimed_at = NULL,
            claim_owner = NULL",
    )
    .bind(dispatch_id)
    .bind(&agent_id)
    .bind(&card_id)
    .bind(&title)
    .execute(pool)
    .await
    .map_err(|error| format!("rearm postgres notify outbox for {dispatch_id}: {error}"))?
    .rows_affected();
    Ok(rearmed > 0)
}
