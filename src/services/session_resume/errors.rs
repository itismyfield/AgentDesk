use axum::{Json, http::StatusCode};
use serde_json::json;

use super::ResumeRebindError;

impl ResumeRebindError {
    pub(crate) fn into_response(self) -> (StatusCode, Json<serde_json::Value>) {
        match self {
            ResumeRebindError::SessionNotFound => (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "session not found"})),
            ),
            ResumeRebindError::ActiveTurn => (
                StatusCode::CONFLICT,
                Json(json!({
                    "error": "channel has an active turn or dispatch; stop it before resuming a previous session"
                })),
            ),
            ResumeRebindError::TransitionBusy => (
                StatusCode::CONFLICT,
                Json(json!({
                    "error": format!(
                        "채널 세션 전환이 {}초 동안 계속 사용 중입니다. 잠시 후 /resume을 다시 시도해 주세요.",
                        crate::services::discord::SESSION_TRANSITION_LOCK_WAIT_TIMEOUT.as_secs()
                    )
                })),
            ),
            ResumeRebindError::NoPreviousSession => (
                StatusCode::NOT_FOUND,
                Json(json!({
                    "error": "no previous provider session found to resume; pass an explicit session_id"
                })),
            ),
            ResumeRebindError::MissingCwd => (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({
                    "error": "target cwd is unknown; supply cwd alongside session_id"
                })),
            ),
            ResumeRebindError::TargetTranscriptMissing(reason) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({"error": reason})),
            ),
            ResumeRebindError::TargetCwdMissing(path) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({"error": format!("target cwd does not exist: {path}")})),
            ),
            ResumeRebindError::AutoUnsupportedProvider(provider) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({
                    "error": format!(
                        "auto previous-session selection is only supported for Claude; provider={provider}. Pass an explicit session_id."
                    )
                })),
            ),
            ResumeRebindError::HostUnsupported(reason) => (
                StatusCode::CONFLICT,
                Json(json!({
                    "error": format!("/resume is not supported for this session host: {reason}")
                })),
            ),
            ResumeRebindError::Database(error) | ResumeRebindError::Filesystem(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            ),
        }
    }
}
