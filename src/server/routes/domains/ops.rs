use axum::{
    Router,
    routing::{delete, get, patch, post},
};

use super::super::{
    ApiRouter, AppState, auto_queue, cluster, cron_api, dispatched_sessions, dispatches, docs,
    e2e_control, health_api, maintenance, message_outbox, messages, pipeline,
    prompt_manifest_retention, protected_api_domain, provider_cli_api, queue_api, routines,
    scheduled_messages, skills_api,
};

// Category: dispatches, queue, and ops

pub(crate) fn router(state: AppState) -> ApiRouter {
    let router = protected_api_domain(
        Router::new()
            .route(
                "/dispatches",
                get(dispatches::list_dispatches).post(dispatches::create_dispatch),
            )
            .route(
                "/dispatch-outbox/failed",
                get(health_api::list_dispatch_outbox_failures_handler)
                    .post(health_api::ack_dispatch_outbox_failures_handler),
            )
            .route("/message-outbox/failed", get(message_outbox::list_failed))
            .route(
                "/message-outbox/monitor-alerts",
                post(message_outbox::enqueue_monitor_alert),
            )
            .route(
                "/message-outbox/failed/redrive",
                post(message_outbox::redrive_failed),
            )
            .route("/discord/send", post(health_api::send_handler))
            .route(
                "/discord/bot-tokens/reload",
                post(health_api::reload_discord_bot_tokens_handler),
            )
            // Destructive E2E routes are conditionally mounted in the enabled branch below.
            .route(
                "/discord/send-to-agent",
                post(health_api::send_to_agent_handler),
            )
            .route("/discord/send-dm", post(health_api::senddm_handler))
            .route("/cluster/resource-locks", get(cluster::list_resource_locks))
            .route(
                "/cluster/resource-locks/acquire",
                post(cluster::acquire_resource_lock),
            )
            .route(
                "/cluster/resource-locks/heartbeat",
                post(cluster::heartbeat_resource_lock),
            )
            .route(
                "/cluster/resource-locks/release",
                post(cluster::release_resource_lock),
            )
            .route(
                "/cluster/resource-locks/reclaim-expired",
                post(cluster::reclaim_expired_resource_locks),
            )
            .route(
                "/cluster/test-phase-runs",
                get(cluster::list_test_phase_runs),
            )
            .route(
                "/cluster/test-phase-runs/upsert",
                post(cluster::upsert_test_phase_run),
            )
            .route(
                "/cluster/test-phase-runs/start",
                post(cluster::start_test_phase_run),
            )
            .route(
                "/cluster/test-phase-runs/complete",
                post(cluster::complete_test_phase_run),
            )
            .route(
                "/cluster/test-phase-runs/evidence",
                get(cluster::latest_test_phase_evidence),
            )
            .route(
                "/cluster/task-dispatches/claim",
                post(cluster::claim_task_dispatches),
            )
            .route("/cluster/issue-specs", get(cluster::list_issue_specs))
            .route(
                "/cluster/issue-specs/upsert",
                post(cluster::upsert_issue_spec),
            )
            .route(
                "/dispatches/delivery-events/reconcile-stats",
                get(dispatches::get_dispatch_delivery_reconcile_stats),
            )
            .route(
                "/dispatches/{id}",
                get(dispatches::get_dispatch).patch(dispatches::update_dispatch),
            )
            .route(
                "/dispatches/{id}/events",
                get(dispatches::get_dispatch_delivery_events),
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
                "/dispatched-sessions/cleanup",
                delete(dispatched_sessions::cleanup_sessions),
            )
            .route(
                "/dispatched-sessions/gc-threads",
                delete(dispatched_sessions::gc_thread_sessions),
            )
            // #1067: watch-agent-turn skill promotion — capture the last N lines
            // of the tmux pane bound to a session id.
            .route(
                "/messages",
                get(messages::list_messages).post(messages::create_message),
            )
            .route("/skills/catalog", get(skills_api::catalog))
            .route("/skills/ranking", get(skills_api::ranking))
            .route("/skills/prune", post(skills_api::prune))
            .route("/cron-jobs", get(cron_api::list_cron_jobs))
            .route("/maintenance/jobs", get(maintenance::list_jobs))
            .route(
                "/prompt-manifest/retention",
                get(prompt_manifest_retention::get_retention_status),
            )
            .route(
                "/routines",
                get(routines::list_routines).post(routines::attach_routine),
            )
            .route("/routines/metrics", get(routines::routine_metrics))
            .route(
                "/routines/runs/search",
                get(routines::search_routine_run_results),
            )
            .route(
                "/routines/{id}",
                get(routines::get_routine)
                    .patch(routines::patch_routine)
                    .delete(routines::delete_routine),
            )
            .route("/routines/{id}/runs", get(routines::list_routine_runs))
            .route("/routines/{id}/pause", post(routines::pause_routine))
            .route("/routines/{id}/resume", post(routines::resume_routine))
            .route("/routines/{id}/detach", post(routines::detach_routine))
            .route("/routines/{id}/run-now", post(routines::run_routine_now))
            .route(
                "/routines/{id}/session/reset",
                post(routines::reset_routine_session),
            )
            .route(
                "/routines/{id}/session/kill",
                post(routines::kill_routine_session),
            )
            .route(
                "/scheduled-messages",
                get(scheduled_messages::list_scheduled_messages)
                    .post(scheduled_messages::create_scheduled_message),
            )
            .route(
                "/scheduled-messages/{id}",
                get(scheduled_messages::get_scheduled_message)
                    .patch(scheduled_messages::patch_scheduled_message)
                    .delete(scheduled_messages::cancel_scheduled_message),
            )
            .route(
                "/scheduled-messages/{id}/trigger-now",
                post(scheduled_messages::trigger_scheduled_message_now),
            )
            .route(
                "/scheduled-messages/{id}/deliveries",
                get(scheduled_messages::list_scheduled_message_deliveries),
            )
            .route("/queue/generate", post(auto_queue::generate))
            .route(
                "/queue/request-generate",
                post(auto_queue::request_generate),
            )
            .route(
                "/queue/phase-gates/catalog",
                get(auto_queue::phase_gate_catalog),
            )
            .route(
                "/queue/phase-gates/violations",
                get(auto_queue::phase_gate_violations),
            )
            .route("/queue/dispatch-next", post(auto_queue::activate))
            .route("/queue/status", get(auto_queue::status))
            .route("/queue/history", get(auto_queue::history))
            .route("/queue/entries/{id}", patch(auto_queue::update_entry))
            .route("/queue/runs/{id}/restore", post(auto_queue::restore_run))
            .route(
                "/queue/runs/{id}/phase-gates/repair",
                post(auto_queue::repair_phase_gates),
            )
            .route("/queue/runs/{id}/entries", post(auto_queue::add_run_entry))
            .route("/queue/entries/{id}/skip", patch(auto_queue::skip_entry))
            .route("/queue/runs/{id}", patch(auto_queue::update_run))
            .route("/queue/runs/{id}/pause", post(auto_queue::pause_run))
            .route(
                "/queue/runs/{id}/resume",
                post(auto_queue::resume_run_scoped),
            )
            .route("/queue/runs/{id}/end", post(auto_queue::end_run))
            .route("/queue/reorder", patch(auto_queue::reorder))
            .route(
                "/queue/slots/{agent_id}/{slot_index}/rebind",
                post(auto_queue::rebind_slot),
            )
            .route(
                "/queue/slots/{agent_id}/{slot_index}/reset-thread",
                post(auto_queue::reset_slot_thread),
            )
            .route("/queue/reset", post(auto_queue::reset))
            .route("/queue/reset-global", post(auto_queue::reset_global))
            .route("/queue/pause", post(auto_queue::pause))
            .route("/queue/resume", post(auto_queue::resume_run))
            .route("/queue/cancel", post(auto_queue::cancel))
            .route("/queue/runs/{id}/order", post(auto_queue::submit_order))
            .route(
                "/dispatches/pending",
                get(queue_api::list_pending_dispatches),
            )
            .route("/dispatches/{id}/cancel", post(queue_api::cancel_dispatch))
            .route(
                "/dispatches/cancel-all",
                post(queue_api::cancel_all_dispatches),
            )
            .route("/help", get(docs::api_help))
            .route("/docs", get(docs::api_docs))
            .route("/docs/{segment}", get(docs::api_docs_group_or_category))
            .route(
                "/docs/{group}/{category}",
                get(docs::api_docs_group_category),
            )
            .route(
                "/provider-cli/{provider}",
                patch(provider_cli_api::patch_provider_cli),
            ),
        state.clone(),
    );
    // Keep this branch below the ordinary route chain so disabled boots expose no
    // method router or body extractor for any /e2e/discord path.
    if crate::services::discord::e2e_control::enabled() {
        crate::services::discord::e2e_control::sweep_expired();
        router.merge(protected_api_domain(
            Router::new()
                .route(
                    "/e2e/discord/channels/{channel_id}/messages/{message_id}",
                    delete(e2e_control::delete_discord_message),
                )
                .route(
                    "/e2e/discord/failures",
                    post(e2e_control::inject_discord_failure)
                        .delete(e2e_control::clear_discord_failure),
                ),
            state,
        ))
    } else {
        router
    }
}
