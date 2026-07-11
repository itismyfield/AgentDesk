use axum::{
    Router,
    routing::{delete, get, patch, post},
};

use super::super::{
    ApiRouter, AppState, analytics, departments, escalation, home_metrics, offices,
    protected_api_domain, settings, stats, voice_config,
};

// Category: admin

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
            .route("/stats/memento", get(stats::get_memento_stats))
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
                "/settings/operator-connectors",
                get(settings::get_operator_connectors),
            )
            .route(
                "/settings/escalation",
                get(escalation::get_escalation_settings).put(escalation::put_escalation_settings),
            )
            .route(
                "/voice/config",
                get(voice_config::get_voice_config).put(voice_config::put_voice_config),
            )
            .route(
                "/internal/escalation/emit",
                post(escalation::emit_escalation),
            )
            .route("/audit-logs", get(analytics::audit_logs))
            .route("/machine-status", get(analytics::machine_status))
            .route("/rate-limits", get(analytics::rate_limits))
            .route("/home/kpi-trends", get(home_metrics::home_kpi_trends))
            .route("/skills-trend", get(analytics::skills_trend)),
        state,
    )
}
