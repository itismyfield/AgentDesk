use super::{
    AppError, AppResult, AppState, ErrorCode, Json, ListDispatchedSessionsQuery, Query, State,
    StatusCode, dispatched_sessions_db, json,
};
use crate::db::dispatched_sessions::DispatchedSessionSnapshot;
use crate::db::session_agent_resolution::{
    normalize_thread_channel_id, parse_thread_channel_id_from_session_key,
};
use crate::domain::session_identity::tmux_name_from_session_key;
use crate::services::session_activity::SessionActivityResolver;

/// GET /api/dispatched-sessions
pub async fn list_dispatched_sessions(
    State(state): State<AppState>,
    Query(params): Query<ListDispatchedSessionsQuery>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let include_all = params.include_merged.as_deref() == Some("1");
    if let Some(pool) = state.pg_pool_ref() {
        return match list_dispatched_sessions_pg(pool, include_all).await {
            Ok(mut sessions) => {
                let worker_nodes = match crate::services::cluster::node_registry::list_worker_nodes(
                    pool,
                    state.config.cluster.lease_ttl_secs.max(1),
                )
                .await
                {
                    Ok(nodes) => nodes,
                    Err(error) => {
                        tracing::warn!(
                            "failed to list worker nodes for dispatched session owner routing: {error}"
                        );
                        Vec::new()
                    }
                };
                crate::services::cluster::session_routing::enrich_session_owner_routing(
                    &mut sessions,
                    state.cluster_instance_id.as_deref(),
                    &worker_nodes,
                );
                Ok((StatusCode::OK, Json(json!({"sessions": sessions}))))
            }
            Err(error) => Err(AppError::internal(error).with_code(ErrorCode::Database)),
        };
    }

    Err(AppError::internal("postgres pool unavailable").with_code(ErrorCode::Database))
}

