use super::*;

async fn hook_session_pg(
    state: &AppState,
    pool: &sqlx::PgPool,
    body: HookSessionBody,
    expected_terminal_nonce: Option<&str>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let mut thread_channel_id = normalize_thread_channel_id(body.thread_channel_id.as_deref())
        .or_else(|| {
            body.name
                .as_deref()
                .and_then(parse_thread_channel_name)
                .map(|(_, tid)| tid.to_string())
        })
        .or_else(|| parse_thread_channel_id_from_session_key(&body.session_key));
    if thread_channel_id.is_none()
        && let Some(dispatch_id) = body.dispatch_id.as_deref()
    {
        thread_channel_id =
            dispatched_sessions_db::load_dispatch_thread_id_pg(pool, dispatch_id).await;
    }

    let agent_id = resolve_agent_id_for_session_pg(
        pool,
        None,
        Some(&body.session_key),
        body.name.as_deref(),
        thread_channel_id.as_deref(),
        body.dispatch_id.as_deref(),
        body.channel_id.as_deref(),
    )
    .await;

    let status = normalize_incoming_session_status(body.status.as_deref());
    let provider = body.provider.as_deref().unwrap_or("claude");
    // `None` here means "metadata-only hook" — the upsert must preserve the
    // existing `sessions.tokens` (#2045 follow-up: `save_provider_session_id`
    // and similar callers used to zero this column on every metadata update).
    let tokens = body.tokens.map(|t| t as i64);
    let active_dispatch_id = normalize_hook_active_dispatch_id(status, body.dispatch_id.as_deref());
    let turn_start_nonce = body
        .turn_start_nonce
        .as_deref()
        .filter(|value| !value.is_empty());
    let dispatched_origin = body.dispatched_origin.unwrap_or(false);
    let instance_id = body
        .instance_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or(state.cluster_instance_id.as_deref());
    let claude_session_id = body.claude_session_id.as_deref().filter(|s| !s.is_empty());
    let raw_provider_session_id = body.session_id.as_deref().filter(|s| !s.is_empty());
    let identity = canonical_identity::parse_hook_identity(
        &body.session_key,
        provider,
        body.identity_kind.as_deref(),
        body.discord_token_hash.as_deref(),
        body.channel_id.as_deref(),
    )
    .map_err(|error| {
        AppError::bad_request(format!("invalid canonical session identity: {error:?}"))
    })?;

    // #2045 Finding 7 (P2): the upsert helper now reports whether the row
    // was inserted in this transaction (`xmax = 0` RETURNING). The earlier
    // pattern of "SELECT exists, then upsert" raced under cluster hand-off
    // and could broadcast `dispatched_session_new` twice for the same
    // session_key — once per concurrent webhook.
    let params = dispatched_sessions_db::HookSessionUpsert {
        session_key: &body.session_key,
        instance_id,
        agent_id: agent_id.as_deref(),
        provider,
        status,
        session_info: body.session_info.as_deref(),
        model: body.model.as_deref(),
        tokens,
        cwd: body.cwd.as_deref(),
        active_dispatch_id: active_dispatch_id.as_deref(),
        thread_channel_id: thread_channel_id.as_deref(),
        // #3207 (part 2) P0: persist the unique channel id so worktree reuse
        // can require an exact channel match. Prefer the explicit channel id
        // from the hook body; fall back to the resolved thread channel id so
        // thread sessions (which set `thread_channel_id` but may omit
        // `channel_id`) still scope correctly.
        channel_id: body
            .channel_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .or(thread_channel_id.as_deref()),
        claude_session_id,
        raw_provider_session_id,
        turn_start_nonce,
        dispatched_origin,
    };
    let result = match expected_terminal_nonce {
        Some(nonce) => {
            crate::db::dispatched_session_canonical_identity::upsert_hook_session_terminal_pg(
                pool, params, identity, nonce,
            )
            .await
        }
        None => {
            crate::db::dispatched_session_canonical_identity::upsert_hook_session_with_identity_pg(
                pool, params, identity,
            )
            .await
        }
    };

    match result {
        Ok(outcome) => {
            let is_new_session = outcome.inserted;
            let resolved_session_key = outcome.session_key;
            let dispatch_id = body.dispatch_id.clone();

            crate::kanban::fire_event_hooks_with_backends(
                &state.engine,
                "on_session_status_change",
                "OnSessionStatusChange",
                json!({
                    "session_key": resolved_session_key,
                    "instance_id": instance_id,
                    "status": status,
                    "agent_id": agent_id,
                    "dispatch_id": dispatch_id,
                    "provider": provider,
                }),
            );

            if is_user_wait_status(status)
                && let Some(aid) = agent_id.as_ref()
            {
                spawn_auto_queue_activate_for_agent(state.clone(), aid.clone());
            }

            match dispatched_sessions_db::load_session_event_payload_pg(pool, &resolved_session_key)
                .await
            {
                Ok(Some(payload)) => {
                    if is_new_session {
                        crate::eventbus::emit_event(
                            &state.broadcast_tx,
                            "dispatched_session_new",
                            payload,
                        );
                    } else {
                        crate::eventbus::emit_batched_event(
                            &state.batch_buffer,
                            "dispatched_session_update",
                            &resolved_session_key,
                            payload,
                        );
                    }
                }
                Ok(None) => {}
                Err(error) => tracing::warn!(
                    "[dispatched-sessions] hook_session_pg: failed to load session payload for {}: {}",
                    body.session_key,
                    error
                ),
            }

            if let Some(aid) = agent_id.as_deref() {
                match dispatched_sessions_db::load_agent_status_payload_pg(
                    pool,
                    aid,
                    &resolved_session_key,
                )
                .await
                {
                    Ok(Some(agent)) => {
                        crate::eventbus::emit_batched_event(
                            &state.batch_buffer,
                            "agent_status",
                            aid,
                            agent,
                        );
                    }
                    Ok(None) => {}
                    Err(error) => tracing::warn!(
                        "[dispatched-sessions] hook_session_pg: failed to load agent payload for {} / {}: {}",
                        aid,
                        body.session_key,
                        error
                    ),
                }
            }

            Ok((StatusCode::OK, Json(json!({"ok": true}))))
        }
        Err(error) => {
            if let Some(kind) = error.conflict_kind() {
                let channel_id = body
                    .channel_id
                    .as_deref()
                    .and_then(|value| value.parse::<u64>().ok())
                    .unwrap_or(0);
                crate::services::observability::metrics::record_session_identity_conflict(
                    channel_id, provider, kind,
                );
                tracing::warn!(
                    conflict_kind = kind.as_str(),
                    provider,
                    channel_id,
                    "canonical session identity write rejected"
                );
            }
            Err(
                crate::db::dispatched_session_canonical_identity::hook_session_upsert_error_to_app_error(
                    error,
                ),
            )
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct HookSessionQuery {
    pub expected_turn_nonce: Option<String>,
}

pub async fn hook_session_with_query(
    State(state): State<AppState>,
    Query(query): Query<HookSessionQuery>,
    Json(body): Json<HookSessionBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    if let Some(pool) = state.pg_pool_ref() {
        return hook_session_pg(&state, pool, body, query.expected_turn_nonce.as_deref()).await;
    }
    Err(AppError::internal("postgres pool unavailable").with_code(ErrorCode::Database))
}
