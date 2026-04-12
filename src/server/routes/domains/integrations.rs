use axum::{
    Router,
    routing::{get, patch, post},
};

use super::super::{
    ApiRouter, AppState, discord, dm_reply, github, github_dashboard, meetings,
    protected_api_domain,
};

pub(crate) fn router(state: AppState) -> ApiRouter {
    protected_api_domain(
        Router::new()
            .route(
                "/github/repos",
                get(github::list_repos).post(github::register_repo),
            )
            .route("/github/repos/{owner}/{repo}/sync", post(github::sync_repo))
            .route("/github-repos", get(github_dashboard::list_repos))
            .route("/github-issues", get(github_dashboard::list_issues))
            .route(
                "/github-issues/{owner}/{repo}/{number}/close",
                patch(github_dashboard::close_issue),
            )
            .route("/github-closed-today", get(github_dashboard::closed_today))
            .route("/discord-bindings", get(discord::list_bindings))
            .route(
                "/discord/channels/{id}/messages",
                get(discord::channel_messages),
            )
            .route("/discord/channels/{id}", get(discord::channel_info))
            .route("/dm-reply/register", post(dm_reply::register_handler))
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
            ),
        state,
    )
}
