use axum::{
    Router,
    routing::{get, patch, post},
};

use super::super::{
    ApiRouter, AppState, automation_candidates, kanban, kanban_repos, protected_api_domain, resume,
};

// Category: kanban

pub(crate) fn router(state: AppState) -> ApiRouter {
    protected_api_domain(
        Router::new()
            .route(
                "/kanban-cards",
                get(kanban::list_cards).post(kanban::create_card),
            )
            .route("/kanban-cards/stalled", get(kanban::stalled_cards))
            .route("/kanban-cards/assign-issue", post(kanban::assign_issue))
            .route(
                "/kanban-cards/{id}",
                get(kanban::get_card)
                    .patch(kanban::update_card)
                    .delete(kanban::delete_card),
            )
            .route("/kanban-cards/{id}/assign", post(kanban::assign_card))
            .route("/kanban-cards/{id}/rereview", post(kanban::rereview_card))
            .route("/kanban-cards/batch-rereview", post(kanban::batch_rereview))
            .route("/kanban-cards/{id}/reopen", post(kanban::reopen_card))
            .route(
                "/kanban-cards/{id}/transition",
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
            .route(
                "/kanban-repos",
                get(kanban_repos::list_repos).post(kanban_repos::create_repo),
            )
            .route(
                "/kanban-repos/{owner}/{repo}",
                patch(kanban_repos::update_repo).delete(kanban_repos::delete_repo),
            )
            .route("/pm-decision", post(kanban::pm_decision))
            .route(
                "/automation-candidates",
                post(automation_candidates::materialize_candidate),
            )
            .route(
                "/automation-candidates/{card_id}/iteration-result",
                post(automation_candidates::submit_iteration_result),
            )
            .route(
                "/automation-candidates/{card_id}/iterations",
                get(automation_candidates::list_iterations),
            )
            .route(
                "/automation-candidates/{card_id}/approve",
                post(automation_candidates::approve_candidate),
            )
            .route(
                "/automation-candidates/{card_id}/automation-inventory",
                get(automation_candidates::get_automation_inventory),
            )
            .route(
                "/automation-candidates/{card_id}/prepare-worktree",
                post(automation_candidates::prepare_worktree),
            ),
        state,
    )
}
