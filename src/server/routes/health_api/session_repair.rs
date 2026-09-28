use crate::services::discord::{health, relay_recovery::unmeasured_tail_of};
use crate::services::health_diagnostics::ChannelSessionState;
use crate::services::provider::ProviderKind;
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use std::future::Future;

pub(super) async fn with_session<E, F: Future<Output = Response>>(
    lookup: impl Future<Output = Result<Option<ChannelSessionState>, String>>,
    before: &impl serde::Serialize,
    before_watcher_inflight: &impl serde::Serialize,
    measured_gate: impl FnOnce() -> Result<E, Response>,
    repair: impl FnOnce(Option<ChannelSessionState>, E) -> F,
) -> Response {
    let lookup = lookup.await;
    if let Ok(Some(session)) = &lookup
        && session
            .active_dispatch_id
            .as_deref()
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
                "pre_repair_session": session,
                "post_repair_mailbox": before,
                "post_repair_watcher_inflight": before_watcher_inflight
            })),
        )
            .into_response();
    }
    // Measured refusals keep their reason and audit record even if session lookup failed.
    let evidence = match measured_gate() {
        Ok(evidence) => evidence,
        Err(refusal) => return refusal,
    };
    let before_session_state = match lookup {
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
    repair(before_session_state, evidence).await
}

pub(super) fn idle_tmux_admits(
    registry_present: bool,
    snapshot: &Option<health::WatcherStateSnapshot>,
    channel_id: u64,
) -> bool {
    let Some(snapshot) = snapshot.as_ref().filter(|_| registry_present) else {
        return false;
    };
    let Some(provider) = ProviderKind::from_str(&snapshot.provider) else {
        return false;
    };
    if snapshot.tmux_session.is_none() {
        return false;
    }
    let inflight_safe = !snapshot.inflight_state_present
        || crate::services::discord::inflight_state_allows_idle_tmux_repair_for_channel(
            &provider, channel_id,
        )
        .unwrap_or(false);
    // Preserve persisted final answers for normal recovery.
    let unrelayed_tail =
        crate::services::discord::relay_recovery::channel_has_unrelayed_idle_tmux_tail_answer(
            &provider, channel_id,
        );
    // Record an unmeasured tail only when the other idle-tmux conditions admit.
    let no_unread_bytes = crate::services::discord::relay_recovery::stale_mailbox_idle_tail_admits(
        &provider,
        snapshot,
        inflight_safe && !unrelayed_tail,
    );
    inflight_safe && no_unread_bytes && !unrelayed_tail
}

pub(super) fn tmux_refusal(
    before: &impl serde::Serialize,
    before_watcher_inflight: &Option<health::WatcherStateSnapshot>,
) -> Response {
    (
        StatusCode::CONFLICT,
        Json(serde_json::json!({
            "ok": false,
            "applied": false,
            "skipped": true,
            "fix_safety": crate::cli::doctor::contract::FixSafety::ExplicitRestartRequired,
            "safety_gate": "tmux_present",
            "skipped_reason": "live tmux evidence exists",
            "unread_tail": before_watcher_inflight.as_ref().and_then(unmeasured_tail_of),
            "post_repair_mailbox": before,
            "post_repair_watcher_inflight": before_watcher_inflight
        })),
    )
        .into_response()
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
    async fn measured_refusal_precedes_failed_session_lookup() {
        let before = json!({"has_cancel_token": true, "queue_depth": 0});
        let mut gate_calls = 0;
        let mut repair_calls = 0;
        let response = with_session(
            std::future::ready(Err("query failed".to_owned())),
            &before,
            &None::<health::WatcherStateSnapshot>,
            || {
                gate_calls += 1;
                Err::<(), _>(tmux_refusal(&before, &None))
            },
            |_, ()| {
                repair_calls += 1;
                std::future::ready(StatusCode::OK.into_response())
            },
        )
        .await;
        let status = response.status();
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(repair_calls, 0, "{body}");
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["safety_gate"], "tmux_present");
        assert_eq!(body["fix_safety"], "explicit_restart_required");
        assert_eq!(body["applied"], false);
        assert_eq!(body["skipped"], true);
        assert_eq!(body["post_repair_mailbox"], before);
        assert_eq!(gate_calls, 1);
    }

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
            let response = with_session(std::future::ready(lookup), &before, &serde_json::Value::Null, || Ok(()), |session, ()| {
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
