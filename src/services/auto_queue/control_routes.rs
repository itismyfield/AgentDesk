use super::*;

/// PATCH /api/queue/runs/{id}
pub async fn update_run(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateRunBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    if body
        .deploy_phases
        .as_ref()
        .is_some_and(|phases| !phases.is_empty())
        && !deploy_phase_api_enabled(&state)
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "error": "deploy_phases requires server.auth_token to be configured"
            })),
        );
    }

    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable_response();
    };
    if let Some(max_concurrent_threads) = body.max_concurrent_threads {
        if max_concurrent_threads <= 0 {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "max_concurrent_threads must be > 0"})),
            );
        }
    }

    let ignored_unified_thread = body.unified_thread.is_some();
    if body.status.is_none()
        && body.deploy_phases.is_none()
        && body.max_concurrent_threads.is_none()
        && !ignored_unified_thread
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "no fields to update"})),
        );
    }

    match update_run_with_pg(&id, &body, pool).await {
        Ok(_) => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "ignored": ignored_unified_thread.then_some(vec!["unified_thread"]),
            })),
        ),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        ),
    }
}

/// POST /api/queue/slots/{agent_id}/{slot_index}/reset-thread
pub async fn reset_slot_thread(
    State(state): State<AppState>,
    Path((agent_id, slot_index)): Path<(String, i64)>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable_response();
    };
    match crate::services::auto_queue::runtime::reset_slot_thread_bindings_pg(
        pool, &agent_id, slot_index,
    )
    .await
    {
        Ok((archived_threads, cleared_sessions, cleared_bindings)) => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "agent_id": agent_id,
                "slot_index": slot_index,
                "archived_threads": archived_threads,
                "cleared_sessions": cleared_sessions,
                "cleared_bindings": cleared_bindings,
            })),
        ),
        Err(err) if err.contains("has active dispatch") => {
            (StatusCode::CONFLICT, Json(json!({"error": err})))
        }
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": err})),
        ),
    }
}

/// POST /api/queue/reset
/// Reset a single agent queue. Requires `agent_id`.
pub async fn reset(
    State(state): State<AppState>,
    body: Bytes,
) -> (StatusCode, Json<serde_json::Value>) {
    let body: ResetBody = match parse_json_body(body, "reset") {
        Ok(parsed) => parsed,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": error})));
        }
    };

    let agent_id = match body
        .agent_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(agent_id) => agent_id,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "agent_id is required for reset"})),
            );
        }
    };

    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable_response();
    };
    match reset_scoped_with_pg(agent_id, pool).await {
        Ok(response) => (StatusCode::OK, Json(response)),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        ),
    }
}

/// POST /api/queue/reset-global
/// Global reset requires an explicit confirmation token.
pub async fn reset_global(
    State(state): State<AppState>,
    body: Bytes,
) -> (StatusCode, Json<serde_json::Value>) {
    let body: ResetGlobalBody = match parse_json_body(body, "reset-global") {
        Ok(parsed) => parsed,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": error})));
        }
    };

    let confirmation_token = body
        .confirmation_token
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if confirmation_token != Some(RESET_GLOBAL_CONFIRMATION_TOKEN) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "confirmation_token is required for reset-global"})),
        );
    }

    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable_response();
    };
    match reset_global_with_pg(pool).await {
        Ok(response) => (StatusCode::OK, Json(response)),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        ),
    }
}

/// POST /api/queue/pause — soft-pause active runs; `force=true` keeps the legacy cancel path
pub async fn pause(
    State(state): State<AppState>,
    body: Bytes,
) -> (StatusCode, Json<serde_json::Value>) {
    let body: PauseBody = match parse_json_body(body, "pause") {
        Ok(parsed) => parsed,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": error})));
        }
    };
    let force = body.force.unwrap_or(false);

    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable_response();
    };
    match if force {
        force_pause_with_pg(state.health_registry.clone(), pool).await
    } else {
        soft_pause_with_pg(pool).await
    } {
        Ok(response) => (StatusCode::OK, Json(response)),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        ),
    }
}

pub(super) fn cancel_route_error_response(
    error: crate::error::AppError,
) -> (StatusCode, Json<serde_json::Value>) {
    let mut body = json!({ "error": error.message() });
    if let Some(run_id) = error.context().get("run_id") {
        body["run_id"] = run_id.clone();
    }
    if let Some(status) = error.context().get("status") {
        body["status"] = status.clone();
    }
    (error.status(), Json(body))
}

