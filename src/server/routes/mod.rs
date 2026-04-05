pub mod agents;
mod agents_crud;
pub mod analytics;
pub mod auth;
pub mod auto_queue;
pub mod cron_api;
pub mod departments;
pub mod discord;
pub mod dispatched_sessions;
pub mod dispatches;
pub mod dm_reply;
pub mod docs;
pub mod github;
pub mod github_dashboard;
pub mod health_api;
pub mod kanban;
pub mod kanban_repos;
pub mod meetings;
pub mod messages;
pub mod offices;
pub mod onboarding;
pub mod pipeline;
mod queue_api;
pub mod receipt;
pub mod resume;
pub mod review_verdict;
pub mod reviews;
mod session_activity;
pub mod settings;
pub mod skills_api;
pub mod stats;
pub mod termination_events;

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{delete, get, patch, post},
};
use serde::Deserialize;
use serde_json::json;

use std::sync::Arc;

use crate::db::Db;
use crate::engine::PolicyEngine;
use crate::services::discord::health::HealthRegistry;

/// Shared application state passed to all route handlers.
#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub engine: PolicyEngine,
    pub broadcast_tx: crate::server::ws::BroadcastTx,
    pub batch_buffer: crate::server::ws::BatchBuffer,
    pub health_registry: Option<Arc<HealthRegistry>>,
}

#[cfg(test)]
impl AppState {
    pub fn test_state(db: Db, engine: PolicyEngine) -> Self {
        let tx = crate::server::ws::new_broadcast();
        let buf = crate::server::ws::spawn_batch_flusher(tx.clone());
        Self {
            db,
            engine,
            broadcast_tx: tx,
            batch_buffer: buf,
            health_registry: None,
        }
    }
}

