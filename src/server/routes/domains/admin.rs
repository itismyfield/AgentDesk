use axum::{
    Router,
    routing::{delete, get, patch, post},
};

use super::super::{
    ApiRouter, AppState, analytics, departments, escalation, offices, protected_api_domain,
    receipt, settings, stats,
};

pub(crate) fn router(state: AppState) -> ApiRouter {
    protected_api_domain(
        Router::new()
            .route(
                "/offices",
                get(offices::list_offices).post(offices::create_office),
            )
            .route("/offices/reorder", patch(offices::reorder_offices))
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
            .route("/stats", get(stats::get_stats))
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
            .route(
                "/settings/escalation",
                get(escalation::get_escalation_settings).put(escalation::put_escalation_settings),
            )
            .route(
                "/internal/escalation/emit",
                post(escalation::emit_escalation),
            )
            .route("/analytics", get(analytics::analytics))
            .route("/streaks", get(analytics::streaks))
            .route("/achievements", get(analytics::achievements))
            .route("/activity-heatmap", get(analytics::activity_heatmap))
            .route("/audit-logs", get(analytics::audit_logs))
            .route("/machine-status", get(analytics::machine_status))
            .route("/rate-limits", get(analytics::rate_limits))
            .route("/receipt", get(receipt::get_receipt))
            .route("/token-analytics", get(receipt::get_token_analytics))
            .route("/skills-trend", get(analytics::skills_trend)),
        state,
    )
}