pub(crate) async fn list_dispatched_sessions_pg(
    pool: &sqlx::PgPool,
    include_all: bool,
) -> Result<Vec<serde_json::Value>, String> {
    let snapshots =
        dispatched_sessions_db::list_dispatched_session_snapshots_pg(pool, include_all).await?;
    let mut resolver = SessionActivityResolver::new();
    let mut sessions = Vec::with_capacity(snapshots.len());
    for snapshot in snapshots {
        let DispatchedSessionSnapshot {
            id,
            session_key,
            instance_id,
            agent_id,
            provider,
            status,
            active_dispatch_id,
            model,
            tokens,
            cwd,
            last_heartbeat,
            session_info,
            department_id,
            sprite_number,
            avatar_emoji,
            department_name,
            department_name_ko,
            department_color,
            thread_channel_id,
            dispatch_thread_id,
            dispatch_type,
            dispatch_row_status,
            dispatch_created_at,
            dispatch_sent_at,
            auto_queue_entry_id,
            auto_queue_run_id,
            auto_queue_slot_index,
            auto_queue_thread_group,
        } = snapshot;
        let last_heartbeat = last_heartbeat.map(|value| value.to_rfc3339());
        let tmux_session = tmux_session_name_from_session_key(session_key.as_deref());
        let resolved_thread_channel_id = normalize_thread_channel_id(dispatch_thread_id.as_deref())
            .or_else(|| normalize_thread_channel_id(thread_channel_id.as_deref()))
            .or_else(|| {
                session_key
                    .as_deref()
                    .and_then(parse_thread_channel_id_from_session_key)
            });

        let effective = resolver.resolve(
            session_key.as_deref(),
            status.as_deref(),
            active_dispatch_id.as_deref(),
            last_heartbeat.as_deref(),
        );
        if !include_all && !effective.is_working && effective.active_dispatch_id.is_none() {
            continue;
        }
        if !include_all && thread_channel_id.is_some() && !effective.is_working {
            continue;
        }

        // A new dispatch may precede the hook that refreshes the cached label.
        let active_dispatch_present = effective.active_dispatch_id.is_some();
        let dispatch_type_label = active_dispatch_present
            .then(|| dispatch_type_label_for_session_info(dispatch_type.as_deref()));
        let session_info_effective: Option<String> = match (
            active_dispatch_present,
            dispatch_type.as_deref(),
            session_info.as_deref(),
        ) {
            (true, Some(td_type), Some(existing))
                if existing.trim_start().starts_with("── ")
                    && !existing.contains(&format!("── {td_type} dispatch ──")) =>
            {
                dispatch_type_label.map(|label| format!("── {label} ──"))
            }
            (true, Some(_), None) => dispatch_type_label.map(|label| format!("── {label} ──")),
            _ => session_info.clone(),
        };

        let delivery_state = if active_dispatch_present {
            classify_delivery_state(
                dispatch_row_status.as_deref(),
                effective.is_working,
                dispatch_sent_at.is_some(),
            )
        } else {
            "none"
        };
        let dispatch_created_at_iso = dispatch_created_at.map(|value| value.to_rfc3339());
        let dispatch_sent_at_iso = dispatch_sent_at.map(|value| value.to_rfc3339());

        sessions.push(json!({
            "id": id.to_string(),
            "session_key": session_key,
            "instance_id": instance_id,
            "agent_id": agent_id,
            "provider": provider,
            "status": effective.status,
            "active_dispatch_id": effective.active_dispatch_id,
            "model": model,
            "tokens": tokens,
            "cwd": cwd,
            "last_heartbeat": last_heartbeat,
            "session_info": session_info_effective,
            "session_info_raw": session_info,
            "dispatch_type": dispatch_type,
            "dispatch_row_status": dispatch_row_status,
            "delivery_state": delivery_state,
            "dispatch_created_at": dispatch_created_at_iso,
            "dispatch_sent_at": dispatch_sent_at_iso,
            "linked_agent_id": agent_id,
            "last_seen_at": last_heartbeat,
            "name": session_key,
            "department_id": department_id,
            "sprite_number": sprite_number,
            "avatar_emoji": avatar_emoji.unwrap_or_else(|| "\u{1F916}".to_string()),
            "connected_at": null,
            "department_name": department_name,
            "department_name_ko": department_name_ko,
            "department_color": department_color,
            "thread_channel_id": thread_channel_id,
            "dispatch_thread_id": dispatch_thread_id,
            "resolved_thread_channel_id": resolved_thread_channel_id,
            "tmux_session": tmux_session,
            "auto_queue_entry_id": auto_queue_entry_id,
            "auto_queue_run_id": auto_queue_run_id,
            "auto_queue_slot_index": auto_queue_slot_index,
            "auto_queue_thread_group": auto_queue_thread_group,
            "recovery_identifiers": {
                "session_key": session_key,
                "tmux_session": tmux_session,
                "active_dispatch_id": effective.active_dispatch_id,
                "thread_channel_id": resolved_thread_channel_id,
                "auto_queue_entry_id": auto_queue_entry_id,
                "auto_queue_run_id": auto_queue_run_id,
                "auto_queue_slot_index": auto_queue_slot_index,
                "auto_queue_thread_group": auto_queue_thread_group,
            },
        }));
    }

    Ok(sessions)
}

fn dispatch_type_label_for_session_info(dispatch_type: Option<&str>) -> String {
    let kind = match dispatch_type {
        Some("implementation") => "구현",
        Some("review") => "리뷰",
        Some("rework") => "리워크",
        Some("review-decision") => "리뷰 검토",
        Some("pm-decision") => "PM 판단",
        Some("e2e-test") => "E2E 테스트",
        Some("consultation") => "상담",
        Some("phase-gate") => "phase-gate",
        Some(other) => return format!("{other} dispatch"),
        None => return "dispatch".to_string(),
    };
    format!("{kind} dispatch")
}

fn classify_delivery_state(
    td_status: Option<&str>,
    session_is_working: bool,
    sent_at_is_set: bool,
) -> &'static str {
    match td_status.map(str::to_ascii_lowercase).as_deref() {
        Some("completed") => "completed",
        Some("failed") => "failed",
        Some("cancelled") => "cancelled",
        Some("dispatched") | Some("in_progress") => {
            // Delivery must have happened before a working session counts as active.
            if !sent_at_is_set {
                "queued"
            } else if session_is_working {
                "codex_active"
            } else {
                "delivered"
            }
        }
        Some("pending") => "queued",
        _ => "unknown",
    }
}

