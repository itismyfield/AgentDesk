use axum::{
    Router,
    routing::{delete, get, patch, post},
};

use super::super::{
    ApiRouter, AppState, auto_queue, cron_api, dispatched_sessions, dispatches, docs, messages,
    pipeline, protected_api_domain, queue_api, skills_api, termination_events,
};

pub(crate) fn router(state: AppState) -> ApiRouter {
    protected_api_domain(
        Router::new()
            .route(
                "/dispatches",
                get(dispatches::list_dispatches).post(dispatches::create_dispatch),
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
            .route("/pipeline/cards/{cardId}", get(pipeline::get_card_pipeline))
            .route(
                "/pipeline/cards/{cardId}/history",
                get(pipeline::get_card_history),
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
                "/hook/session",
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
                "/session-termination-events",
                get(termination_events::list_termination_events),
            )
            .route(
                "/messages",
                get(messages::list_messages).post(messages::create_message),
            )
            .route("/skills/catalog", get(skills_api::catalog))
            .route("/skills/ranking", get(skills_api::ranking))
            .route("/cron-jobs", get(cron_api::list_cron_jobs))
            .route("/auto-queue/generate", post(auto_queue::generate))
            .route("/auto-queue/dispatch", post(auto_queue::dispatch))
            .route("/auto-queue/activate", post(auto_queue::activate))
            .route("/auto-queue/status", get(auto_queue::status))
            .route("/auto-queue/entries/{id}", patch(auto_queue::update_entry))
            .route(
                "/auto-queue/entries/{id}/skip",
                patch(auto_queue::skip_entry),
            )
            .route("/auto-queue/runs/{id}", patch(auto_queue::update_run))
            .route("/auto-queue/reorder", patch(auto_queue::reorder))
            .route(
                "/auto-queue/slots/{agent_id}/{slot_index}/reset-thread",
                post(auto_queue::reset_slot_thread),
            )
            .route("/auto-queue/reset", post(auto_queue::reset))
            .route("/auto-queue/pause", post(auto_queue::pause))
            .route("/auto-queue/resume", post(auto_queue::resume_run))
            .route("/auto-queue/cancel", post(auto_queue::cancel))
            .route(
                "/auto-queue/runs/{id}/order",
                post(auto_queue::submit_order),
            )
            .route("/channels/{id}/queue", get(queue_api::list_channel_queue))
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
            .route("/docs/{category}", get(docs::api_docs_category)),
        state,
    )
}
