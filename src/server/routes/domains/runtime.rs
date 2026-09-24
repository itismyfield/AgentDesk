//! Node-local execution, health, hooks and session control shared by all profiles.
//! Configuration and orchestration administration is composed separately.
use super::super::{
    ApiRouter, AppState, agents, agents_crud, cluster, dispatched_sessions, dispatches, dm_reply,
    health_api, hooks, idle_recap, monitoring, protected_api_domain, provider_cli_api, queue_api,
    termination_events, turn_lease,
};
use axum::{
    Router,
    routing::{delete, get, patch, post},
};

pub(crate) fn router(state: AppState) -> ApiRouter {
    protected_api_domain(
        Router::new()
            .route(
                "/turn-lease/{provider}/{channel_id}",
                get(turn_lease::inspect),
            )
            .route("/turn-lease/release", post(turn_lease::release))
            .route("/health/detail", get(health_api::health_detail_handler))
            .route(
                "/doctor/startup/latest",
                get(health_api::startup_doctor_latest_handler),
            )
            .route(
                "/inflight/rebind",
                post(health_api::rebind_inflight_handler),
            )
            .route("/cluster/nodes", get(cluster::list_nodes))
            .route(
                "/cluster/machine-resources/history",
                get(cluster::machine_resource_history),
            )
            .route("/cluster/sessions", get(cluster::list_sessions))
            .route(
                "/cluster/routing-diagnostics",
                get(cluster::routing_diagnostics),
            )
            .route(
                "/doctor/stale-mailbox/repair",
                post(health_api::stale_mailbox_repair_handler),
            )
            .route(
                "/internal/link-dispatch-thread",
                post(dispatches::link_dispatch_thread),
            )
            .route("/internal/card-thread", get(dispatches::get_card_thread))
            .route(
                "/internal/pending-dispatch-for-thread",
                get(dispatches::get_pending_dispatch_for_thread),
            )
            .route(
                "/dispatched-sessions",
                get(dispatched_sessions::list_dispatched_sessions),
            )
            .route(
                "/dispatched-sessions/{id}",
                patch(dispatched_sessions::update_dispatched_session),
            )
            .route(
                "/dispatched-sessions/webhook",
                post(dispatched_sessions::hook_session).delete(dispatched_sessions::delete_session),
            )
            .route(
                "/dispatched-sessions/claude-session-id",
                get(dispatched_sessions::get_claude_session_id),
            )
            .route(
                "/dispatched-sessions/clear-stale-session-id",
                post(dispatched_sessions::clear_stale_session_id),
            )
            .route(
                "/dispatched-sessions/clear-session-id",
                post(dispatched_sessions::clear_session_id_by_key),
            )
            .route(
                "/sessions/{session_key}/force-kill",
                post(dispatched_sessions::force_kill_session),
            )
            .route(
                "/sessions/{session_key}/kill-tmux",
                post(dispatched_sessions::kill_tmux_session),
            )
            .route(
                "/sessions/{session_key}/reconcile-stale-turn",
                post(dispatched_sessions::reconcile_stale_turn),
            )
            .route(
                "/sessions/{session_key}/resume-previous",
                post(dispatched_sessions::resume_previous_session),
            )
            .route(
                "/sessions/{session_key}/idle-recap",
                post(idle_recap::post_idle_recap),
            )
            .route(
                "/sessions/{id}/tmux-output",
                get(dispatched_sessions::tmux_output),
            )
            .route(
                "/sessions/{id}/output",
                get(dispatched_sessions::tmux_output),
            )
            .route(
                "/session-termination-events",
                get(termination_events::list_termination_events),
            )
            .route("/channels/{id}/queue", get(queue_api::list_channel_queue))
            .route(
                "/channels/{id}/watcher-state",
                get(queue_api::get_watcher_state),
            )
            .route(
                "/channels/{id}/relay-recovery",
                post(health_api::relay_recovery_handler),
            )
            .route(
                "/channels/{channel_id}/monitoring",
                post(monitoring::upsert_monitoring).get(monitoring::list_monitoring),
            )
            .route(
                "/channels/{channel_id}/monitoring/{key}",
                delete(monitoring::remove_monitoring),
            )
            .route("/turns/{channel_id}/cancel", post(queue_api::cancel_turn))
            .route(
                "/provider-cli",
                get(provider_cli_api::get_provider_cli_status),
            )
            .route(
                "/agents/{id}/dispatched-sessions",
                get(agents::agent_dispatched_sessions),
            )
            .route("/agents/{id}/turn", get(agents::agent_turn))
            .route("/agents/{id}/turn/start", post(agents::start_agent_turn))
            .route("/agents/{id}/turn/stop", post(agents::stop_agent_turn))
            .route("/agents/{id}/transcripts", get(agents::agent_transcripts))
            .route("/agents/{id}/timeline", get(agents::agent_timeline))
            .route("/sessions", get(agents_crud::list_sessions))
            .route("/dm-reply/register", post(dm_reply::register_handler))
            .route("/hook/reset-status", post(hooks::reset_status))
            .route("/hook/skill-usage", post(hooks::skill_usage))
            .route(
                "/hook/session/{sessionKey}",
                delete(hooks::disconnect_session),
            )
            .route("/internal/node-probe", get(cluster::node_probe)),
        state,
    )
}