/// POST /api/queue/resume — resume paused runs and dispatch next entry
pub async fn resume_run(State(state): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable_response();
    };
    let blocked_runs = match sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)
         FROM auto_queue_runs r
         WHERE r.status = 'paused'
           AND EXISTS (
               SELECT 1
               FROM auto_queue_phase_gates pg
               WHERE pg.run_id = r.id
                 AND pg.status IN ('pending', 'failed')
           )",
    )
    .fetch_one(pool)
    .await
    {
        Ok(value) => value,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("count postgres blocked auto-queue runs: {error}")})),
            );
        }
    };
    let resumed = match sqlx::query(
        "UPDATE auto_queue_runs
         SET status = 'active',
             completed_at = NULL
         WHERE status = 'paused'
           AND NOT EXISTS (
               SELECT 1
               FROM auto_queue_phase_gates pg
               WHERE pg.run_id = auto_queue_runs.id
                 AND pg.status IN ('pending', 'failed')
           )",
    )
    .execute(pool)
    .await
    {
        Ok(result) => result.rows_affected() as i64,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("resume postgres auto-queue runs: {error}")})),
            );
        }
    };

    if resumed > 0 {
        let (_status, body) = activate(
            State(state),
            Json(ActivateBody {
                run_id: None,
                repo: None,
                agent_id: None,
                thread_group: None,
                unified_thread: None,
                active_only: Some(true),
            }),
        )
        .await;
        let dispatched = body.0.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
        return (
            StatusCode::OK,
            Json(
                json!({"ok": true, "resumed_runs": resumed, "blocked_runs": blocked_runs, "dispatched": dispatched}),
            ),
        );
    }

    (
        StatusCode::OK,
        Json(
            json!({"ok": true, "resumed_runs": 0, "blocked_runs": blocked_runs, "message": "No resumable runs"}),
        ),
    )
}

/// Max length of `failed_reason` carried into structured audit logs / JSON
/// responses. The repair pipeline pulls this string from `task_dispatches.result`
/// which is operator-authored — truncating bounds log volume and limits the
/// blast radius if a reviewer ever puts something sensitive in a verdict body.
/// #2257: rationale for the explicit cap.
const FAILED_REASON_AUDIT_MAX_LEN: usize = 256;

fn truncate_failed_reason(reason: Option<String>) -> Option<String> {
    reason.map(|raw| {
        if raw.len() <= FAILED_REASON_AUDIT_MAX_LEN {
            raw
        } else {
            let mut clipped: String = raw.chars().take(FAILED_REASON_AUDIT_MAX_LEN).collect();
            clipped.push_str("…[truncated]");
            clipped
        }
    })
}

#[derive(Debug, Clone)]
struct RepairCaller {
    /// Whatever the client claimed via `x-agent-id` / `x-channel-id`
    /// headers BEFORE the auth check. Attacker-controlled on rejection.
    claimed: String,
    /// `Some` only if PG resolved the headers to a real agent identity
    /// (`resolve_requesting_agent_id_with_pg`). `None` means we accepted
    /// the request via the bearer-token path with no PG-verified
    /// principal — audit consumers must treat `claimed` as unverified.
    verified: Option<String>,
}

impl RepairCaller {
    fn from_headers(headers: &HeaderMap) -> Self {
        Self {
            claimed: repair_phase_gate_caller_from_headers(headers),
            verified: None,
        }
    }

    fn verify(&mut self, resolved: Option<String>) {
        self.verified = resolved;
    }

    /// For logs: prefer the verified principal; fall back to the claimed
    /// header value with an explicit `unverified:` prefix so audit
    /// aggregators don't confuse the two.
    fn audit_label(&self) -> String {
        match self.verified.as_deref() {
            Some(label) => label.to_string(),
            None => format!("unverified:{}", self.claimed),
        }
    }
}

