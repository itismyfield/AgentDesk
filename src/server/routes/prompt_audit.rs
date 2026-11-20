use axum::{
    Json,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::IntoResponse,
};
use serde::Deserialize;

use crate::db::{
    prompt_manifests, provider_session_prompts, provider_session_prompts::SessionPrompt,
};
use crate::error::{AppError, AppResult, ErrorCode};
use crate::services::{
    discord::prompt_builder::audit, memory::backend_state, provider::ProviderKind,
};

use super::{AppState, agents_turn_target::resolve_agent_turn_target};

mod anchors;
mod projection;
mod redaction;
#[cfg(test)]
mod tests;

#[derive(Deserialize)]
pub(super) struct AuditQuery {
    provider: Option<String>,
}

async fn binding(
    state: &AppState,
    id: &str,
    params: AuditQuery,
) -> AppResult<(ProviderKind, u64, Option<String>)> {
    let target = resolve_agent_turn_target(
        pool(state)?,
        state.health_registry.as_deref(),
        id,
        params.provider.as_deref(),
        None,
    )
    .await
    .map_err(|(status, _)| AppError::new(status, ErrorCode::Config, "agent binding unavailable"))?;
    let provider = target.provider;
    if !matches!(provider, ProviderKind::Claude | ProviderKind::Codex) {
        return Err(AppError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            ErrorCode::Config,
            "prompt audit supports Claude and Codex",
        ));
    }
    let channel = target.channel_id;
    let current = if let Some(shared) = match state.health_registry.as_ref() {
        Some(registry) => {
            registry
                .shared_for_provider_on_channel(
                    &provider,
                    poise::serenity_prelude::ChannelId::new(channel),
                )
                .await
        }
        None => None,
    } {
        audit::current_session(&shared, &provider, channel).await
    } else {
        None
    };
    Ok((provider, channel, current))
}

fn pool(state: &AppState) -> AppResult<&sqlx::PgPool> {
    state.pg_pool_ref().ok_or_else(|| {
        AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Config,
            "postgres pool unavailable",
        )
    })
}

fn database_error() -> AppError {
    AppError::internal("prompt audit query failed").with_code(ErrorCode::Database)
}

async fn load_snapshot(
    pool: &sqlx::PgPool,
    provider: &ProviderKind,
    channel: u64,
    current: Option<&str>,
) -> AppResult<Option<SessionPrompt>> {
    let channel = channel.to_string();
    match current {
        Some(session) => {
            provider_session_prompts::fetch(pool, provider.as_str(), &channel, session).await
        }
        None => provider_session_prompts::fetch_latest(pool, provider.as_str(), &channel).await,
    }
    .map_err(|_| database_error())
}

pub(super) async fn prompt(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<AuditQuery>,
) -> AppResult<impl IntoResponse> {
    let (provider, channel, current) = binding(&state, &id, query).await?;
    let pool = pool(&state)?;
    let snapshot = load_snapshot(pool, &provider, channel, current.as_deref()).await?;
    let manifest = match &snapshot {
        Some(snapshot) => {
            prompt_manifests::fetch_prompt_manifest(Some(pool), &snapshot.source_turn_id)
                .await
                .map_err(|_| database_error())?
                .filter(|manifest| manifest.channel_id == snapshot.channel_id)
        }
        None => None,
    };
    let canon = audit::canon(&state.config, &id, &provider);
    let body = projection::prompt_response(
        &id,
        &provider,
        current,
        snapshot.as_ref(),
        manifest.as_ref(),
        &canon,
    );
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(body)))
}

pub(super) async fn anchors(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<AuditQuery>,
) -> AppResult<impl IntoResponse> {
    let (provider, channel, current) = binding(&state, &id, query).await?;
    let snapshot = load_snapshot(pool(&state)?, &provider, channel, current.as_deref()).await?;
    let health = backend_state(crate::domain::memory::MemoryBackendKind::Memento);
    let canon = audit::canon(&state.config, &id, &provider);
    let body = anchors::response(&id, &provider, current, snapshot.as_ref(), health, &canon);
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(body)))
}
