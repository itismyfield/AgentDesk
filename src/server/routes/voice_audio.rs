//! Browser voice I/O: a recorded utterance in, its transcript out; text in,
//! spoken mp3 out. Uses whichever STT/TTS provider `voice` config selects.

use axum::{Json, extract::State};
use base64::Engine;
use serde::{Deserialize, Serialize};

use super::AppState;
use crate::error::{AppError, AppResult};
use crate::voice::stt::SttRuntime;
use crate::voice::tts::{ProgressTtsCacheStatus, TtsRuntime, TtsSynthesisKind};
use crate::voice::utils::expand_tilde;

const BASE64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

#[derive(Debug, Deserialize)]
pub(crate) struct TranscribeBody {
    audio_base64: String,
    #[serde(default)]
    mime: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct TranscribeResponse {
    text: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct SpeakBody {
    text: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct SpeakResponse {
    audio_base64: String,
    mime: &'static str,
}

/// POST /api/voice/transcribe
pub(crate) async fn transcribe(
    State(state): State<AppState>,
    Json(body): Json<TranscribeBody>,
) -> AppResult<Json<TranscribeResponse>> {
    let audio = BASE64
        .decode(body.audio_base64.trim())
        .map_err(|error| AppError::bad_request(format!("audio_base64: {error}")))?;
    if audio.is_empty() {
        return Err(AppError::bad_request("audio is empty"));
    }
    let config = super::voice_config::live_voice_config(&state.config.voice);
    let temp_dir = expand_tilde(&config.audio.temp_dir);
    tokio::fs::create_dir_all(&temp_dir)
        .await
        .map_err(|error| AppError::internal(format!("create voice temp dir: {error}")))?;
    let path = temp_dir.join(format!(
        "agentdesk-web-utterance-{}.{}",
        uuid::Uuid::new_v4(),
        extension_for_mime(&body.mime)
    ));
    tokio::fs::write(&path, &audio)
        .await
        .map_err(|error| AppError::internal(format!("write utterance: {error}")))?;

    let result = SttRuntime::from_voice_config(&config)
        .transcribe(&path)
        .await;
    let _ = tokio::fs::remove_file(&path).await;
    let text = result.map_err(|error| AppError::internal(format!("transcribe: {error:#}")))?;
    Ok(Json(TranscribeResponse { text }))
}

/// POST /api/voice/speak
pub(crate) async fn speak(
    State(state): State<AppState>,
    Json(body): Json<SpeakBody>,
) -> AppResult<Json<SpeakResponse>> {
    let text = body.text.trim();
    if text.is_empty() {
        return Err(AppError::bad_request("text is required"));
    }
    let config = super::voice_config::live_voice_config(&state.config.voice);
    let output = TtsRuntime::from_voice_config(&config)
        .map_err(|error| AppError::internal(format!("tts backend: {error:#}")))?
        .synthesize(text, TtsSynthesisKind::Final)
        .await
        .map_err(|error| AppError::internal(format!("synthesize: {error:#}")))?;
    let audio = tokio::fs::read(&output.path).await;
    if output.cache_status == ProgressTtsCacheStatus::Bypassed {
        let _ = tokio::fs::remove_file(&output.path).await;
    }
    let audio = audio.map_err(|error| AppError::internal(format!("read speech: {error}")))?;
    Ok(Json(SpeakResponse {
        audio_base64: BASE64.encode(audio),
        mime: "audio/mpeg",
    }))
}

fn extension_for_mime(mime: &str) -> &'static str {
    let mime = mime.to_ascii_lowercase();
    if mime.contains("ogg") {
        "ogg"
    } else if mime.contains("mp4") || mime.contains("m4a") || mime.contains("aac") {
        "m4a"
    } else if mime.contains("wav") {
        "wav"
    } else {
        "webm"
    }
}
