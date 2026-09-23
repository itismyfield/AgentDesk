use axum::{
    Router,
    routing::{delete, get, patch, post, put},
};

use super::super::{
    ApiRouter, AppState, claude_accounts_api, discord, github, github_dashboard, kakao_calendar,
    meetings, pr_summary, protected_api_domain, provider_auth_profiles,
};

// Category: integrations

pub(crate) fn router(state: AppState) -> ApiRouter {
    protected_api_domain(
        Router::new()
            .route("/kakao/calendar/accounts", get(kakao_calendar::accounts))
            .route(
                "/kakao/calendar/accounts/{accountId}/check",
                post(kakao_calendar::check_account),
            )
            .route(
                "/kakao/calendar/events",
                get(kakao_calendar::list)
                    .post(kakao_calendar::create)
                    .layer(axum::extract::DefaultBodyLimit::max(32 * 1024)),
            )
            .route(
                "/kakao/calendar/events/{eventId}",
                get(kakao_calendar::get)
                    .patch(kakao_calendar::patch)
                    .delete(kakao_calendar::delete)
                    .layer(axum::extract::DefaultBodyLimit::max(32 * 1024)),
            )
            .route(
                "/kakao/calendar/events/{eventId}/operations",
                get(kakao_calendar::operations),
            )
            .route(
                "/kakao/calendar/events/{eventId}/operations/{operationId}/recover",
                post(kakao_calendar::recover).layer(axum::extract::DefaultBodyLimit::max(8 * 1024)),
            )
            .route(
                "/claude-accounts",
                get(claude_accounts_api::get_claude_accounts),
            )
            .route(
                "/claude-accounts/switch",
                post(claude_accounts_api::switch_claude_account),
            )
            .route(
                "/provider-auth-profiles",
                get(provider_auth_profiles::list_provider_auth_profiles),
            )
            .route(
                "/provider-auth-profiles/{provider}/login-start",
                post(provider_auth_profiles::login_start),
            )
            .route(
                "/provider-auth-profiles/{provider}/login-complete",
                post(provider_auth_profiles::login_complete),
            )
            .route(
                "/provider-auth-profiles/{provider}/primary",
                put(provider_auth_profiles::set_primary_profile),
            )
            .route(
                "/provider-auth-profiles/{provider}/{profile_id}",
                delete(provider_auth_profiles::remove_profile),
            )
            .route(
                "/channels/{id}",
                patch(provider_auth_profiles::patch_channel_auth_profile),
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
