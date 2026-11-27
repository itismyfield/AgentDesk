//! An HTTP session force-kill of a Herdr execution, picked from the force-kill's own session read:
//! the P11 terminate service retires it and the dispatch effects run under that service's exclusion.

use axum::Json;
use axum::http::StatusCode;
use futures::FutureExt;
use serde_json::{Value, json};

use super::force_kill::{FORCE_KILL_RETRY_LIMIT, Prepared};
use crate::app_state::AppState;
use crate::db::dispatched_sessions::hosted_execution::http_force::herdr_execution;
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::{Expected, HttpTerminate};

mod effects;
use effects::{HERDR_TERMINATE, HerdrKillEffects, KillInput, herdr_force_kill_effects};

/// The answer when the force-kill's read picked a Claude/Codex Herdr execution its row owns, under
/// the requested key and no other runtime channel, bound to that read's owner and dispatch.
pub(super) async fn herdr_force_kill(
    state: &AppState,
    prepared: &Prepared<'_>,
    retry: bool,
    reason: &str,
) -> Option<(StatusCode, Json<Value>)> {
    let hosted = prepared.hosted.as_ref()?;
    let record = herdr_execution(hosted.hosted.as_ref(), &hosted.identity)?;
    let provider = ProviderKind::from_str(&record.owner.provider);
    let named = prepared.provider.as_ref();
    // Only a request that named the row's own primary reaches the terminate service.
    let chosen = hosted.session_key == prepared.requested_key
        && (prepared.runtime_channel_id.as_deref()).is_none_or(|c| c == record.owner.channel_id)
        && named.is_none_or(|named| Some(named) == provider.as_ref())
        && matches!(provider, Some(ProviderKind::Claude | ProviderKind::Codex));
    if !chosen {
        return None;
    }
    let registry = state.health_registry.as_deref()?;
    #[cfg(any(test, feature = "test-support"))]
    super::herdr_probe_tests::run_before_selection_probe().await;
    let (key, dispatch) = (prepared.requested_key, prepared.active_dispatch_id.clone());
    let input = KillInput {
        state: state.clone(),
        pool: prepared.pool.clone(),
        session_key: key.to_string(),
        agent_id: prepared.agent_id.clone(),
        retry,
        reason: reason.to_string(),
    };
    let expected = Expected {
        owner: record.owner,
        dispatch: dispatch.clone(),
        captured_dispatch: true,
    };
    let bound = (hosted.session_key.clone(), record.execution_nonce, expected);
    let pool = prepared.pool.clone();
    let (outcome, effects) = registry
        .terminate_for_http(pool, bound, reason, {
            move |exclusion| herdr_force_kill_effects(exclusion, input).boxed()
        })
        .await;
    Some(answer(key, dispatch, outcome, effects))
}

/// Only a retire whose effects all applied is success; a run or effect that cannot say what it
/// left is a server error, and every other refusal changed nothing.
fn answer(
    session_key: &str,
    dispatch: Option<String>,
    (name, detail): (&'static str, String),
    effects: Option<HerdrKillEffects>,
) -> (StatusCode, Json<Value>) {
    let termination = json!({"result": name, "detail": detail});
    let refused = |status: StatusCode, error: String| {
        let body = json!({"error": error, "session_key": session_key, "termination": termination});
        (status, Json(body))
    };
    let Some(effects) = effects else {
        let status = match name {
            "indeterminate" => StatusCode::INTERNAL_SERVER_ERROR,
            _ => StatusCode::CONFLICT,
        };
        return refused(status, format!("herdr terminate {name}: {detail}"));
    };
    if let Some(step) = effects.failed {
        let error = format!("herdr terminate retired the execution; {step}");
        return refused(StatusCode::INTERNAL_SERVER_ERROR, error);
    }
    let body = json!({
        "ok": true,
        "tmux_killed": false,
        "inflight_cleared": effects.inflight_cleared,
        "lifecycle_path": HERDR_TERMINATE,
        "queued_remaining": effects.queued_remaining,
        "queue_preserved": true,
        "dispatch_failed": dispatch,
        "retry_dispatch_id": effects.retry_dispatch_id,
        "retry_limit": FORCE_KILL_RETRY_LIMIT,
        "retry_skipped_reason": effects.retry_skipped_reason,
        "queue_activation_requested": effects.activation_requested,
        "termination": termination,
    });
    (StatusCode::OK, Json(body))
}
