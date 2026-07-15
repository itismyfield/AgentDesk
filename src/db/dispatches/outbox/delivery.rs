use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row as SqlxRow};

// reason: the `Db` payload and `StaleLeaseLost` fields are retained for
// `Debug` diagnostics and future structured logging; callers currently branch
// on the variant (`is_ok`/`matches!`) without reading the fields, so the lib
// build sees them as dead. See #3312.
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) enum DispatchOutboxLeaseUpdateError {
    Db(sqlx::Error),
    StaleLeaseLost {
        outbox_id: i64,
        claim_owner: String,
        claimed_at: DateTime<Utc>,
    },
}

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
                updated_at = NOW(),
                last_stuck_alert_at = NULL
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
    claim_owner: &str,
    claimed_at: DateTime<Utc>,
) -> Result<(), DispatchOutboxLeaseUpdateError> {
    let result = sqlx::query(
        "UPDATE dispatch_outbox
            SET status = 'done',
                processed_at = NOW(),
                error = NULL,
                delivery_status = $2,
                delivery_result = $3::jsonb,
                claimed_at = NULL,
                claim_owner = NULL
          WHERE id = $1
            AND claim_owner = $4
            AND claimed_at = $5",
    )
    .bind(outbox_id)
    .bind(delivery_status)
    .bind(delivery_result_json)
    .bind(claim_owner)
    .bind(claimed_at)
    .execute(pool)
    .await
    .map_err(|error| {
        tracing::warn!(
            outbox_id,
            claim_owner,
            %claimed_at,
            %error,
            "[dispatch-outbox] db failure while marking outbox row done"
        );
        DispatchOutboxLeaseUpdateError::Db(error)
    })?;
    if result.rows_affected() == 0 {
        tracing::warn!(
            outbox_id,
            claim_owner,
            %claimed_at,
            "[dispatch-outbox] stale lease no-op while marking outbox row done"
        );
        return Err(DispatchOutboxLeaseUpdateError::StaleLeaseLost {
            outbox_id,
            claim_owner: claim_owner.to_string(),
            claimed_at,
        });
    }
    Ok(())
}

/// Mark an outbox row as permanently `failed` after the retry budget is
/// exhausted.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn mark_outbox_failed_pg(
    pool: &PgPool,
    outbox_id: i64,
    error_message: &str,
    new_count: i64,
    delivery_status: &str,
    delivery_result_json: &str,
    claim_owner: &str,
    claimed_at: DateTime<Utc>,
) -> Result<(), DispatchOutboxLeaseUpdateError> {
    let result = sqlx::query(
        "UPDATE dispatch_outbox
            SET status = 'failed',
                error = $1,
                retry_count = $2,
                processed_at = NOW(),
                delivery_status = $4,
                delivery_result = $5::jsonb,
                claimed_at = NULL,
                claim_owner = NULL
          WHERE id = $3
            AND claim_owner = $6
            AND claimed_at = $7",
    )
    .bind(error_message)
    .bind(new_count)
    .bind(outbox_id)
    .bind(delivery_status)
    .bind(delivery_result_json)
    .bind(claim_owner)
    .bind(claimed_at)
    .execute(pool)
    .await
    .map_err(|error| {
        tracing::warn!(
            outbox_id,
            claim_owner,
            %claimed_at,
            %error,
            "[dispatch-outbox] db failure while marking outbox row failed"
        );
        DispatchOutboxLeaseUpdateError::Db(error)
    })?;
    if result.rows_affected() == 0 {
        tracing::warn!(
            outbox_id,
            claim_owner,
            %claimed_at,
            "[dispatch-outbox] stale lease no-op while marking outbox row failed"
        );
        return Err(DispatchOutboxLeaseUpdateError::StaleLeaseLost {
            outbox_id,
            claim_owner: claim_owner.to_string(),
            claimed_at,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn old_owner_completion_after_stale_reclaim_is_noop_pg() {
        let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
            "agentdesk_dispatch_outbox_lease",
            "dispatch outbox lease fencing tests",
        )
        .await
        else {
            return;
        };
        let pool = pg_db.connect_and_migrate().await;

        crate::dispatch::test_support::seed_pg_dispatch(
            &pool,
            "dispatch-lease-fence",
            "Lease fence",
        )
        .await;
        let row = sqlx::query(
            "INSERT INTO dispatch_outbox (
                dispatch_id, action, status, retry_count, claimed_at, claim_owner
             ) VALUES (
                'dispatch-lease-fence', 'notify', 'processing', 0,
                NOW() - INTERVAL '10 minutes', 'old-owner'
             )
             RETURNING id, claimed_at",
        )
        .fetch_one(&pool)
        .await
        .expect("seed claimed dispatch outbox row");
        let outbox_id: i64 = row.try_get("id").unwrap();
        let old_claimed_at: DateTime<Utc> = row.try_get("claimed_at").unwrap();

        sqlx::query(
            "UPDATE dispatch_outbox
                SET claim_owner = 'new-owner',
                    claimed_at = NOW()
              WHERE id = $1",
        )
        .bind(outbox_id)
        .execute(&pool)
        .await
        .expect("reclaim dispatch outbox row");

        let result =
            mark_outbox_done_pg(&pool, outbox_id, "sent", "{}", "old-owner", old_claimed_at).await;
        assert!(matches!(
            result,
            Err(DispatchOutboxLeaseUpdateError::StaleLeaseLost { .. })
        ));

        let state = sqlx::query(
            "SELECT status, claim_owner, delivery_status
               FROM dispatch_outbox
              WHERE id = $1",
        )
        .bind(outbox_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(state.try_get::<String, _>("status").unwrap(), "processing");
        assert_eq!(
            state.try_get::<String, _>("claim_owner").unwrap(),
            "new-owner"
        );
        assert!(
            state
                .try_get::<Option<String>, _>("delivery_status")
                .unwrap()
                .is_none()
        );

        pool.close().await;
        pg_db.drop().await;
    }
}
