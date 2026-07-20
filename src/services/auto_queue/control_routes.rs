use super::*;
use axum::extract::Extension;

use crate::api_caller_observability::{
    RequestPrincipal, log_identity_consumption, manager_channel_check_relied_on_claimed_header,
};

/// PATCH /api/queue/runs/{id}
pub async fn update_run(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateRunBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    if body
        .deploy_phases
        .as_ref()
        .is_some_and(|phases| !phases.is_empty())
        && !deploy_phase_api_enabled(&state)
    {
        return Err(auto_queue_json_error(
            StatusCode::FORBIDDEN,
            Json(json!({
                "error": "deploy_phases requires server.auth_token to be configured"
            })),
        ));
    }

    let Some(pool) = state.pg_pool_ref() else {
        return Err(auto_queue_tuple_error(pg_unavailable_response()));
    };
    if let Some(max_concurrent_threads) = body.max_concurrent_threads {
        if max_concurrent_threads <= 0 {
            return Err(auto_queue_json_error(
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "max_concurrent_threads must be > 0"})),
            ));
        }
    }

    let ignored_unified_thread = body.unified_thread.is_some();
    if body.status.is_none()
        && body.deploy_phases.is_none()
        && body.max_concurrent_threads.is_none()
        && !ignored_unified_thread
    {
        return Err(auto_queue_json_error(
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "no fields to update"})),
        ));
    }

    match update_run_with_pg(&id, &body, pool).await {
        Ok(_) => Ok((
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "ignored": ignored_unified_thread.then_some(vec!["unified_thread"]),
            })),
        )),
        Err(error) => Err(auto_queue_json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        )),
    }
}

/// POST /api/queue/slots/{agent_id}/{slot_index}/reset-thread
pub async fn reset_slot_thread(
    State(state): State<AppState>,
    Path((agent_id, slot_index)): Path<(String, i64)>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let Some(pool) = state.pg_pool_ref() else {
        return Err(auto_queue_tuple_error(pg_unavailable_response()));
    };
    match crate::services::auto_queue::runtime::reset_slot_thread_bindings_pg(
        pool, &agent_id, slot_index,
    )
    .await
    {
        Ok((archived_threads, cleared_sessions, cleared_bindings)) => Ok((
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "agent_id": agent_id,
                "slot_index": slot_index,
                "archived_threads": archived_threads,
                "cleared_sessions": cleared_sessions,
                "cleared_bindings": cleared_bindings,
            })),
        )),
        Err(err) if err.contains("has active dispatch") => Err(auto_queue_json_error(
            StatusCode::CONFLICT,
            Json(json!({"error": err})),
        )),
        Err(err) => Err(auto_queue_json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": err})),
        )),
    }
}

/// POST /api/queue/reset
/// Reset a single agent queue. Requires `agent_id`.
pub async fn reset(
    State(state): State<AppState>,
    body: Bytes,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let body: ResetBody = match parse_json_body(body, "reset") {
        Ok(parsed) => parsed,
        Err(error) => {
            return Err(auto_queue_json_error(
                StatusCode::BAD_REQUEST,
                Json(json!({"error": error})),
            ));
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
            return Err(auto_queue_json_error(
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "agent_id is required for reset"})),
            ));
        }
    };

    let Some(pool) = state.pg_pool_ref() else {
        return Err(auto_queue_tuple_error(pg_unavailable_response()));
    };
    match reset_scoped_with_pg(agent_id, pool).await {
        Ok(response) => Ok((StatusCode::OK, Json(response))),
        Err(error) => Err(auto_queue_json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        )),
    }
}

/// POST /api/queue/reset-global
/// Global reset requires an explicit confirmation token.
pub async fn reset_global(
    State(state): State<AppState>,
    body: Bytes,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let body: ResetGlobalBody = match parse_json_body(body, "reset-global") {
        Ok(parsed) => parsed,
        Err(error) => {
            return Err(auto_queue_json_error(
                StatusCode::BAD_REQUEST,
                Json(json!({"error": error})),
            ));
        }
    };

    let confirmation_token = body
        .confirmation_token
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if confirmation_token != Some(RESET_GLOBAL_CONFIRMATION_TOKEN) {
        return Err(auto_queue_json_error(
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "confirmation_token is required for reset-global"})),
        ));
    }

    let Some(pool) = state.pg_pool_ref() else {
        return Err(auto_queue_tuple_error(pg_unavailable_response()));
    };
    match reset_global_with_pg(pool).await {
        Ok(response) => Ok((StatusCode::OK, Json(response))),
        Err(error) => Err(auto_queue_json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        )),
    }
}

