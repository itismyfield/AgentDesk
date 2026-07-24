use super::*;

pub(super) fn normalize_human_alert_target(channel: &str) -> Option<String> {
    let channel = channel.trim();
    if channel.is_empty() {
        return None;
    }
    Some(if channel.starts_with("channel:") {
        channel.to_string()
    } else {
        format!("channel:{channel}")
    })
}

pub(super) fn load_human_alert_target(shared: &SharedData) -> Option<String> {
    if let Some(pool) = shared.pg_pool.as_ref() {
        return crate::utils::async_bridge::block_on_pg_result(
            pool,
            |pool| async move {
                sqlx::query_scalar::<_, String>(
                    "SELECT value FROM kv_meta WHERE key = 'kanban_human_alert_channel_id'",
                )
                .fetch_optional(&pool)
                .await
                .map_err(|error| format!("load postgres human alert target: {error}"))
            },
            |message| message,
        )
        .ok()
        .flatten()
        .and_then(|channel| normalize_human_alert_target(&channel));
    }

    let _ = shared;
    None
}

pub(super) fn merge_card_label_metadata(existing_metadata: Option<&str>, label: &str) -> String {
    let mut metadata = existing_metadata
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();

    let mut labels = metadata
        .get("labels")
        .and_then(|value| value.as_str())
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if !labels.iter().any(|existing| existing == label) {
        labels.push(label.to_string());
    }
    metadata.insert(
        "labels".to_string(),
        serde_json::Value::String(labels.join(",")),
    );

    serde_json::Value::Object(metadata).to_string()
}

pub(super) async fn update_card_ready_failure_marker_pg(
    pool: &sqlx::PgPool,
    card_id: &str,
    reason: &str,
) -> Result<bool, String> {
    let existing_metadata = sqlx::query_scalar::<_, Option<String>>(
        "SELECT metadata::text FROM kanban_cards WHERE id = $1",
    )
    .bind(card_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres card metadata for {card_id}: {error}"))?
    .flatten();
    let metadata_json =
        merge_card_label_metadata(existing_metadata.as_deref(), READY_FOR_INPUT_STUCK_LABEL);
    let updated = sqlx::query(
        "UPDATE kanban_cards
         SET metadata = $1::jsonb,
             blocked_reason = $2,
             updated_at = NOW()
         WHERE id = $3",
    )
    .bind(metadata_json)
    .bind(reason)
    .bind(card_id)
    .execute(pool)
    .await
    .map_err(|error| format!("update postgres ready marker for {card_id}: {error}"))?
    .rows_affected();
    Ok(updated > 0)
}

pub(super) fn load_dispatch_card_id(shared: &SharedData, dispatch_id: &str) -> Option<String> {
    if let Some(pool) = shared.pg_pool.as_ref() {
        let dispatch_id = dispatch_id.to_string();
        return crate::utils::async_bridge::block_on_pg_result(
            pool,
            move |pool| async move {
                sqlx::query_scalar::<_, String>(
                    "SELECT kanban_card_id FROM task_dispatches WHERE id = $1",
                )
                .bind(dispatch_id)
                .fetch_optional(&pool)
                .await
                .map_err(|error| format!("load postgres dispatch card id: {error}"))
            },
            |message| message,
        )
        .ok()
        .flatten();
    }

    let _ = (shared, dispatch_id);
    None
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(in crate::services::discord) struct ReadyForInputFailureResult {
    pub dispatch_failed: bool,
    pub card_id: Option<String>,
    pub card_marked: bool,
    pub human_alert_sent: bool,
}

pub(in crate::services::discord) async fn fail_dispatch_for_ready_for_input_stall(
    shared: &Arc<SharedData>,
    dispatch_id: &str,
    tmux_session_name: &str,
) -> Result<ReadyForInputFailureResult, String> {
    let payload = serde_json::json!({
        "reason": READY_FOR_INPUT_STUCK_REASON,
        "failure_kind": READY_FOR_INPUT_STUCK_LABEL,
        "tmux_session_name": tmux_session_name,
    });
    let changed = crate::dispatch::set_dispatch_status_with_backends(
        shared.pg_pool.as_ref(),
        dispatch_id,
        "failed",
        Some(&payload),
        "tmux_ready_for_input_stuck",
        Some(&["pending", "dispatched"]),
        false,
    )
    .map_err(|error| format!("mark dispatch {dispatch_id} failed for ready stall: {error}"))?;

    let card_id = load_dispatch_card_id(shared.as_ref(), dispatch_id);
    let mut card_marked = false;
    if let Some(card_id_ref) = card_id.as_deref() {
        card_marked = if let Some(pool) = shared.pg_pool.as_ref() {
            update_card_ready_failure_marker_pg(pool, card_id_ref, READY_FOR_INPUT_STUCK_REASON)
                .await?
        } else {
            false
        };
    }

    let human_alert_sent = if changed > 0 {
        load_human_alert_target(shared.as_ref()).is_some_and(|target| {
            let card_label = card_id.as_deref().unwrap_or("-");
            let content = format!(
                "자동큐 safety-net 발동: dispatch {dispatch_id} / card {card_label} / session {tmux_session_name} / {READY_FOR_INPUT_STUCK_REASON}"
            );
            enqueue_lifecycle_notification_best_effort(
                shared.pg_pool.as_ref(),
                &target,
                Some(dispatch_id),
                "dispatch.stuck_at_ready",
                &content,
            )
        })
    } else {
        false
    };

    Ok(ReadyForInputFailureResult {
        dispatch_failed: changed > 0,
        card_id,
        card_marked,
        human_alert_sent,
    })
}