pub fn api_router(
    db: Db,
    engine: PolicyEngine,
    broadcast_tx: crate::server::ws::BroadcastTx,
    batch_buffer: crate::server::ws::BatchBuffer,
    health_registry: Option<Arc<HealthRegistry>>,
) -> Router {
    let state = AppState {
        db,
        engine,
        broadcast_tx,
        batch_buffer,
        health_registry,
    };

    Router::new()
        .route("/health", get(health_api::health_handler))
        .route("/send", post(health_api::send_handler))
        .route("/senddm", post(health_api::senddm_handler))
        .route("/session/start", post(health_api::session_start_handler))
        .route(
            "/agents",
            get(agents_crud::list_agents).post(agents_crud::create_agent),
        )
        .route(
            "/agents/{id}",
            get(agents_crud::get_agent)
                .patch(agents_crud::update_agent)
                .delete(agents_crud::delete_agent),
        )
        // Onboarding
        .route("/onboarding/status", get(onboarding::status))
        .route(
            "/onboarding/validate-token",
            post(onboarding::validate_token),
        )
        .route(
            "/onboarding/channels",
            get(onboarding::channels).post(onboarding::channels_post),
        )
        .route("/onboarding/complete", post(onboarding::complete))
        .route(
            "/onboarding/check-provider",
            post(onboarding::check_provider),
        )
        .route(
            "/onboarding/generate-prompt",
            post(onboarding::generate_prompt),
        )
        .route("/agent-channels", get(agents::agent_channels))
        .route("/agents/{id}/offices", get(agents::agent_offices))
        .route("/agents/{id}/signal", post(agents::agent_signal))
        .route("/agents/{id}/cron", get(cron_api::agent_cron_jobs))
        .route("/agents/{id}/skills", get(agents::agent_skills))
        .route(
            "/agents/{id}/dispatched-sessions",
            get(agents::agent_dispatched_sessions),
        )
        .route("/agents/{id}/timeline", get(agents::agent_timeline))
        .route("/sessions", get(agents_crud::list_sessions))
        .route("/policies", get(agents_crud::list_policies))
        // Auth
        .route("/auth/session", get(auth::get_session))
        // Kanban
        .route(
            "/kanban-cards",
            get(kanban::list_cards).post(kanban::create_card),
        )
        .route("/kanban-cards/stalled", get(kanban::stalled_cards))
        .route("/kanban-cards/bulk-action", post(kanban::bulk_action))
        .route("/kanban-cards/assign-issue", post(kanban::assign_issue))
        .route(
            "/kanban-cards/{id}",
            get(kanban::get_card)
                .patch(kanban::update_card)
                .delete(kanban::delete_card),
        )
        .route("/kanban-cards/{id}/assign", post(kanban::assign_card))
        .route("/kanban-cards/{id}/rereview", post(kanban::rereview_card))
        .route("/re-review", post(kanban::batch_rereview))
        .route("/kanban-cards/{id}/reopen", post(kanban::reopen_card))
        .route(
            "/kanban-cards/{id}/force-transition",
            post(kanban::force_transition),
        )
        .route("/kanban-cards/{id}/retry", post(kanban::retry_card))
        .route(
            "/kanban-cards/{id}/redispatch",
            post(kanban::redispatch_card),
        )
        .route("/kanban-cards/{id}/resume", post(resume::resume_card))
        .route("/kanban-cards/{id}/defer-dod", patch(kanban::defer_dod))
        .route("/kanban-cards/{id}/reviews", get(kanban::list_card_reviews))
        .route(
            "/kanban-cards/{id}/review-state",
            get(kanban::get_card_review_state),
        )
        .route("/kanban-cards/{id}/audit-log", get(kanban::card_audit_log))
        .route(
            "/kanban-cards/{id}/comments",
            get(kanban::card_github_comments),
        )
        // Kanban repos
        .route(
            "/kanban-repos",
            get(kanban_repos::list_repos).post(kanban_repos::create_repo),
        )
        .route(
            "/kanban-repos/{owner}/{repo}",
            patch(kanban_repos::update_repo).delete(kanban_repos::delete_repo),
        )
        // Reviews
        .route(
            "/kanban-reviews/{id}/decisions",
            patch(reviews::update_decisions),
        )
        .route(
            "/kanban-reviews/{id}/trigger-rework",
            post(reviews::trigger_rework),
        )
        // Dispatches
        .route(
            "/dispatches",
            get(dispatches::list_dispatches).post(dispatches::create_dispatch),
        )
        .route(
            "/dispatches/{id}",
            get(dispatches::get_dispatch).patch(dispatches::update_dispatch),
        )
        .route("/dispatch-cancel/{id}", post(dispatches::cancel_dispatch))
        .route(
            "/internal/link-dispatch-thread",
            post(dispatches::link_dispatch_thread),
        )
        .route("/internal/card-thread", get(dispatches::get_card_thread))
        .route(
            "/internal/pending-dispatch-for-thread",
            get(dispatches::get_pending_dispatch_for_thread),
        )
        // Pipeline stages (legacy path)
        .route(
            "/pipeline-stages",
            get(pipeline::list_stages).post(pipeline::create_stage),
        )
        .route("/pipeline-stages/{id}", delete(pipeline::delete_stage))
        // Pipeline stages (dashboard v2 path)
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
        // Pipeline config hierarchy (#135)
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
        // GitHub repos
        .route(
            "/github/repos",
            get(github::list_repos).post(github::register_repo),
        )
        .route("/github/repos/{owner}/{repo}/sync", post(github::sync_repo))
        // GitHub dashboard
        .route("/github-repos", get(github_dashboard::list_repos))
        .route("/github-issues", get(github_dashboard::list_issues))
        .route(
            "/github-issues/{owner}/{repo}/{number}/close",
            patch(github_dashboard::close_issue),
        )
        .route("/github-closed-today", get(github_dashboard::closed_today))
        // Offices
        .route(
            "/offices",
            get(offices::list_offices).post(offices::create_office),
        )
        .route(
            "/offices/{id}",
            patch(offices::update_office).delete(offices::delete_office),
        )
        .route("/offices/{id}/agents", post(offices::add_agent))
        .route(
            "/offices/{id}/agents/batch",
            post(offices::batch_add_agents),
        )
        .route(
            "/offices/{id}/agents/{agentId}",
            delete(offices::remove_agent).patch(offices::update_office_agent),
        )
        // Departments
        .route(
            "/departments",
            get(departments::list_departments).post(departments::create_department),
        )
        .route(
            "/departments/reorder",
            patch(departments::reorder_departments),
        )
        .route(
            "/departments/{id}",
            patch(departments::update_department).delete(departments::delete_department),
        )
        // Stats
        .route("/stats", get(stats::get_stats))
        // Settings
        .route(
            "/settings",
            get(settings::get_settings).put(settings::put_settings),
        )
        .route(
            "/settings/config",
            get(settings::get_config_entries).patch(settings::patch_config_entries),
        )
        .route(
            "/settings/runtime-config",
            get(settings::get_runtime_config).put(settings::put_runtime_config),
        )
        // Dispatched sessions
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
            "/sessions/force-kill",
            post(dispatched_sessions::force_kill_session),
        )
        // Session termination events (#212)
        .route(
            "/session-termination-events",
            get(termination_events::list_termination_events),
        )
        // Messages
        .route(
            "/messages",
            get(messages::list_messages).post(messages::create_message),
        )
        // Discord bindings
        .route("/discord-bindings", get(discord::list_bindings))
        .route(
            "/discord/channels/{id}/messages",
            get(discord::channel_messages),
        )
        .route("/discord/channels/{id}", get(discord::channel_info))
        // DM reply tracking (#189)
        .route("/dm-reply/register", post(dm_reply::register_handler))
        // Round-table meetings
        .route(
            "/round-table-meetings",
            get(meetings::list_meetings).post(meetings::upsert_meeting),
        )
        .route("/round-table-meetings/start", post(meetings::start_meeting))
        .route(
            "/round-table-meetings/{id}",
            get(meetings::get_meeting).delete(meetings::delete_meeting),
        )
        .route(
            "/round-table-meetings/{id}/issue-repo",
            patch(meetings::update_issue_repo),
        )
        .route(
            "/round-table-meetings/{id}/issues",
            post(meetings::create_issues),
        )
        .route(
            "/round-table-meetings/{id}/issues/discard",
            post(meetings::discard_issue),
        )
        .route(
            "/round-table-meetings/{id}/issues/discard-all",
            post(meetings::discard_all_issues),
        )
        // Skills API
        .route("/skills/catalog", get(skills_api::catalog))
        .route("/skills/ranking", get(skills_api::ranking))
        // Cron jobs (stub)
        .route("/cron-jobs", get(cron_api::list_cron_jobs))
        // Auto-queue
        .route("/auto-queue/generate", post(auto_queue::generate))
        .route("/auto-queue/activate", post(auto_queue::activate))
        .route("/auto-queue/status", get(auto_queue::status))
        .route(
            "/auto-queue/entries/{id}/skip",
            patch(auto_queue::skip_entry),
        )
        .route("/auto-queue/runs/{id}", patch(auto_queue::update_run))
        .route("/auto-queue/reorder", patch(auto_queue::reorder))
        .route("/auto-queue/reset", post(auto_queue::reset))
        .route("/auto-queue/pause", post(auto_queue::pause))
        .route("/auto-queue/resume", post(auto_queue::resume_run))
        .route("/auto-queue/cancel", post(auto_queue::cancel))
        // Queue management (#138)
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
        .route(
            "/auto-queue/runs/{id}/order",
            post(auto_queue::submit_order),
        )
        .route("/auto-queue/enqueue", post(auto_queue::enqueue))
        // Analytics
        .route("/streaks", get(analytics::streaks))
        .route("/achievements", get(analytics::achievements))
        .route("/activity-heatmap", get(analytics::activity_heatmap))
        .route("/audit-logs", get(analytics::audit_logs))
        .route("/machine-status", get(analytics::machine_status))
        .route("/rate-limits", get(analytics::rate_limits))
        .route("/receipt", get(receipt::get_receipt))
        .route("/skills-trend", get(analytics::skills_trend))
        // Docs
        .route("/docs", get(docs::api_docs))
        // Review verdict
        .route("/review-verdict", post(review_verdict::submit_verdict))
        .route(
            "/review-decision",
            post(review_verdict::submit_review_decision),
        )
        // #119: Review tuning aggregation
        .route(
            "/review-tuning/aggregate",
            post(review_verdict::aggregate_review_tuning),
        )
        .route("/pm-decision", post(kanban::pm_decision))
        .layer(axum::middleware::from_fn(auth::auth_middleware))
        .with_state(state)
}

#[cfg(test)]
mod routes_tests;
