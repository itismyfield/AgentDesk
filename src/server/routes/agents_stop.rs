use super::*;

/// POST /api/agents/:id/turn/stop
pub async fn stop_agent_turn(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_required_response();
    };
    let session = {
        match agent_exists_pg(pool, &id).await {
            Ok(true) => {}
            Ok(false) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "agent not found"})),
                );
            }
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("query: {e}")})),
                );
            }
        }

        match find_agent_turn_session_pg(pool, &id).await {
            Ok(session) => session,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("query: {e}")})),
                );
            }
        }
    };

    // A session the host check refuses, working or idle, is neither stopped nor marked.
    if let Some(session) = session.as_ref()
        && let Some(reason) = session.host_unsupported.as_deref()
    {
        let (unsupported, key) = ("session_host_not_tmux", &session.session_key);
        let body = json!({"error": reason, "unsupported": unsupported, "session_key": key});
        return (StatusCode::CONFLICT, Json(body));
    }
    let Some(session) = session.filter(|candidate| candidate.is_working) else {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "no active turn found for agent",
                "agent_id": id,
                "status": "idle",
            })),
        );
    };

    if session.session_key.trim().is_empty() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "active session is missing session_key"})),
        );
    }

    let admission = match session.runtime_channel_id.as_deref() {
        Some(channel) => crate::services::cluster::channel_home::admit_command(
            channel,
            session.provider.as_deref().unwrap_or("agent_stop"),
        ),
        None => crate::services::cluster::channel_home::admit_unattributed(),
    };
    let permit = match admission {
        Ok(permit) => permit,
        Err(reason) => {
            return (
                StatusCode::CONFLICT,
                Json(json!({"error": reason.to_string()})),
            );
        }
    };
    crate::services::cluster::channel_home::command_scope(permit, async {
        let session_key = session.session_key.clone();
        let tmux_name = extract_tmux_name(&session_key).unwrap_or_else(|| session_key.clone());
        let lifecycle = stop_turn_preserving_queue(
            state.health_registry.as_deref(),
            &TurnLifecycleTarget {
                provider: session.provider.as_deref().and_then(ProviderKind::from_str),
                channel_id: session
                    .runtime_channel_id
                    .as_deref()
                    .and_then(|value| value.parse::<u64>().ok())
                    .map(poise::serenity_prelude::ChannelId::new),
                tmux_name: tmux_name.clone(),
            },
            &format!("사용자가 {id} 에이전트 턴 수동 중단 (POST /api/agents/{id}/turn/stop)"),
        )
        .await;
        // A turn the host guard kept is running on: its session is not marked disconnected.
        if lifecycle.host_guard_kept() {
            let (error, unsupported) = ("session host is not legacy tmux", "session_host_not_tmux");
            let body =
                json!({"error": error, "unsupported": unsupported, "session_key": session_key});
            return (StatusCode::CONFLICT, Json(body));
        }

        mark_session_disconnected_pg(pool, &session_key).await;

        let status = StatusCode::OK;
        let Json(mut body) = Json(json!({
            "ok": true,
            "session_key": session_key,
            "tmux_session": tmux_name,
            "tmux_killed": lifecycle.tmux_killed,
            "lifecycle_path": lifecycle.lifecycle_path,
            "queued_remaining": lifecycle.queue_depth,
            "queue_preserved": lifecycle.queue_preserved,
        }));
        body["agent_id"] = json!(id);
        body["session_key"] = json!(session_key);
        body["status"] = json!(if status == StatusCode::OK {
            "stopped"
        } else {
            "error"
        });
        (status, Json(body))
    })
    .await
}
