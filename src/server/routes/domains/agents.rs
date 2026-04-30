use axum::{
    Router,
    routing::{get, post},
};

use super::super::{
    ApiRouter, AppState, agents, agents_crud, agents_setup, cron_api, protected_api_domain,
};

// Category: agents

pub(crate) fn router(state: AppState) -> ApiRouter {
    protected_api_domain(
        Router::new()
            .route(
                "/agents",
                get(agents_crud::list_agents).post(agents_crud::create_agent),
            )
            .route("/agents/setup", post(agents_setup::setup_agent))
            .route(
                "/agents/quality/ranking",
                get(agents::agents_quality_ranking),
            )
            .route("/agents/diag/{identifier}", get(agents::agent_diag))
            .route(
                "/agents/{id}",
                get(agents_crud::get_agent)
                    .patch(agents_crud::update_agent)
                    .delete(agents_crud::delete_agent),
            )
            .route("/agents/{id}/quality", get(agents::agent_quality))
            .route("/agents/{id}/archive", post(agents_crud::archive_agent))
            .route("/agents/{id}/unarchive", post(agents_crud::unarchive_agent))
            .route("/agents/{id}/duplicate", post(agents_crud::duplicate_agent))
            .route("/agents/{id}/offices", get(agents::agent_offices))
            .route("/agents/{id}/signal", post(agents::agent_signal))
            .route("/agents/{id}/message", post(agents::agent_message))
            .route("/agents/{id}/cron", get(cron_api::agent_cron_jobs))
            .route("/agents/{id}/skills", get(agents::agent_skills))
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
            .route("/policies", get(agents_crud::list_policies)),
        state,
    )
}
