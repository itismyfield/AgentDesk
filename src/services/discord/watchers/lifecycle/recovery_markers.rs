use super::*;

pub(in crate::services::discord) fn recovery_handled_channel_exists(
    shared: &SharedData,
    channel_id: u64,
) -> bool {
    let key = recovery_handled_channel_key(channel_id);

    if let Ok(value) = super::super::super::internal_api::get_kv_value(&key) {
        return value.is_some();
    }

    if let Some(pg_pool) = shared.pg_pool.as_ref() {
        return crate::utils::async_bridge::block_on_pg_result(
            pg_pool,
            move |pool| async move {
                sqlx::query_scalar::<_, bool>(
                    "SELECT EXISTS(
                         SELECT 1
                         FROM kv_meta
                         WHERE key = $1
                           AND (expires_at IS NULL OR expires_at > NOW())
                     )",
                )
                .bind(&key)
                .fetch_one(&pool)
                .await
                .map_err(|error| format!("load recovery handled marker {key}: {error}"))
            },
            |message| message,
        )
        .unwrap_or(false);
    }

    let _ = (shared, key);
    false
}

pub(in crate::services::discord) async fn store_recovery_handled_channels(
    shared: &SharedData,
    channel_ids: &[u64],
) {
    if channel_ids.is_empty() {
        return;
    }

    let marker_value = chrono::Utc::now().timestamp().to_string();
    let mut stored_via_internal_api = true;
    for channel_id in channel_ids {
        let key = recovery_handled_channel_key(*channel_id);
        if let Err(error) = super::super::super::internal_api::set_kv_value(&key, &marker_value) {
            tracing::debug!(
                "recovery handled marker fallback for {key}: direct runtime API unavailable: {error}"
            );
            stored_via_internal_api = false;
            break;
        }
    }
    if stored_via_internal_api {
        return;
    }

    if let Some(pg_pool) = shared.pg_pool.as_ref() {
        match pg_pool.begin().await {
            Ok(mut tx) => {
                for channel_id in channel_ids {
                    let key = recovery_handled_channel_key(*channel_id);
                    if let Err(error) = sqlx::query(
                        "INSERT INTO kv_meta (key, value, expires_at)
                         VALUES ($1, $2, NULL)
                         ON CONFLICT (key) DO UPDATE
                         SET value = EXCLUDED.value,
                             expires_at = EXCLUDED.expires_at",
                    )
                    .bind(&key)
                    .bind(&marker_value)
                    .execute(&mut *tx)
                    .await
                    {
                        tracing::warn!(
                            "failed to persist recovery handled marker {key} in postgres: {error}"
                        );
                        return;
                    }
                }
                if let Err(error) = tx.commit().await {
                    tracing::warn!("failed to commit recovery handled marker tx: {error}");
                }
            }
            Err(error) => {
                tracing::warn!("failed to begin recovery handled marker tx: {error}");
            }
        }
        return;
    }

    let _ = shared;
}

pub(in crate::services::discord) async fn clear_recovery_handled_channels(shared: &SharedData) {
    if let Err(error) =
        super::super::super::internal_api::clear_kv_prefix("recovery_handled_channel:")
    {
        tracing::debug!(
            "recovery handled marker clear fallback: direct runtime API unavailable: {error}"
        );
    } else {
        return;
    }

    if let Some(pg_pool) = shared.pg_pool.as_ref() {
        if let Err(error) =
            sqlx::query("DELETE FROM kv_meta WHERE key LIKE 'recovery_handled_channel:%'")
                .execute(pg_pool)
                .await
        {
            tracing::warn!("failed to clear recovery handled markers in postgres: {error}");
        }
        return;
    }

    let _ = shared;
}

pub(crate) async fn clear_provider_session_for_retry(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    tmux_session_name: &str,
    fallback_session_id: Option<&str>,
) {
    let stale_sid = {
        let mut data = shared.core.lock().await;
        let old = data
            .sessions
            .get(&channel_id)
            .and_then(|session| session.session_id.clone())
            .or_else(|| fallback_session_id.map(ToString::to_string));
        if let Some(session) = data.sessions.get_mut(&channel_id) {
            session.clear_provider_session();
        }
        old
    };

    let session_key = format!(
        "{}:{}",
        crate::services::platform::hostname_short(),
        tmux_session_name
    );
    super::super::super::adk_session::clear_provider_session_id(&session_key, shared.api_port)
        .await;

    if let Some(sid) = stale_sid {
        let _ = super::super::super::internal_api::clear_stale_session_id(&sid).await;
    }
}
