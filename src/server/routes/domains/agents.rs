use axum::{
    Router,
    routing::{get, post},
};

use super::super::{ApiRouter, AppState, agents, agents_crud, cron_api, protected_api_domain};

pub(crate) fn router(state: AppState) -> ApiRouter {
    protected_api_domain(
        Router::new()
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
            .route("/agents/{id}/offices", get(agents::agent_offices))
            .route("/agents/{id}/signal", post(agents::agent_signal))
            .route("/agents/{id}/cron", get(cron_api::agent_cron_jobs))
            .route("/agents/{id}/skills", get(agents::agent_skills))
            .route(
                "/agents/{id}/dispatched-sessions",
                get(agents::agent_dispatched_sessions),
            )
            .route("/agents/{id}/turn", get(agents::agent_turn))
            .route("/agents/{id}/turn/stop", post(agents::stop_agent_turn))
            .route("/agents/{id}/timeline", get(agents::agent_timeline))
            .route("/sessions", get(agents_crud::list_sessions))
            .route("/policies", get(agents_crud::list_policies)),
        state,
    )
}
