//! Voice conductor routes: text in, agent turns out, spoken summary back.

use axum::{
    Json,
    extract::{Path, State},
};
use serde::Deserialize;

use super::AppState;
use crate::error::{AppError, AppResult};
use crate::services::voice_conductor::{self, ConductorJob};

const LISTED_JOBS: usize = 20;

#[derive(Debug, Deserialize)]
pub(crate) struct SayBody {
    pub(crate) text: String,
}

/// POST /api/voice/conductor/say
pub(crate) async fn say(
    State(state): State<AppState>,
    Json(body): Json<SayBody>,
) -> AppResult<Json<ConductorJob>> {
    let text = body.text.trim();
    if text.is_empty() {
        return Err(AppError::bad_request("text is required"));
    }
    let pool = state
        .pg_pool
        .clone()
        .ok_or_else(|| AppError::internal("postgres pool unavailable"))?;
    let registry = state
        .health_registry
        .clone()
        .ok_or_else(|| AppError::internal("discord runtime health registry unavailable"))?;

    let start_turn = |agent_id: String, prompt: String| {
        let pool = pool.clone();
        let registry = registry.clone();
        async move {
            let target =
                super::agents_turn_target::resolve_agent_turn_target(&pool, &agent_id, None, None)
                    .await
                    .map_err(|(_, Json(body))| {
                        body.get("error")
                            .and_then(|error| error.as_str())
                            .unwrap_or("agent turn target unavailable")
                            .to_string()
                    })?;
            super::agents_turn_target::start_headless_turn_on_target(
                &registry,
                target,
                prompt,
                Some("voice_conductor".to_string()),
                None,
            )
            .await
            .map(|(turn_id, _)| turn_id)
            .map_err(|error| match error {
                crate::services::discord::HeadlessTurnStartError::Conflict(error)
                | crate::services::discord::HeadlessTurnStartError::Internal(error) => error,
            })
        }
    };

    let config = super::voice_config::live_voice_config(&state.config.voice);
    let job = voice_conductor::say(&pool, &config, text, start_turn)
        .await
        .map_err(AppError::internal)?;
    if job.finished_at.is_none() {
        tokio::spawn(voice_conductor::gather(
            pool,
            config,
            job.id.clone(),
            state.broadcast_tx.clone(),
        ));
    }
    Ok(Json(job))
}

/// GET /api/voice/conductor/jobs
pub(crate) async fn list_jobs() -> Json<Vec<ConductorJob>> {
    Json(voice_conductor::recent_jobs(LISTED_JOBS))
}

/// GET /api/voice/conductor/jobs/{id}
pub(crate) async fn get_job(Path(id): Path<String>) -> AppResult<Json<ConductorJob>> {
    voice_conductor::job(&id)
        .map(Json)
        .ok_or_else(|| AppError::not_found("voice conductor job not found"))
}