/// POST /api/queue/pause — soft-pause active runs; `force=true` keeps the legacy cancel path
pub async fn pause(
    State(state): State<AppState>,
    body: Bytes,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let body: PauseBody = match parse_json_body(body, "pause") {
        Ok(parsed) => parsed,
        Err(error) => {
            return Err(auto_queue_json_error(
                StatusCode::BAD_REQUEST,
                Json(json!({"error": error})),
            ));
        }
    };
    let force = body.force.unwrap_or(false);

    let Some(pool) = state.pg_pool_ref() else {
        return Err(auto_queue_tuple_error(pg_unavailable_response()));
    };
    match if force {
        force_pause_with_pg(state.health_registry.clone(), pool).await
    } else {
        soft_pause_with_pg(pool).await
    } {
        Ok(response) => Ok((StatusCode::OK, Json(response))),
        Err(error) => Err(auto_queue_json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        )),
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
pub async fn resume_run(
    State(state): State<AppState>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let Some(pool) = state.pg_pool_ref() else {
        return Err(auto_queue_tuple_error(pg_unavailable_response()));
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
            return Err(auto_queue_json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("count postgres blocked auto-queue runs: {error}")})),
            ));
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
            return Err(auto_queue_json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("resume postgres auto-queue runs: {error}")})),
            ));
        }
    };

    if resumed > 0 {
        let dispatched = match activate(
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
        .await
        {
            Ok((_status, body)) => body.0.get("count").and_then(|v| v.as_u64()).unwrap_or(0),
            Err(_) => 0,
        };
        return Ok((
            StatusCode::OK,
            Json(
                json!({"ok": true, "resumed_runs": resumed, "blocked_runs": blocked_runs, "dispatched": dispatched}),
            ),
        ));
    }

    Ok((
        StatusCode::OK,
        Json(
            json!({"ok": true, "resumed_runs": 0, "blocked_runs": blocked_runs, "message": "No resumable runs"}),
        ),
    ))
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

/// Scope identifier under which phase-gate repair idempotency keys are stored.
/// Lets the same operator-supplied UUID be reused across unrelated endpoints
/// without aliasing in `idempotency_keys`.
const PHASE_GATE_REPAIR_IDEMPOTENCY_SCOPE: &str = "phase-gate-repair";

fn parse_idempotency_key_header(headers: &HeaderMap) -> Option<String> {
    headers
        .get("idempotency-key")
        .or_else(|| headers.get("Idempotency-Key"))
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty() && value.len() <= 256)
        .map(str::to_string)
}

fn trimmed_repair_header_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn configured_repair_operator_auth(config: &crate::config::Config) -> (Option<&str>, Option<&str>) {
    let token = config
        .server
        .auth_token
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let channel_id = config
        .kanban
        .manager_channel_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    (token, channel_id)
}

fn require_phase_gate_repair_operator_auth(
    config: &crate::config::Config,
    headers: &HeaderMap,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    let (expected_token, expected_channel_id) = configured_repair_operator_auth(config);
    if expected_token.is_none() && expected_channel_id.is_none() {
        tracing::warn!(
            "[auto-queue] phase-gate repair accepted without operator auth because no server.auth_token or kanban.manager_channel_id is configured"
        );
        return Ok(());
    }

    if let Some(expected_token) = expected_token {
        let provided = trimmed_repair_header_value(headers, "authorization")
            .and_then(|value| value.strip_prefix("Bearer "))
            .map(str::trim);
        if !provided
            .map(|token| crate::utils::auth::constant_time_token_eq(expected_token, token))
            .unwrap_or(false)
        {
            return Err((
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "phase-gate repair requires explicit Bearer token"})),
            ));
        }
    }

    if let Some(expected_channel_id) = expected_channel_id {
        let provided_channel_id = trimmed_repair_header_value(headers, "x-channel-id");
        if provided_channel_id != Some(expected_channel_id) {
            return Err((
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "phase-gate repair requires PMD channel authorization"})),
            ));
        }
    }

    Ok(())
}

fn log_phase_gate_repair_identity_consumption(
    headers: &HeaderMap,
    config: &crate::config::Config,
    principal: &Option<Extension<RequestPrincipal>>,
    caller: &RepairCaller,
    manager_channel_check_relied: bool,
) {
    let (_, expected_channel_id) = configured_repair_operator_auth(config);
    let caller_label = caller.audit_label();
    log_identity_consumption(
        "POST /api/queue/runs/{id}/phase-gates/repair",
        principal.as_ref().map(|Extension(principal)| principal),
        Some(caller_label.as_str()),
        manager_channel_check_relied
            && manager_channel_check_relied_on_claimed_header(headers, expected_channel_id),
    );
}

