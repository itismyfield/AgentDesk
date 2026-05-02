use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
};
use serde_json::Value;

use super::AppState;
use crate::services::onboarding as service;

pub async fn status(state: State<AppState>) -> (StatusCode, Json<Value>) {
    service::status(&state).await
}

pub async fn draft_get(state: State<AppState>) -> (StatusCode, Json<Value>) {
    service::draft_get(&state).await
}

pub async fn draft_put(body: Json<service::OnboardingDraft>) -> (StatusCode, Json<Value>) {
    service::draft_put(body.0).await
}

pub async fn draft_delete() -> (StatusCode, Json<Value>) {
    service::draft_delete().await
}

pub async fn validate_token(body: Json<service::ValidateTokenBody>) -> (StatusCode, Json<Value>) {
    service::validate_token(body.0).await
}

pub async fn channels(
    state: State<AppState>,
    Query(query): Query<service::ChannelsQuery>,
) -> (StatusCode, Json<Value>) {
    service::channels(&state, query).await
}

pub async fn channels_post(
    state: State<AppState>,
    body: Json<service::ChannelsBody>,
) -> (StatusCode, Json<Value>) {
    service::channels_post(&state, body.0).await
}

pub async fn complete(
    state: State<AppState>,
    body: Json<service::CompleteBody>,
) -> (StatusCode, Json<Value>) {
    service::complete(&state, body.0).await
}

pub async fn check_provider(body: Json<service::CheckProviderBody>) -> (StatusCode, Json<Value>) {
    service::check_provider(body.0).await
}

pub async fn generate_prompt(body: Json<service::GeneratePromptBody>) -> (StatusCode, Json<Value>) {
    service::generate_prompt(body.0).await
}
