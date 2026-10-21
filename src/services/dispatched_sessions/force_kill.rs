//! The session force-kill shared by the HTTP route and the direct CLI: the session read and the
//! forward fence, then either the Herdr arm or the lifecycle kill with its dispatch failure or retry.

use super::*;

pub(super) const FORCE_KILL_RETRY_LIMIT: i64 = 5;

type Reply = (StatusCode, Json<serde_json::Value>);

/// The surface that asked: only the HTTP route may hand a Herdr execution to the terminate service.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Entry {
    Direct,
    Http,
}

/// What the session read and the forward fence settled for a force-kill run on this node.
pub(super) struct Prepared<'a> {
    pub(super) session_key: &'a str,
    pub(super) tmux_name: String,
    pub(super) provider_info: Option<(ProviderKind, String)>,
    pub(super) pool: &'a sqlx::PgPool,
    pub(super) active_dispatch_id: Option<String>,
    pub(super) agent_id: Option<String>,
    pub(super) runtime_channel_id: Option<String>,
    pub(super) session_provider: Option<String>,
    /// The same read's hosted record; read only for an HTTP force-kill with terminate on.
    #[cfg(unix)]
    pub(super) hosted: Option<dispatched_sessions_db::ForceKillHosted>,
}

/// The terminate switch decides, before any IO, whether the session read also takes the record.
fn reads_hosted(entry: Entry) -> bool {
    #[cfg(unix)]
    return entry == Entry::Http && crate::services::discord::herdr_terminate::terminate_enabled();
    #[cfg(not(unix))]
    let _ = entry;
    #[cfg(not(unix))]
    return false;
}

pub(super) async fn force_kill_session_impl_with_reason_and_forwarding(
    state: &AppState,
    headers: &HeaderMap,
    session_key: &str,
    retry: bool,
    reason: &str,
    entry: Entry,
) -> Reply {
    let hosted = reads_hosted(entry);
    let prepared = match prepare(state, headers, session_key, retry, reason, hosted).await {
        Ok(prepared) => prepared,
        Err(reply) => return reply,
    };
    #[cfg(unix)]
    if let Some(reply) = super::herdr_kill::herdr_force_kill(state, &prepared, retry, reason).await
    {
        return reply;
    }
    legacy_force_kill(state, prepared, retry, reason).await
}

async fn prepare<'a>(
    state: &'a AppState,
    headers: &HeaderMap,
    session_key: &'a str,
    retry: bool,
    reason: &str,
    hosted: bool,
) -> Result<Prepared<'a>, Reply> {
    let session_key = session_key;

    let tmux_name = match tmux_name_from_session_key(session_key) {
        Some(name) => name,
        None => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(
                    json!({"error": "invalid session_key format — expected legacy host:tmux or namespaced provider/token/host:tmux"}),
                ),
            ));
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
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "postgres pool unavailable"})),
        ));
    };
    #[cfg(unix)]
    let loaded = if hosted {
        dispatched_sessions_db::load_force_kill_session_hosted_pg(pool, session_key, provider_name)
            .await
            .map(|row| row.map(|(session, record)| (session, Some(record))))
    } else {
        dispatched_sessions_db::load_force_kill_session_pg(pool, session_key, provider_name)
            .await
            .map(|row| row.map(|session| (session, None)))
    };
    #[cfg(not(unix))]
    let loaded = {
        let _ = hosted;
        dispatched_sessions_db::load_force_kill_session_pg(pool, session_key, provider_name)
            .await
            .map(|row| row.map(|session| (session, ())))
    };
    let (session, _record) = match loaded {
        Ok(Some(selected)) => selected,
        Ok(None) => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(json!({"error": "session not found"})),
            ));
        }
        Err(error) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            ));
        }
    };
    let (active_dispatch_id, agent_id, runtime_channel_id, session_provider, owner_instance_id) =
        session;

    let forward_context = crate::services::session_forwarding::ForwardCallerContext::from(state);
    if let Err(response) = crate::services::session_forwarding::enforce_receiver_fence(
        headers,
        owner_instance_id.as_deref(),
        forward_context.cluster_instance_id.as_deref(),
    ) {
        return Err(response);
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
                return Err(crate::services::session_forwarding::forward_force_kill(
                    &forward_context,
                    &target,
                    session_key,
                    retry,
                    reason,
                )
                .await);
            }
            crate::services::session_forwarding::ForwardResolution::Unavailable {
                status,
                body,
            } => {
                return Err((status, Json(body)));
            }
        }
    }

    Ok(Prepared {
        session_key,
        tmux_name,
        provider_info,
        pool,
        active_dispatch_id,
        agent_id,
        runtime_channel_id,
        session_provider,
        #[cfg(unix)]
        hosted: _record,
    })
}

async fn legacy_force_kill(
    state: &AppState,
    prepared: Prepared<'_>,
    retry: bool,
    reason: &str,
) -> Reply {
    let Prepared {
        session_key,
        tmux_name,
        provider_info,
        pool,
        active_dispatch_id,
        agent_id,
        runtime_channel_id,
        session_provider,
        ..
    } = prepared;
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
    let retry_meta = match dispatched_sessions_db::disconnect_session_and_prepare_retry_pg(
        pool,
        session_key,
        active_dispatch_id.as_deref(),
        retry,
    )
    .await
    {
        Ok(meta) => meta,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            );
        }
    };
    let (retry_dispatch_id, retry_skipped_reason) =
        match fail_or_retry(pool, session_key, retry_meta).await {
            Ok(outcome) => outcome,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": error})),
                );
            }
        };

    let queue_activation_requested =
        if activates_auto_queue(retry_dispatch_id.as_deref(), retry_skipped_reason) {
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

/// The dispatch outcome of a force-kill whose session is disconnected: the retry created through
/// the origin dispatch's single writer, or the dispatch failed and why no retry was made.
pub(super) async fn fail_or_retry(
    pool: &sqlx::PgPool,
    session_key: &str,
    retry_meta: Option<dispatched_sessions_db::RetryDispatchMeta>,
) -> Result<(Option<String>, Option<&'static str>), String> {
    let (mut retry_dispatch_id, mut retry_skipped_reason) = (None, None);
    if let Some(dispatched_sessions_db::RetryDispatchMeta {
        origin_dispatch_id,
        card_id,
        to_agent_id,
        dispatch_type,
        title,
        context,
        retry_count,
    }) = retry_meta
    {
        if retry_count >= FORCE_KILL_RETRY_LIMIT {
            if let Err(error) = dispatched_sessions_db::fail_force_killed_dispatch_without_retry_pg(
                pool,
                &origin_dispatch_id,
                Some(session_key),
            )
            .await
            {
                return Err(error);
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
                        return Err(format!(
                            "retry creation failed: {e}; standalone dispatch failure also failed: {fallback_error}"
                        ));
                    }
                    tracing::warn!(
                        "[force-kill] retry dispatch creation via postgres path failed for card {}: {e}",
                        meta.card_id
                    );
                }
            }
        }
    }
    Ok((retry_dispatch_id, retry_skipped_reason))
}

/// Whether the agent's auto-queue is woken: only when no retry was created or skipped.
pub(super) fn activates_auto_queue(
    retry_dispatch_id: Option<&str>,
    retry_skipped_reason: Option<&str>,
) -> bool {
    retry_dispatch_id.is_none() && retry_skipped_reason.is_none()
}