/// POST /api/queue/runs/{id}/phase-gates/repair
pub async fn repair_phase_gates(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    principal: Option<Extension<RequestPrincipal>>,
    body: Bytes,
) -> (StatusCode, Json<serde_json::Value>) {
    let mut caller = RepairCaller::from_headers(&headers);
    if let Err(response) = require_phase_gate_repair_operator_auth(&state.config, &headers) {
        log_phase_gate_repair_identity_consumption(
            &headers,
            &state.config,
            &principal,
            &caller,
            false,
        );
        // Unverified caller — explicitly mark the audit field so a spoofed
        // `x-agent-id` doesn't masquerade as a real principal in logs.
        audit_phase_gate_repair_rejected(&id, &caller, "unauthorized", "authorization failed");
        return response;
    }

    let idempotency_key = parse_idempotency_key_header(&headers);
    let request_fingerprint = idempotency_key.as_ref().map(|_| {
        crate::db::idempotency::fingerprint_request(
            "POST",
            &format!("/api/queue/runs/{id}/phase-gates/repair"),
            &body,
        )
    });

    let parsed_body: RepairPhaseGateBody = match parse_json_body(body, "phase-gates/repair") {
        Ok(parsed) => parsed,
        Err(error) => {
            log_phase_gate_repair_identity_consumption(
                &headers,
                &state.config,
                &principal,
                &caller,
                true,
            );
            audit_phase_gate_repair_rejected(&id, &caller, "bad_request", &error);
            return (StatusCode::BAD_REQUEST, Json(json!({"error": error})));
        }
    };
    let Some(pool) = state.pg_pool_ref() else {
        log_phase_gate_repair_identity_consumption(
            &headers,
            &state.config,
            &principal,
            &caller,
            true,
        );
        audit_phase_gate_repair_rejected(
            &id,
            &caller,
            "pg_unavailable",
            "postgres pool unavailable",
        );
        return pg_unavailable_response();
    };

    caller
        .verify(crate::services::kanban::resolve_requesting_agent_id_with_pg(pool, &headers).await);
    log_phase_gate_repair_identity_consumption(&headers, &state.config, &principal, &caller, true);

    // #2257 concern 5 / #3318: Stripe-style idempotency-key handling.
    // Inventory: this is currently the only route using the table-backed
    // `idempotency_keys` claim/record helper. Full transaction coupling is
    // not practical here without moving the phase-gate repair transaction
    // boundary out of the DB module, so this route relies on the repair
    // mutation's independent replay-safety: after a committed repair clears
    // a gate, a later re-execution sees no pending/failed candidate to clear.
    // Crash-window semantics are therefore:
    // - crash before `record_response`, retry before expiry: 409 in-flight;
    // - retry after expiry: repair re-executes and logs expired re-execution;
    // - completed slot: response is replayed verbatim.
    let idempotency_slot = if let (Some(key), Some(fingerprint)) =
        (idempotency_key.as_ref(), request_fingerprint.as_ref())
    {
        match crate::db::idempotency::claim(
            pool,
            PHASE_GATE_REPAIR_IDEMPOTENCY_SCOPE,
            key,
            fingerprint,
            Some(&caller.audit_label()),
            crate::db::idempotency::DEFAULT_IDEMPOTENCY_TTL,
        )
        .await
        {
            Ok(crate::db::idempotency::IdempotencyOutcome::Created { reclaimed_expired }) => {
                audit_phase_gate_repair_idempotency_claimed(&id, &caller, key, reclaimed_expired);
                Some(key.clone())
            }
            Ok(crate::db::idempotency::IdempotencyOutcome::Replay { status, body, .. }) => {
                audit_phase_gate_repair_rejected(
                    &id,
                    &caller,
                    "idempotency_replay",
                    "returning cached response",
                );
                return (
                    StatusCode::from_u16(status).unwrap_or(StatusCode::OK),
                    Json(body),
                );
            }
            Ok(crate::db::idempotency::IdempotencyOutcome::InFlight) => {
                audit_phase_gate_repair_rejected(
                    &id,
                    &caller,
                    "idempotency_in_flight",
                    "concurrent request with the same key is still running",
                );
                return (
                    StatusCode::CONFLICT,
                    Json(json!({
                        "error": "another request with this Idempotency-Key is still in flight",
                    })),
                );
            }
            Ok(crate::db::idempotency::IdempotencyOutcome::FingerprintMismatch { .. }) => {
                audit_phase_gate_repair_rejected(
                    &id,
                    &caller,
                    "idempotency_fingerprint_mismatch",
                    "key reused with a different request body",
                );
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(json!({
                        "error": "Idempotency-Key already used with a different request body",
                    })),
                );
            }
            Err(error) => {
                tracing::warn!(
                    run_id = %id,
                    key = %key,
                    error = %error,
                    "phase-gate repair idempotency claim failed; rejecting before mutation"
                );
                audit_phase_gate_repair_rejected(
                    &id,
                    &caller,
                    "idempotency_claim_failed",
                    "failed to claim Idempotency-Key before mutation",
                );
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({
                        "error": "failed to claim Idempotency-Key before mutation"
                    })),
                );
            }
        }
    } else {
        None
    };

    let options = crate::db::auto_queue::PhaseGateRepairOptions {
        phase: parsed_body.phase,
        dispatch_id: parsed_body.dispatch_id,
    };
    let (status, response_body) =
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
        };

    // #2257 concern 5: stamp the idempotency slot with the final response
    // so subsequent retries with the same key replay verbatim. We do this
    // for every terminal outcome — both success (200 OK) and the
    // structured error paths (400 / 404 / 500) — because Stripe's
    // contract guarantees the same response on replay regardless of
    // whether the original request "succeeded". On the rare write
    // failure we log and continue; the mutation is already committed, and
    // the slot will eventually be GC'd.
    if let Some(key) = idempotency_slot.as_ref() {
        let status_u16 = status.as_u16();
        let body_value = response_body.0.clone();
        if let Err(error) = crate::db::idempotency::record_response(
            pool,
            PHASE_GATE_REPAIR_IDEMPOTENCY_SCOPE,
            key,
            status_u16,
            &body_value,
        )
        .await
        {
            tracing::warn!(
                run_id = %id,
                key = %key,
                error = %error,
                "phase-gate repair idempotency record_response failed; slot will expire via GC"
            );
        }
    }

    (status, response_body)
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

