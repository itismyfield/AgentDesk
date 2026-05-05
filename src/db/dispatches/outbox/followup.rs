use sqlx::{PgPool, Row as SqlxRow};

use super::model::CompletedDispatchInfo;

pub(crate) async fn load_completed_dispatch_info_pg(
    pool: &PgPool,
    dispatch_id: &str,
) -> Result<Option<CompletedDispatchInfo>, String> {
    let row = sqlx::query(
        "SELECT td.dispatch_type,
                td.status,
                kc.id AS card_id,
                td.result,
                td.context,
                td.thread_id,
                CAST(
                    EXTRACT(
                        EPOCH FROM (
                            COALESCE(td.completed_at, td.updated_at, td.created_at) - td.created_at
                        )
                    ) AS BIGINT
                ) AS duration_seconds
         FROM task_dispatches td
         JOIN kanban_cards kc ON kc.id = td.kanban_card_id
         WHERE td.id = $1",
    )
    .bind(dispatch_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load dispatch {dispatch_id} followup info from postgres: {error}"))?;

    row.map(|row| {
        Ok(CompletedDispatchInfo {
            dispatch_type: row.try_get("dispatch_type").map_err(|error| {
                format!("read postgres dispatch_type for {dispatch_id}: {error}")
            })?,
            status: row
                .try_get("status")
                .map_err(|error| format!("read postgres status for {dispatch_id}: {error}"))?,
            card_id: row
                .try_get("card_id")
                .map_err(|error| format!("read postgres card_id for {dispatch_id}: {error}"))?,
            result_json: row
                .try_get("result")
                .map_err(|error| format!("read postgres result for {dispatch_id}: {error}"))?,
            context_json: row
                .try_get("context")
                .map_err(|error| format!("read postgres context for {dispatch_id}: {error}"))?,
            thread_id: row
                .try_get("thread_id")
                .map_err(|error| format!("read postgres thread_id for {dispatch_id}: {error}"))?,
            duration_seconds: row.try_get("duration_seconds").map_err(|error| {
                format!("read postgres duration_seconds for {dispatch_id}: {error}")
            })?,
        })
    })
    .transpose()
}

pub(crate) async fn load_card_status_pg(
    pool: &PgPool,
    card_id: &str,
) -> Result<Option<String>, String> {
    let row = sqlx::query("SELECT status FROM kanban_cards WHERE id = $1")
        .bind(card_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("load postgres card status for {card_id}: {error}"))?;
    row.map(|row| {
        row.try_get("status")
            .map_err(|error| format!("read postgres card status for {card_id}: {error}"))
    })
    .transpose()
}

pub(crate) async fn clear_all_dispatch_threads_pg(
    pool: &PgPool,
    card_id: &str,
) -> Result<(), String> {
    sqlx::query(
        "UPDATE kanban_cards
         SET channel_thread_map = NULL,
             active_thread_id = NULL
         WHERE id = $1",
    )
    .bind(card_id)
    .execute(pool)
    .await
    .map_err(|error| format!("clear postgres thread mappings for {card_id}: {error}"))?;
    Ok(())
}
