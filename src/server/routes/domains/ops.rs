use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{delete, get, patch, post},
};
use serde_json::Value;

use super::super::{
    ApiRouter, AppState, auto_queue, cron_api, dispatched_sessions, dispatches, docs, health_api,
    hooks, log_deprecated_alias, maintenance, messages, pipeline, protected_api_domain,
    provider_cli_api, queue_api, skills_api, termination_events,
};

// Category: dispatches, queue, and ops

pub(crate) fn router(state: AppState) -> ApiRouter {
    protected_api_domain(
        Router::new()
            .route(
                "/dispatches",
                get(dispatches::list_dispatches).post(dispatches::create_dispatch),
            )
            .route("/health/detail", get(health_api::health_detail_handler))
            .route(
                "/doctor/startup/latest",
                get(health_api::startup_doctor_latest_handler),
            )
            .route(
                "/doctor/stale-mailbox/repair",
                post(health_api::stale_mailbox_repair_handler),
            )
            .route(
                "/dispatches/{id}",
                get(dispatches::get_dispatch).patch(dispatches::update_dispatch),
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
                "/pipeline/stages",
                get(pipeline::get_stages)
                    .put(pipeline::put_stages)
                    .delete(pipeline::delete_stages),
            )
            .route(
                "/pipeline/cards/{card_id}",
                get(pipeline::get_card_pipeline),
            )
            .route(
                "/pipeline/cards/{card_id}/history",
                get(pipeline::get_card_history),
            )
            .route(
                "/pipeline/cards/{card_id}/transcripts",
                get(pipeline::get_card_transcripts),
            )
            .route(
                "/pipeline/config/default",
                get(pipeline::get_default_pipeline),
            )
            .route(
                "/pipeline/config/effective",
                get(pipeline::get_effective_pipeline),
            )
            .route(
                "/pipeline/config/repo/{owner}/{repo}",
                get(pipeline::get_repo_pipeline).put(pipeline::set_repo_pipeline),
            )
            .route(
                "/pipeline/config/agent/{agent_id}",
                get(pipeline::get_agent_pipeline).put(pipeline::set_agent_pipeline),
            )
            .route("/pipeline/config/graph", get(pipeline::get_pipeline_graph))
            .route(
                "/dispatched-sessions",
                get(dispatched_sessions::list_dispatched_sessions),
            )
            .route(
                "/dispatched-sessions/cleanup",
                delete(dispatched_sessions::cleanup_sessions),
            )
            .route(
                "/dispatched-sessions/gc-threads",
                delete(dispatched_sessions::gc_thread_sessions),
            )
            .route(
                "/dispatched-sessions/{id}",
                patch(dispatched_sessions::update_dispatched_session),
            )
            .route(
                "/dispatched-sessions/webhook",
                post(dispatched_sessions::hook_session).delete(dispatched_sessions::delete_session),
            )
            .route("/hook/reset-status", post(hooks::reset_status))
            .route("/hook/skill-usage", post(hooks::skill_usage))
            .route(
                "/hook/session/{sessionKey}",
                delete(hooks::disconnect_session),
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
            // #1067: watch-agent-turn skill promotion — capture the last N lines
            // of the tmux pane bound to a session id.
            .route(
                "/sessions/{id}/tmux-output",
                get(dispatched_sessions::tmux_output),
            )
            .route(
                "/session-termination-events",
                get(termination_events::list_termination_events),
            )
            .route(
                "/messages",
                get(messages::list_messages).post(messages::create_message),
            )
            .route("/skills/catalog", get(skills_api::catalog))
            .route("/skills/ranking", get(skills_api::ranking))
            .route("/skills/prune", post(skills_api::prune))
            .route("/cron-jobs", get(cron_api::list_cron_jobs))
            .route("/maintenance/jobs", get(maintenance::list_jobs))
            // Canonical queue routes (#1065): /api/queue/*
            // Legacy /api/auto-queue/* still mounted (same handlers) for backward compat.
            .route("/queue/generate", post(auto_queue::generate))
            .route("/auto-queue/generate", post(auto_queue::generate))
            // #1064: /auto-queue/dispatch is deprecated; prefer /generate + /dispatch-next.
            // Kept as a functional deprecated alias (logs warning) for CLI callers
            // that still send the legacy `groups` body shape. TODO(#1064): remove
            // after migrating src/cli/client.rs::cmd_dispatch to /generate.
            // #1065: canonical /queue/dispatch routes straight through to the real handler.
            .route("/queue/dispatch", post(auto_queue::dispatch))
            .route("/auto-queue/dispatch", post(deprecated_auto_queue_dispatch))
            .route("/queue/dispatch-next", post(auto_queue::activate))
            .route("/auto-queue/dispatch-next", post(auto_queue::activate))
            .route("/queue/status", get(auto_queue::status))
            .route("/auto-queue/status", get(auto_queue::status))
            .route("/queue/history", get(auto_queue::history))
            .route("/auto-queue/history", get(auto_queue::history))
            .route("/queue/entries/{id}", patch(auto_queue::update_entry))
            .route("/auto-queue/entries/{id}", patch(auto_queue::update_entry))
            .route("/queue/runs/{id}/restore", post(auto_queue::restore_run))
            .route(
                "/auto-queue/runs/{id}/restore",
                post(auto_queue::restore_run),
            )
            .route("/queue/runs/{id}/entries", post(auto_queue::add_run_entry))
            .route(
                "/auto-queue/runs/{id}/entries",
                post(auto_queue::add_run_entry),
            )
            .route("/queue/entries/{id}/skip", patch(auto_queue::skip_entry))
            .route(
                "/auto-queue/entries/{id}/skip",
                patch(auto_queue::skip_entry),
            )
            .route("/queue/runs/{id}", patch(auto_queue::update_run))
            .route("/auto-queue/runs/{id}", patch(auto_queue::update_run))
            .route("/queue/reorder", patch(auto_queue::reorder))
            .route("/auto-queue/reorder", patch(auto_queue::reorder))
            .route(
                "/queue/slots/{agent_id}/{slot_index}/rebind",
                post(auto_queue::rebind_slot),
            )
            .route(
                "/auto-queue/slots/{agent_id}/{slot_index}/rebind",
                post(auto_queue::rebind_slot),
            )
            .route(
                "/queue/slots/{agent_id}/{slot_index}/reset-thread",
                post(auto_queue::reset_slot_thread),
            )
            .route(
                "/auto-queue/slots/{agent_id}/{slot_index}/reset-thread",
                post(auto_queue::reset_slot_thread),
            )
            .route("/queue/reset", post(auto_queue::reset))
            .route("/auto-queue/reset", post(auto_queue::reset))
            .route("/queue/reset-global", post(auto_queue::reset_global))
            .route("/auto-queue/reset-global", post(auto_queue::reset_global))
            .route("/queue/pause", post(auto_queue::pause))
            .route("/auto-queue/pause", post(auto_queue::pause))
            .route("/queue/resume", post(auto_queue::resume_run))
            .route("/auto-queue/resume", post(auto_queue::resume_run))
            .route("/queue/cancel", post(auto_queue::cancel))
            .route("/auto-queue/cancel", post(auto_queue::cancel))
            .route("/queue/runs/{id}/order", post(auto_queue::submit_order))
            .route(
                "/auto-queue/runs/{id}/order",
                post(auto_queue::submit_order),
            )
            .route("/channels/{id}/queue", get(queue_api::list_channel_queue))
            .route(
                "/channels/{id}/watcher-state",
                get(queue_api::get_watcher_state),
            )
            .route(
                "/dispatches/pending",
                get(queue_api::list_pending_dispatches),
            )
            .route("/dispatches/{id}/cancel", post(queue_api::cancel_dispatch))
            .route(
                "/dispatches/cancel-all",
                post(queue_api::cancel_all_dispatches),
            )
            .route("/turns/{channel_id}/cancel", post(queue_api::cancel_turn))
            .route(
                "/turns/{channel_id}/extend-timeout",
                post(queue_api::extend_turn_timeout),
            )
            .route("/help", get(docs::api_help))
            .route("/docs", get(docs::api_docs))
            .route("/docs/{segment}", get(docs::api_docs_group_or_category))
            .route(
                "/docs/{group}/{category}",
                get(docs::api_docs_group_category),
            )
            .route(
                "/provider-cli",
                get(provider_cli_api::get_provider_cli_status),
            )
            .route(
                "/provider-cli/{provider}",
                patch(provider_cli_api::patch_provider_cli),
            ),
        state,
    )
}

/// #1064: Deprecated alias for /api/auto-queue/generate (+ /dispatch-next when
/// the caller set `activate=true`). Kept functional until CLI callers migrate.
async fn deprecated_auto_queue_dispatch(
    State(state): State<AppState>,
    Json(body): Json<auto_queue::DispatchBody>,
) -> (StatusCode, Json<Value>) {
    log_deprecated_alias(
        "/api/auto-queue/dispatch",
        "/api/auto-queue/generate (+ /api/auto-queue/dispatch-next)",
    );
    auto_queue::dispatch(State(state), Json(body)).await
}
