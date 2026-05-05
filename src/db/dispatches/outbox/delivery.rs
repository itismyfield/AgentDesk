use sqlx::{PgPool, Row as SqlxRow};

pub(crate) async fn dispatch_notify_delivery_suppressed_pg(
    pool: &PgPool,
    dispatch_id: &str,
) -> Result<bool, sqlx::Error> {
    let status =
        sqlx::query_scalar::<_, Option<String>>("SELECT status FROM task_dispatches WHERE id = $1")
            .bind(dispatch_id)
            .fetch_optional(pool)
            .await?;
    Ok(matches!(
        status.flatten().as_deref(),
        Some("completed") | Some("failed") | Some("cancelled")
    ))
}

pub(crate) async fn mark_dispatch_dispatched_pg(
    pool: &PgPool,
    dispatch_id: &str,
) -> Result<(), String> {
    let current = sqlx::query(
        "SELECT status, kanban_card_id, dispatch_type
           FROM task_dispatches
          WHERE id = $1",
    )
    .bind(dispatch_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres dispatch {dispatch_id} for status update: {error}"))?;

    let Some(current) = current else {
        return Ok(());
    };

    let current_status = current
        .try_get::<String, _>("status")
        .map_err(|error| format!("read postgres dispatch status for {dispatch_id}: {error}"))?;
    if current_status != "pending" {
        return Ok(());
    }

    let kanban_card_id = current
        .try_get::<Option<String>, _>("kanban_card_id")
        .map_err(|error| format!("read postgres dispatch card for {dispatch_id}: {error}"))?;
    let dispatch_type = current
        .try_get::<Option<String>, _>("dispatch_type")
        .map_err(|error| format!("read postgres dispatch type for {dispatch_id}: {error}"))?;

    let changed = sqlx::query(
        "UPDATE task_dispatches
            SET status = 'dispatched',
                updated_at = NOW()
          WHERE id = $1
            AND status = 'pending'",
    )
    .bind(dispatch_id)
    .execute(pool)
    .await
    .map_err(|error| format!("update postgres dispatch {dispatch_id} to dispatched: {error}"))?
    .rows_affected();
    if changed == 0 {
        return Ok(());
    }

    sqlx::query(
        "INSERT INTO dispatch_events (
            dispatch_id,
            kanban_card_id,
            dispatch_type,
            from_status,
            to_status,
            transition_source,
            payload_json
        ) VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(dispatch_id)
    .bind(kanban_card_id)
    .bind(dispatch_type)
    .bind(Some(current_status.as_str()))
    .bind("dispatched")
    .bind("dispatch_outbox_notify")
    .bind(Option::<serde_json::Value>::None)
    .execute(pool)
    .await
    .map_err(|error| format!("insert postgres dispatch event for {dispatch_id}: {error}"))?;

    sqlx::query(
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
    .execute(pool)
    .await
    .map_err(|error| format!("enqueue postgres status_reaction for {dispatch_id}: {error}"))?;

    Ok(())
}

/// Mark an outbox row as `done` and clear claim state. Used both by the
/// notify-suppressed early-exit and by the success branch of the worker.
pub(crate) async fn mark_outbox_done_pg(
    pool: &PgPool,
    outbox_id: i64,
    delivery_status: &str,
    delivery_result_json: &str,
) {
    sqlx::query(
        "UPDATE dispatch_outbox
            SET status = 'done',
                processed_at = NOW(),
                error = NULL,
                delivery_status = $2,
                delivery_result = $3::jsonb,
                claimed_at = NULL,
                claim_owner = NULL
          WHERE id = $1",
    )
    .bind(outbox_id)
    .bind(delivery_status)
    .bind(delivery_result_json)
    .execute(pool)
    .await
    .ok();
}

/// Mark an outbox row as permanently `failed` after the retry budget is
/// exhausted.
pub(crate) async fn mark_outbox_failed_pg(
    pool: &PgPool,
    outbox_id: i64,
    error_message: &str,
    new_count: i64,
    delivery_status: &str,
    delivery_result_json: &str,
) {
    sqlx::query(
        "UPDATE dispatch_outbox
            SET status = 'failed',
                error = $1,
                retry_count = $2,
                processed_at = NOW(),
                delivery_status = $4,
                delivery_result = $5::jsonb,
                claimed_at = NULL,
                claim_owner = NULL
          WHERE id = $3",
    )
    .bind(error_message)
    .bind(new_count)
    .bind(outbox_id)
    .bind(delivery_status)
    .bind(delivery_result_json)
    .execute(pool)
    .await
    .ok();
}