/// POST /api/queue/runs/{id}/phase-gates/repair
pub async fn repair_phase_gates(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, Json<serde_json::Value>) {
    let mut caller = RepairCaller::from_headers(&headers);
    if let Err(response) =
        crate::server::routes::kanban::require_explicit_bearer_token(&headers, "phase-gate repair")
    {
        // Unverified caller — explicitly mark the audit field so a spoofed
        // `x-agent-id` doesn't masquerade as a real principal in logs.
        audit_phase_gate_repair_rejected(&id, &caller, "unauthorized", "authorization failed");
        return response;
    }

    let body: RepairPhaseGateBody = match parse_json_body(body, "phase-gates/repair") {
        Ok(parsed) => parsed,
        Err(error) => {
            audit_phase_gate_repair_rejected(&id, &caller, "bad_request", &error);
            return (StatusCode::BAD_REQUEST, Json(json!({"error": error})));
        }
    };
    let Some(pool) = state.pg_pool_ref() else {
        audit_phase_gate_repair_rejected(
            &id,
            &caller,
            "pg_unavailable",
            "postgres pool unavailable",
        );
        return pg_unavailable_response();
    };

    caller.verify(
        crate::server::routes::kanban::resolve_requesting_agent_id_with_pg(pool, &headers).await,
    );
    let options = crate::db::auto_queue::PhaseGateRepairOptions {
        phase: body.phase,
        dispatch_id: body.dispatch_id,
    };
    match crate::db::auto_queue::repair_phase_gates_for_run_on_pg(pool, &id, options).await {
        Ok(summary) => {
            audit_phase_gate_repair_summary(&caller, &summary);
            let outcomes: Vec<serde_json::Value> = summary
                .outcomes
                .into_iter()
                .map(|outcome| {
                    json!({
                        "dispatch_id": outcome.dispatch_id,
                        "phase": outcome.phase,
                        "outcome": outcome.outcome,
                        "run_resumed": outcome.run_resumed,
                        "run_finalized": outcome.run_finalized,
                        "pending_count": outcome.pending_count,
                        "failed_reason": truncate_failed_reason(outcome.failed_reason),
                    })
                })
                .collect();
            (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "run_id": summary.run_id,
                    "phase_filter": summary.phase_filter,
                    "dispatch_id_filter": summary.dispatch_id_filter,
                    "candidate_dispatches": summary.candidate_dispatches,
                    "cleared_gates": summary.cleared_gates,
                    "failed_gates": summary.failed_gates,
                    "awaiting_siblings": summary.awaiting_siblings,
                    "stale_dispatches": summary.stale_dispatches,
                    "no_context_dispatches": summary.no_context_dispatches,
                    "orphan_gates_skipped": summary.orphan_gates_skipped,
                    "blocking_gates_remaining": summary.blocking_gates_remaining,
                    "run_status": summary.run_status,
                    "outcomes": outcomes,
                })),
            )
        }
        Err(error @ crate::db::auto_queue::PhaseGateRepairError::InvalidRequest { .. }) => {
            let message = error.to_string();
            audit_phase_gate_repair_rejected(&id, &caller, error.kind(), &message);
            (StatusCode::BAD_REQUEST, Json(json!({"error": message})))
        }
        Err(error @ crate::db::auto_queue::PhaseGateRepairError::NotFound { .. }) => {
            let message = error.to_string();
            audit_phase_gate_repair_rejected(&id, &caller, error.kind(), &message);
            (StatusCode::NOT_FOUND, Json(json!({"error": message})))
        }
        Err(error) => {
            let message = error.to_string();
            audit_phase_gate_repair_rejected(&id, &caller, error.kind(), &message);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": message})),
            )
        }
    }
}

fn repair_phase_gate_caller_from_headers(headers: &HeaderMap) -> String {
    headers
        .get("x-agent-id")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| format!("agent:{value}"))
        .or_else(|| {
            headers
                .get("x-channel-id")
                .and_then(|value| value.to_str().ok())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| format!("channel:{value}"))
        })
        .unwrap_or_else(|| "api".to_string())
}

fn audit_phase_gate_repair_summary(
    caller: &RepairCaller,
    summary: &crate::db::auto_queue::PhaseGateRepairSummary,
) {
    let ctx = AutoQueueLogContext::new().run(&summary.run_id);
    let span = crate::services::auto_queue::auto_queue_trace_span("phase_gate_repair", &ctx);
    let _guard = span.enter();
    tracing::info!(
        caller = %caller.audit_label(),
        caller_verified = caller.verified.is_some(),
        caller_claimed = %caller.claimed,
        outcome = "ok",
        phase_filter = ?summary.phase_filter,
        dispatch_id_filter = ?summary.dispatch_id_filter,
        candidate_dispatches = summary.candidate_dispatches,
        cleared_gates = summary.cleared_gates,
        failed_gates = summary.failed_gates,
        awaiting_siblings = summary.awaiting_siblings,
        stale_dispatches = summary.stale_dispatches,
        no_context_dispatches = summary.no_context_dispatches,
        orphan_gates_skipped = summary.orphan_gates_skipped,
        blocking_gates_remaining = summary.blocking_gates_remaining,
        run_status = ?summary.run_status,
        "[auto-queue] phase-gate repair completed"
    );
}