fn tmux_session_name_from_session_key(session_key: Option<&str>) -> Option<String> {
    tmux_name_from_session_key(session_key?)
}

#[cfg(test)]
mod recovery_identifier_tests {
    use super::tmux_session_name_from_session_key;

    #[test]
    fn tmux_session_name_from_session_key_preserves_provider_prefixed_hosts() {
        assert_eq!(
            tmux_session_name_from_session_key(Some(
                "codex/hash123/mac-mini:AgentDesk-codex-adk-cdx"
            ))
            .as_deref(),
            Some("AgentDesk-codex-adk-cdx")
        );
        assert_eq!(
            tmux_session_name_from_session_key(Some("missing-colon")),
            None
        );
        assert_eq!(tmux_session_name_from_session_key(Some("host:   ")), None);
    }
}

#[cfg(test)]
mod dispatch_surface_tests {
    use super::{classify_delivery_state, dispatch_type_label_for_session_info};

    // #2036 Surface 1: label resolver derives the dispatch decorator string
    // straight from task_dispatches.dispatch_type so a same-thread
    // phase-gate → implementation transition does not show a stale label.
    #[test]
    fn dispatch_type_label_covers_known_types() {
        assert_eq!(
            dispatch_type_label_for_session_info(Some("implementation")),
            "구현 dispatch"
        );
        assert_eq!(
            dispatch_type_label_for_session_info(Some("phase-gate")),
            "phase-gate dispatch"
        );
        assert_eq!(
            dispatch_type_label_for_session_info(Some("review")),
            "리뷰 dispatch"
        );
        assert_eq!(
            dispatch_type_label_for_session_info(Some("review-decision")),
            "리뷰 검토 dispatch"
        );
        assert_eq!(dispatch_type_label_for_session_info(None), "dispatch");
    }

    // #2036 Surface 2: delivery_state collapses bridge-queue-pending vs
    // codex-actively-running into a single API field.
    #[test]
    fn delivery_state_dispatched_and_working_is_codex_active() {
        assert_eq!(
            classify_delivery_state(Some("dispatched"), true, true),
            "codex_active"
        );
        // #2036 review fix: when sent_at is NOT set, session.is_working reflects
        // the previous turn on the same session (bridge per-channel queue
        // window). The new dispatch hasn't been delivered to codex yet, so
        // delivery_state must be "queued", not "codex_active".
        assert_eq!(
            classify_delivery_state(Some("in_progress"), true, false),
            "queued"
        );
    }

    #[test]
    fn delivery_state_dispatched_not_working_with_sent_event_is_delivered() {
        assert_eq!(
            classify_delivery_state(Some("dispatched"), false, true),
            "delivered"
        );
    }

    #[test]
    fn delivery_state_dispatched_not_working_no_sent_event_is_queued() {
        assert_eq!(
            classify_delivery_state(Some("dispatched"), false, false),
            "queued"
        );
        assert_eq!(
            classify_delivery_state(Some("pending"), false, false),
            "queued"
        );
    }

    #[test]
    fn delivery_state_terminal_statuses_pass_through() {
        assert_eq!(
            classify_delivery_state(Some("completed"), false, true),
            "completed"
        );
        assert_eq!(
            classify_delivery_state(Some("failed"), false, false),
            "failed"
        );
        assert_eq!(
            classify_delivery_state(Some("cancelled"), false, false),
            "cancelled"
        );
    }

    #[test]
    fn delivery_state_uppercase_status_is_normalized() {
        // Defensive: callers occasionally write status values uppercase, so
        // classify_delivery_state lowercases before matching.
        // NOTE: per #2036, sent_at dominates session_is_working — so a
        // DISPATCHED turn only reads "codex_active" once sent_at is set
        // (the bridge handed the prompt to codex); with sent_at unset it is
        // still "queued". This test pins the case-normalization, not the
        // sent_at gating, so it passes sent_at_is_set=true for the active case.
        assert_eq!(
            classify_delivery_state(Some("DISPATCHED"), true, true),
            "codex_active"
        );
        assert_eq!(
            classify_delivery_state(Some("DISPATCHED"), true, false),
            "queued"
        );
        assert_eq!(
            classify_delivery_state(Some("Completed"), false, false),
            "completed"
        );
    }
}
