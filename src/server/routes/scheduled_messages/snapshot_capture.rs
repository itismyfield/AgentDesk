//! #4658 snapshot capture for the create route. Extracted from the parent
//! module to keep it under the giant-file threshold; the SQL still delegates to
//! `crate::db::scheduled_messages` and the snapshot service.

use super::*;

/// #4658: capture the immutable context snapshot and insert the definition in
/// one transaction. The snapshot row is written first so the FK and
/// `chk_smsg_snapshot_required` are satisfied by commit.
pub(super) async fn create_scheduled_message_with_snapshot(
    pool: &PgPool,
    mut new: NewScheduledMessage,
) -> ApiResponse {
    use crate::services::scheduled_messages::context_snapshot::{
        CaptureError, capture_snapshot_tx,
    };

    // Source channel: the agent's turn channel (target override, else primary).
    // This is the channel whose live conversation the reservation freezes.
    let (source_channel_id, intent) = resolve_snapshot_source(pool, &new).await?;

    let mut tx = pool.begin().await.map_err(|error| {
        app_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("begin snapshot transaction: {error}"),
        )
    })?;

    let snapshot = match capture_snapshot_tx(&mut tx, &source_channel_id, None, &intent).await {
        Ok(snapshot) => snapshot,
        Err(CaptureError::EmptyContext) => {
            return Err(app_error(
                StatusCode::BAD_REQUEST,
                "contextStrategy 'snapshot' requires existing conversation context in the \
                 source channel, but none was found",
            ));
        }
        Err(CaptureError::Db(error)) => {
            return Err(app_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("capture context snapshot: {error}"),
            ));
        }
    };
    new.context_snapshot_id = Some(snapshot.id);

    match db::insert_scheduled_message_tx(&mut tx, &new).await {
        Ok(row) => {
            tx.commit().await.map_err(|error| {
                app_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("commit snapshot transaction: {error}"),
                )
            })?;
            Ok((
                StatusCode::CREATED,
                Json(json!({"scheduledMessage": row.to_api_json()})),
            ))
        }
        Err(error) if db::is_unique_violation(&error) => {
            // Roll back so the just-captured snapshot is discarded; the existing
            // active definition keeps its own snapshot.
            let _ = tx.rollback().await;
            let existing = match new.dedupe_key.as_deref() {
                Some(key) => db::find_active_by_dedupe_key_pg(pool, key)
                    .await
                    .ok()
                    .flatten(),
                None => None,
            };
            Ok((
                StatusCode::CONFLICT,
                Json(json!({
                    "error": "an active scheduled message with this dedupeKey already exists",
                    "scheduledMessage": existing.map(|row| row.to_api_json()),
                })),
            ))
        }
        Err(error) => {
            let _ = tx.rollback().await;
            Err(app_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("create scheduled message: {error}"),
            ))
        }
    }
}

/// Resolve the source channel + captured execution intent for a snapshot
/// reservation. Source = target channel override, else the agent's primary
/// channel (the same channel the fire path runs the turn in).
async fn resolve_snapshot_source(
    pool: &PgPool,
    new: &NewScheduledMessage,
) -> Result<
    (
        String,
        crate::services::scheduled_messages::context_snapshot::SnapshotIntent,
    ),
    AppError,
> {
    use crate::services::scheduled_messages::context_snapshot::SnapshotIntent;

    let agent_id = new.agent_id.as_deref().ok_or_else(|| {
        app_error(
            StatusCode::BAD_REQUEST,
            "snapshot strategy requires an agent",
        )
    })?;
    let bindings = crate::db::agents::load_agent_channel_bindings_pg(pool, agent_id)
        .await
        .map_err(|error| {
            app_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("load agent bindings: {error}"),
            )
        })?
        .ok_or_else(|| {
            app_error(
                StatusCode::BAD_REQUEST,
                format!("agent '{agent_id}' not found"),
            )
        })?;
    let primary_channel = bindings.primary_channel().ok_or_else(|| {
        app_error(
            StatusCode::BAD_REQUEST,
            format!("agent '{agent_id}' has no primary channel"),
        )
    })?;
    let resolve = |value: &str| -> Option<String> {
        crate::services::dispatches::outbox_route::resolve_channel_alias_pub(value)
            .or_else(|| value.parse::<u64>().ok())
            .map(|id| id.to_string())
    };
    let source_channel_id = match new.target_channel_id.as_deref() {
        Some(target) => resolve(target).ok_or_else(|| {
            app_error(
                StatusCode::BAD_REQUEST,
                format!("target channel is invalid: {target}"),
            )
        })?,
        None => resolve(&primary_channel).ok_or_else(|| {
            app_error(
                StatusCode::BAD_REQUEST,
                format!("agent '{agent_id}' primary channel is invalid: {primary_channel}"),
            )
        })?,
    };
    let intent = SnapshotIntent {
        provider: bindings
            .resolved_primary_provider_kind()
            .map(|provider| provider.as_str().to_string()),
        model: None,
        reasoning_effort: None,
        fast_mode: None,
        workspace_hint: Some(primary_channel),
    };
    Ok((source_channel_id, intent))
}
