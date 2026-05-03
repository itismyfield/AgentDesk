#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum StreamingFinalDisposition {
    FallbackCompleted,
    FallbackFailed,
    MissingAssistantResponse,
    NonWorkDispatch,
    UnknownDispatch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) struct StreamingFinalResult {
    pub completed: bool,
    pub disposition: StreamingFinalDisposition,
    pub dispatch_type: Option<String>,
    pub error: Option<String>,
}

impl StreamingFinalResult {
    fn completed(
        disposition: StreamingFinalDisposition,
        dispatch_type: Option<String>,
        error: Option<String>,
    ) -> Self {
        Self {
            completed: true,
            disposition,
            dispatch_type,
            error,
        }
    }

    fn not_completed(
        disposition: StreamingFinalDisposition,
        dispatch_type: Option<String>,
        error: Option<String>,
    ) -> Self {
        Self {
            completed: false,
            disposition,
            dispatch_type,
            error,
        }
    }
}

pub(in crate::services::discord) struct WatcherStreamingFinalRequest<'a> {
    pub pg_pool: Option<&'a sqlx::PgPool>,
    pub dispatch_id: &'a str,
    pub adk_cwd: Option<&'a str>,
    pub full_response: &'a str,
    pub has_assistant_response: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WatcherStreamingFinalPlan {
    FallbackCompleteWork,
    MissingAssistantResponse,
    NonWorkDispatch,
    UnknownDispatch,
}

fn plan_watcher_streaming_finalization(
    dispatch_type: Option<&str>,
    has_assistant_response: bool,
) -> WatcherStreamingFinalPlan {
    match dispatch_type {
        Some("implementation") | Some("rework") if has_assistant_response => {
            WatcherStreamingFinalPlan::FallbackCompleteWork
        }
        Some("implementation") | Some("rework") => {
            WatcherStreamingFinalPlan::MissingAssistantResponse
        }
        Some(_) => WatcherStreamingFinalPlan::NonWorkDispatch,
        None => WatcherStreamingFinalPlan::UnknownDispatch,
    }
}

fn watcher_completion_context(
    pg_pool: Option<&sqlx::PgPool>,
    dispatch_id: &str,
    source: &str,
    needs_reconcile: bool,
    adk_cwd: Option<&str>,
    full_response: &str,
) -> serde_json::Value {
    let mut context = super::turn_bridge::build_work_dispatch_completion_result(
        None::<&crate::db::Db>,
        pg_pool,
        dispatch_id,
        source,
        needs_reconcile,
        adk_cwd,
        Some(full_response),
    );
    if let Some(obj) = context.as_object_mut() {
        obj.insert(
            "agent_response_present".to_string(),
            serde_json::Value::Bool(true),
        );
    }
    context
}

async fn watcher_runtime_fallback(
    request: &WatcherStreamingFinalRequest<'_>,
    followup_source: &str,
) -> StreamingFinalResult {
    let fallback_result = watcher_completion_context(
        request.pg_pool,
        request.dispatch_id,
        "watcher_db_fallback",
        true,
        request.adk_cwd,
        request.full_response,
    );
    let completed = super::turn_bridge::runtime_db_fallback_complete_with_result(
        request.dispatch_id,
        &fallback_result,
    );
    if completed {
        let _ = super::turn_bridge::queue_dispatch_followup_with_handles(
            None::<&crate::db::Db>,
            request.pg_pool,
            request.dispatch_id,
            followup_source,
        )
        .await;
        StreamingFinalResult::completed(StreamingFinalDisposition::FallbackCompleted, None, None)
    } else {
        StreamingFinalResult::not_completed(
            StreamingFinalDisposition::FallbackFailed,
            None,
            Some("runtime DB fallback did not complete dispatch".to_string()),
        )
    }
}

pub(in crate::services::discord) async fn finalize_watcher_streaming_dispatch(
    request: WatcherStreamingFinalRequest<'_>,
) -> StreamingFinalResult {
    let dispatch_type =
        crate::services::discord::internal_api::lookup_dispatch_type(request.dispatch_id)
            .await
            .ok()
            .flatten();

    match plan_watcher_streaming_finalization(
        dispatch_type.as_deref(),
        request.has_assistant_response,
    ) {
        WatcherStreamingFinalPlan::FallbackCompleteWork => {
            let mut result =
                watcher_runtime_fallback(&request, "watcher_completed_runtime_fallback").await;
            result.dispatch_type = dispatch_type;
            result
        }
        WatcherStreamingFinalPlan::MissingAssistantResponse => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ watcher: refusing to complete work dispatch {} without assistant response",
                request.dispatch_id
            );
            StreamingFinalResult::not_completed(
                StreamingFinalDisposition::MissingAssistantResponse,
                dispatch_type,
                None,
            )
        }
        WatcherStreamingFinalPlan::NonWorkDispatch => StreamingFinalResult::completed(
            StreamingFinalDisposition::NonWorkDispatch,
            dispatch_type,
            None,
        ),
        WatcherStreamingFinalPlan::UnknownDispatch => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ watcher: could not resolve dispatch type for {}; leaving dispatch completion to reconcile path",
                request.dispatch_id
            );
            StreamingFinalResult::not_completed(
                StreamingFinalDisposition::UnknownDispatch,
                None,
                None,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_final_output_plans_runtime_fallback_and_context() {
        assert_eq!(
            plan_watcher_streaming_finalization(Some("implementation"), true),
            WatcherStreamingFinalPlan::FallbackCompleteWork
        );

        let context = watcher_completion_context(
            None,
            "dispatch-1",
            "watcher_completed",
            false,
            None,
            "final answer",
        );

        assert_eq!(
            context
                .get("completion_source")
                .and_then(|value| value.as_str()),
            Some("watcher_completed")
        );
        assert_eq!(
            context
                .get("agent_response_present")
                .and_then(|value| value.as_bool()),
            Some(true)
        );
        assert_ne!(
            context
                .get("needs_reconcile")
                .and_then(|value| value.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn abnormal_process_exit_without_assistant_response_preserves_work_dispatch() {
        assert_eq!(
            plan_watcher_streaming_finalization(Some("implementation"), false),
            WatcherStreamingFinalPlan::MissingAssistantResponse
        );
    }

    #[test]
    fn non_work_dispatch_is_left_to_own_completion_flow() {
        assert_eq!(
            plan_watcher_streaming_finalization(Some("review"), true),
            WatcherStreamingFinalPlan::NonWorkDispatch
        );
    }

    #[test]
    fn unknown_dispatch_type_preserves_state_for_reconcile() {
        assert_eq!(
            plan_watcher_streaming_finalization(None, true),
            WatcherStreamingFinalPlan::UnknownDispatch
        );
    }
}
