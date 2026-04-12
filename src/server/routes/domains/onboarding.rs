use axum::{
    Router,
    routing::{get, post},
};

use super::super::{ApiRouter, AppState, onboarding, protected_api_domain};

pub(crate) fn router(state: AppState) -> ApiRouter {
    protected_api_domain(
        Router::new()
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
            ),
        state,
    )
}