fn audit_phase_gate_repair_rejected(
    run_id: &str,
    caller: &RepairCaller,
    outcome: &str,
    error: &str,
) {
    let ctx = AutoQueueLogContext::new().run(run_id);
    let span = crate::services::auto_queue::auto_queue_trace_span("phase_gate_repair", &ctx);
    let _guard = span.enter();
    tracing::warn!(
        caller = %caller.audit_label(),
        caller_verified = caller.verified.is_some(),
        caller_claimed = %caller.claimed,
        outcome = %outcome,
        error = %error,
        "[auto-queue] phase-gate repair rejected"
    );
}

/// POST /api/queue/cancel — cancel all active/paused runs and pending entries
pub async fn cancel(
    State(state): State<AppState>,
    Query(query): Query<CancelQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable_response();
    };
    let service = state.auto_queue_service();
    let result = if let Some(run_id) = query
        .run_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        service
            .cancel_run_with_pg(state.health_registry.clone(), pool, run_id)
            .await
    } else {
        service
            .cancel_runs_with_pg(state.health_registry.clone(), pool)
            .await
    };
    match result {
        Ok(payload) => (StatusCode::OK, Json(payload)),
        Err(error) => cancel_route_error_response(error),
    }
}

/// PATCH /api/queue/reorder
pub async fn reorder(
    State(state): State<AppState>,
    Json(body): Json<ReorderBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable_response();
    };
    match reorder_with_pg(&body, pool).await {
        Ok(()) => (StatusCode::OK, Json(json!({ "ok": true }))),
        Err(error) if error.starts_with("not_found:") => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": error.trim_start_matches("not_found:")})),
        ),
        Err(error)
            if error == "ordered_ids cannot be empty"
                || error == "no pending entries found for reorder scope"
                || error == "ordered_ids do not match any pending entries in scope"
                || error == "replacement sequence exhausted"
                || error == "replacement sequence was not fully consumed" =>
        {
            (StatusCode::BAD_REQUEST, Json(json!({ "error": error })))
        }
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        ),
    }
}

#[cfg(test)]
mod phase_gate_repair_route_tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn truncate_failed_reason_returns_none_for_none() {
        assert!(truncate_failed_reason(None).is_none());
    }

    #[test]
    fn truncate_failed_reason_passes_through_short_strings() {
        let input = "short reason".to_string();
        assert_eq!(truncate_failed_reason(Some(input.clone())), Some(input),);
    }

    #[test]
    fn truncate_failed_reason_clips_oversized_strings_with_tag() {
        let oversized = "x".repeat(FAILED_REASON_AUDIT_MAX_LEN + 50);
        let truncated = truncate_failed_reason(Some(oversized)).expect("Some");
        assert!(
            truncated.ends_with("…[truncated]"),
            "expected truncation marker, got: {truncated:?}"
        );
        let prefix_byte_len: usize = truncated
            .chars()
            .take(FAILED_REASON_AUDIT_MAX_LEN)
            .map(char::len_utf8)
            .sum();
        let marker_byte_len: usize = "…[truncated]".len();
        assert_eq!(truncated.len(), prefix_byte_len + marker_byte_len);
    }

    #[test]
    fn repair_caller_audit_label_marks_unverified_when_pg_unresolved() {
        let mut headers = HeaderMap::new();
        headers.insert("x-agent-id", HeaderValue::from_static("attacker-claim"));
        let caller = RepairCaller::from_headers(&headers);
        assert_eq!(caller.audit_label(), "unverified:agent:attacker-claim");
        assert!(caller.verified.is_none());
    }

    #[test]
    fn repair_caller_audit_label_uses_verified_principal_when_pg_resolved() {
        let mut headers = HeaderMap::new();
        headers.insert("x-agent-id", HeaderValue::from_static("attacker-claim"));
        let mut caller = RepairCaller::from_headers(&headers);
        caller.verify(Some("agent:real-orchestrator".to_string()));
        assert_eq!(caller.audit_label(), "agent:real-orchestrator");
        assert_eq!(caller.claimed, "agent:attacker-claim");
    }
}
