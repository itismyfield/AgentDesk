use axum::{
    Router,
    extract::DefaultBodyLimit,
    middleware::map_response,
    routing::{get, patch, post, put},
};

use super::super::{
    ApiRouter, AppState, campaigns, departments, escalation, home_metrics, protected_api_domain,
    server_campaigns, settings, stats,
};

// Category: admin

/// The ledger PUT carries the whole DAG, so only these routes raise the body limit.
fn campaign_router() -> ApiRouter {
    Router::new()
        .route("/campaigns", get(campaigns::list).post(campaigns::create))
        .route(
            "/campaigns/{id}",
            get(campaigns::get).put(campaigns::replace),
        )
        .route("/campaigns/{id}/history", get(campaigns::history))
        .layer(DefaultBodyLimit::max(campaigns::LEDGER_BODY_LIMIT_BYTES))
        .layer(map_response(campaigns::body_limit_envelope))
}

fn server_campaign_router() -> ApiRouter {
    Router::new()
        .route(
            "/server-campaigns",
            get(server_campaigns::list).post(server_campaigns::create),
        )
        .route("/server-campaigns/{id}", get(server_campaigns::detail))
        .route(
            "/server-campaigns/{id}/definition",
            put(server_campaigns::edit),
        )
        .route(
            "/server-campaigns/{id}/commands",
            post(server_campaigns::command),
        )
        .route(
            "/server-campaigns/{id}/events",
            get(server_campaigns::events),
        )
        .layer(DefaultBodyLimit::max(server_campaigns::BODY_LIMIT_BYTES))
}

pub(crate) fn router(state: AppState) -> ApiRouter {
    protected_api_domain(
        Router::new()
            .merge(campaign_router())
            .merge(server_campaign_router())
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
                "/internal/escalation/emit",
                post(escalation::emit_escalation),
            )
            .route("/home/kpi-trends", get(home_metrics::home_kpi_trends)),
        state,
    )
}