fn audit_phase_gate_repair_idempotency_claimed(
    run_id: &str,
    caller: &RepairCaller,
    key: &str,
    reclaimed_expired: bool,
) {
    let ctx = AutoQueueLogContext::new().run(run_id);
    let span = crate::services::auto_queue::auto_queue_trace_span("phase_gate_repair", &ctx);
    let _guard = span.enter();
    let outcome = if reclaimed_expired {
        "idempotency_expired_reexecution"
    } else {
        "idempotency_claimed"
    };
    tracing::info!(
        caller = %caller.audit_label(),
        caller_verified = caller.verified.is_some(),
        caller_claimed = %caller.claimed,
        idempotency_key = %key,
        idempotency_reclaimed_expired = reclaimed_expired,
        outcome = outcome,
        "[auto-queue] phase-gate repair idempotency slot claimed"
    );
}

/// POST /api/queue/cancel — cancel all active/paused runs and pending entries
pub async fn cancel(
    State(state): State<AppState>,
    Query(query): Query<CancelQuery>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let Some(pool) = state.pg_pool_ref() else {
        return Err(auto_queue_tuple_error(pg_unavailable_response()));
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
        Ok(payload) => Ok((StatusCode::OK, Json(payload))),
        Err(error) => Ok(cancel_route_error_response(error)),
    }
}

/// PATCH /api/queue/reorder
pub async fn reorder(
    State(state): State<AppState>,
    Json(body): Json<ReorderBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let Some(pool) = state.pg_pool_ref() else {
        return Err(auto_queue_tuple_error(pg_unavailable_response()));
    };
    match reorder_with_pg(&body, pool).await {
        Ok(()) => Ok((StatusCode::OK, Json(json!({ "ok": true })))),
        Err(error) if error.starts_with("not_found:") => Err(auto_queue_json_error(
            StatusCode::NOT_FOUND,
            Json(json!({"error": error.trim_start_matches("not_found:")})),
        )),
        Err(error)
            if error == "ordered_ids cannot be empty"
                || error == "no pending entries found for reorder scope"
                || error == "ordered_ids do not match any pending entries in scope"
                || error == "replacement sequence exhausted"
                || error == "replacement sequence was not fully consumed" =>
        {
            Err(auto_queue_json_error(
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": error })),
            ))
        }
        Err(error) => Err(auto_queue_json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        )),
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
