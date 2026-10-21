//! The session force-kill shared by the HTTP route and the direct CLI: the session read, the
//! forward fence, the lifecycle kill and the dispatch failure or retry.

use super::*;

pub(super) const FORCE_KILL_RETRY_LIMIT: i64 = 5;

pub(super) async fn force_kill_session_impl_with_reason_and_forwarding(
    state: &AppState,
    headers: &HeaderMap,
    session_key: &str,
    retry: bool,
    reason: &str,
) -> (StatusCode, Json<serde_json::Value>) {
    let session_key = session_key;

    let tmux_name = match tmux_name_from_session_key(session_key) {
        Some(name) => name,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(
                    json!({"error": "invalid session_key format — expected legacy host:tmux or namespaced provider/token/host:tmux"}),
                ),
            );
        }
    };

    // Parse provider from tmux name
    let provider_info =
        crate::services::provider::parse_provider_and_channel_from_tmux_name(&tmux_name);

    // Query session from the authoritative store.
    let provider_name = provider_info
        .as_ref()
        .map(|(provider, _)| provider.as_str());
    let Some(pool) = state.pg_pool_ref() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "postgres pool unavailable"})),
        );
    };
    let (active_dispatch_id, agent_id, runtime_channel_id, session_provider, owner_instance_id) =
        match dispatched_sessions_db::load_force_kill_session_pg(pool, session_key, provider_name)
            .await
        {
            Ok(Some(tuple)) => tuple,
            Ok(None) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "session not found"})),
                );
            }
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": error})),
                );
            }
        };

    let forward_context = crate::services::session_forwarding::ForwardCallerContext::from(state);
    if let Err(response) = crate::services::session_forwarding::enforce_receiver_fence(
        headers,
        owner_instance_id.as_deref(),
        forward_context.cluster_instance_id.as_deref(),
    ) {
        return response;
    }
    if !crate::services::session_forwarding::is_forwarded_request(headers) {
        match crate::services::session_forwarding::resolve_forward_target(
            &forward_context,
            owner_instance_id.as_deref(),
            pool,
        )
        .await
        {
            crate::services::session_forwarding::ForwardResolution::Local => {}
            crate::services::session_forwarding::ForwardResolution::Forward(target) => {
                return crate::services::session_forwarding::forward_force_kill(
                    &forward_context,
                    &target,
                    session_key,
                    retry,
                    reason,
                )
                .await;
            }
            crate::services::session_forwarding::ForwardResolution::Unavailable {
                status,
                body,
            } => {
                return (status, Json(body));
            }
        }
    }

    let termination_reason_code = classify_session_termination_reason(reason);

    let row = ForceKillRow {
        pool,
        session_key,
        stored_provider: session_provider.as_deref(),
    };
    let lifecycle = force_kill_turn_for_row(
        state.health_registry.as_deref(),
        &TurnLifecycleTarget {
            provider: provider_info
                .as_ref()
                .map(|(provider, _)| provider.clone())
                .or_else(|| session_provider.as_deref().and_then(ProviderKind::from_str)),
            channel_id: runtime_channel_id
                .as_deref()
                .and_then(|channel_id| channel_id.parse::<u64>().ok())
                .map(poise::serenity_prelude::ChannelId::new),
            tmux_name: tmux_name.clone(),
        },
        row,
        reason,
        termination_reason_code,
    )
    .await;
    // A refused kill changed nothing, so the session and its dispatch stay as they are.
    if lifecycle.host_guard_kept() {
        let (error, unsupported) = ("session host is not legacy tmux", "session_host_not_tmux");
        let body = json!({"error": error, "unsupported": unsupported, "session_key": session_key});
        return (StatusCode::CONFLICT, Json(body));
    }

    // 1. Kill tmux session (or confirm the runtime path already stopped it).
    let tmux_killed = lifecycle.tmux_killed;

    // 2. Clear persistent inflight state by matching tmux_session_name/channel_id.
    let inflight_cleared = lifecycle.inflight_cleared;

    // 3. Update session → disconnected, clear active fields
    // 4. Mark dispatch → failed
    // 5. Optionally create retry dispatch via central path (#108)
    let mut retry_dispatch_id: Option<String> = None;
    let retry_meta = match dispatched_sessions_db::disconnect_session_and_prepare_retry_pg(
        pool,
        session_key,
        active_dispatch_id.as_deref(),
        retry,
    )
    .await
    {
        Ok(meta) => meta.map(|meta| {
            (
                meta.origin_dispatch_id,
                meta.card_id,
                meta.to_agent_id,
                meta.dispatch_type,
                meta.title,
                meta.context,
                meta.retry_count,
            )
        }),
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            );
        }
    };

    // Create retry dispatch via central authoritative path (#108)
    let mut retry_skipped_reason: Option<&'static str> = None;
    if let Some((
        origin_dispatch_id,
        card_id,
        to_agent_id,
        dispatch_type,
        title,
        context,
        retry_count,
    )) = retry_meta
    {
        if retry_count >= FORCE_KILL_RETRY_LIMIT {
            if let Err(error) = dispatched_sessions_db::fail_force_killed_dispatch_without_retry_pg(
                pool,
                &origin_dispatch_id,
                Some(session_key),
            )
            .await
            {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": error})),
                );
            }
            retry_skipped_reason = Some("retry_limit_reached");
            tracing::warn!(
                "[force-kill] retry dispatch skipped for card {}: retry_count={} limit={}",
                card_id,
                retry_count,
                FORCE_KILL_RETRY_LIMIT
            );
        } else {
            let ctx: serde_json::Value = context
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or_else(|| json!({}));

            let meta = dispatched_sessions_db::RetryDispatchMeta {
                origin_dispatch_id,
                card_id,
                to_agent_id,
                dispatch_type,
                title,
                context: Some(ctx.to_string()),
                retry_count,
            };
            match dispatched_sessions_db::create_retry_dispatch_pg(pool, &meta).await {
                Ok(new_id) => {
                    retry_dispatch_id = Some(new_id);
                }
                Err(e) => {
                    if let Err(fallback_error) =
                        dispatched_sessions_db::fail_force_killed_dispatch_without_retry_pg(
                            pool,
                            &meta.origin_dispatch_id,
                            Some(session_key),
                        )
                        .await
                    {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({
                                "error": format!(
                                    "retry creation failed: {e}; standalone dispatch failure also failed: {fallback_error}"
                                )
                            })),
                        );
                    }
                    tracing::warn!(
                        "[force-kill] retry dispatch creation via postgres path failed for card {}: {e}",
                        meta.card_id
                    );
                }
            }
        }
    }

    let queue_activation_requested =
        if retry_dispatch_id.is_none() && retry_skipped_reason.is_none() {
            if let Some(ref aid) = agent_id {
                spawn_auto_queue_activate_for_agent(state.clone(), aid.clone());
                true
            } else {
                false
            }
        } else {
            false
        };

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::warn!(
        "  [{ts}] ⚡ force-kill: session={}, tmux_killed={}, inflight_cleared={}, dispatch_failed={:?}, lifecycle={}",
        session_key,
        tmux_killed,
        inflight_cleared,
        active_dispatch_id,
        lifecycle.lifecycle_path
    );

    if tmux_killed && !lifecycle.termination_recorded {
        crate::services::termination_audit::record_termination_with_handles(
            state.pg_pool_ref(),
            session_key,
            active_dispatch_id.as_deref(),
            "force_kill_api",
            termination_reason_code,
            Some(reason),
            None,
            None,
            Some(false),
        );
    }

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "tmux_killed": tmux_killed,
            "inflight_cleared": inflight_cleared,
            "lifecycle_path": lifecycle.lifecycle_path,
            "queued_remaining": lifecycle.queue_depth,
            "queue_preserved": lifecycle.queue_preserved,
            "dispatch_failed": active_dispatch_id,
            "retry_dispatch_id": retry_dispatch_id,
            "retry_limit": FORCE_KILL_RETRY_LIMIT,
            "retry_skipped_reason": retry_skipped_reason,
            "queue_activation_requested": queue_activation_requested,
        })),
    )
}
