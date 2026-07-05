use axum::{
    Router,
    routing::{delete, get, patch, post},
};

use super::super::{
    ApiRouter, AppState, claude_accounts_api, discord, dm_reply, github, github_dashboard, hooks,
    meetings, pr_summary, protected_api_domain,
};

// Category: integrations

pub(crate) fn router(state: AppState) -> ApiRouter {
    protected_api_domain(
        Router::new()
            .route(
                "/claude-accounts",
                get(claude_accounts_api::get_claude_accounts),
            )
            .route(
                "/claude-accounts/switch",
                post(claude_accounts_api::switch_claude_account),
            )
            .route("/github/issues/create", post(github::create_issue))
            .route(
                "/github/repos",
                get(github::list_repos).post(github::register_repo),
            )
            .route("/github/repos/{owner}/{repo}/sync", post(github::sync_repo))
            .route("/github/pr-summary", get(pr_summary::get_pr_summary))
            .route(
                "/github/pr-summary/invalidate",
                post(pr_summary::invalidate_pr_summary),
            )
            .route("/github-repos", get(github_dashboard::list_repos))
            .route("/github-issues", get(github_dashboard::list_issues))
            .route(
                "/github-issues/{owner}/{repo}/{number}/close",
                patch(github_dashboard::close_issue),
            )
            .route("/github-closed-today", get(github_dashboard::closed_today))
            .route("/discord/bindings", get(discord::list_bindings))
            .route(
                "/discord/channels/{id}/messages",
                get(discord::channel_messages),
            )
            .route("/discord/channels/{id}", get(discord::channel_info))
            .route("/dm-reply/register", post(dm_reply::register_handler))
            .route("/hook/reset-status", post(hooks::reset_status))
            .route("/hook/skill-usage", post(hooks::skill_usage))
            .route(
                "/hook/session/{sessionKey}",
                delete(hooks::disconnect_session),
            )
            .route(
                "/round-table-meetings",
                get(meetings::list_meetings).post(meetings::upsert_meeting),
            )
            .route(
                "/round-table-meetings/channels",
                get(meetings::list_meeting_channels),
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
