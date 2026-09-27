use crate::services::health_diagnostics::ChannelSessionState;
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use std::future::Future;

pub(super) async fn with_session<F: Future<Output = Response>>(
    lookup: impl Future<Output = Result<Option<ChannelSessionState>, String>>,
    before: &impl serde::Serialize,
    before_watcher_inflight: &impl serde::Serialize,
    repair: impl FnOnce(Option<ChannelSessionState>) -> F,
) -> Response {
    let before_session_state = match lookup.await {
        Ok(session) => session,
        Err(error) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "ok": false,
                    "status": "skipped",
                    "applied": false,
                    "skipped": true,
                    "fix_safety": crate::cli::doctor::contract::FixSafety::NotFixable,
                    "safety_gate": "measurement_unavailable",
                    "skipped_reason": "session state could not be measured before repair",
                    "session_lookup_error": error,
                    "post_repair_mailbox": before,
                    "post_repair_watcher_inflight": before_watcher_inflight
                })),
            )
                .into_response();
        }
    };
    if before_session_state
        .as_ref()
        .and_then(|session| session.active_dispatch_id.as_deref())
        .is_some_and(|dispatch_id| !dispatch_id.trim().is_empty())
    {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "ok": false,
                "applied": false,
                "skipped": true,
                "fix_safety": crate::cli::doctor::contract::FixSafety::ExplicitRestartRequired,
                "safety_gate": "active_dispatch_present",
                "skipped_reason": "session record still has active dispatch evidence",
                "pre_repair_session": before_session_state,
                "post_repair_mailbox": before,
                "post_repair_watcher_inflight": before_watcher_inflight
            })),
        )
            .into_response();
    }
    repair(before_session_state).await
}

pub(super) fn post_session(
    result: Result<Option<ChannelSessionState>, String>,
) -> (Option<ChannelSessionState>, Option<String>) {
    match result {
        Ok(session) => (session, None),
        Err(error) => (None, Some(error)),
    }
}

pub(super) fn post_status(
    residual_inflight: bool,
    residual_working_session: bool,
    disconnect_error: Option<&str>,
    lookup_error: Option<&str>,
) -> &'static str {
    if residual_inflight
        || residual_working_session
        || disconnect_error.is_some()
        || lookup_error.is_some()
    {
        "partial_repair"
    } else {
        "applied"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use serde_json::json;

    #[tokio::test]
    async fn session_observation_gates_actual_repair_calls() {
        for case in [
            "query failure",
            "decode failure",
            "absent",
            "null",
            "blank",
            "active",
        ] {
            let lookup = match case {
                "query failure" | "decode failure" => Err(case.to_owned()),
                "absent" => Ok(None),
                _ => Ok(Some(ChannelSessionState {
                    agent_id: None,
                    provider: None,
                    status: Some("working".into()),
                    active_dispatch_id: match case {
                        "active" => Some(" dispatch-1 ".into()),
                        "blank" => Some(" \t ".into()),
                        _ => None,
                    },
                    thread_channel_id: Some("77".into()),
                })),
            };
            let expected = match case {
                "query failure" | "decode failure" => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "measurement_unavailable",
                    "not_fixable",
                    0,
                ),
                "active" => (
                    StatusCode::CONFLICT,
                    "active_dispatch_present",
                    "explicit_restart_required",
                    0,
                ),
                _ => (
                    StatusCode::OK,
                    "no_live_work_evidence",
                    "safe_local_repair",
                    1,
                ),
            };
            let before = json!({"has_cancel_token": true, "queue_depth": 0});
            let mut changes = 0;
            let response = with_session(std::future::ready(lookup), &before, &serde_json::Value::Null, |session| {
                changes += 1;
                if !case.ends_with("failure") {
                    assert_eq!(session.is_none(), case == "absent");
                }
                std::future::ready(Json(json!({"applied": true, "safety_gate": "no_live_work_evidence", "fix_safety": "safe_local_repair"})).into_response())
            }).await;
            let status = response.status();
            let body: serde_json::Value =
                serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                    .unwrap();
            assert_eq!(changes, expected.3, "{case}: {status} {body}");
            assert_eq!(status, expected.0, "{case}: {body}");
            assert_eq!(body["safety_gate"], expected.1);
            assert_eq!(body["fix_safety"], expected.2);
            if expected.3 == 0 {
                assert_eq!(body["applied"], false);
                assert_eq!(body["skipped"], true);
                assert_eq!(body["post_repair_mailbox"], before);
                if case.ends_with("failure") {
                    assert_eq!(body["session_lookup_error"], case);
                }
            }
        }
        for failed in [false, true] {
            let result = if failed {
                Err("recheck failed".into())
            } else {
                Ok(None)
            };
            let (_, error) = post_session(result);
            let status = post_status(false, false, None, error.as_deref());
            assert_eq!(status, if failed { "partial_repair" } else { "applied" });
            assert_eq!(
                super::super::registry_purge_decision(true, status),
                if failed {
                    super::super::RegistryPurgeDecision::Skip("repair_not_fully_applied")
                } else {
                    super::super::RegistryPurgeDecision::Run
                }
            );
        }
    }
}
