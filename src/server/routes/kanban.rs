use axum::{
    Json,
    extract::{Extension, Path, Query, State},
    http::{HeaderMap, StatusCode},
};
use poise::serenity_prelude::ChannelId;
use serde_json::json;

use super::AppState;
use crate::api_caller_observability::{
    RequestPrincipal, log_identity_consumption, manager_channel_check_relied_on_claimed_header,
};
use crate::db::kanban_cards as kanban_db;
use crate::db::kanban_cards::{IssueCardUpsert, upsert_card_from_issue_pg};
use crate::error::{AppError, AppResult, ErrorCode};
pub use crate::server::dto::kanban::{
    AssignCardBody, AssignIssueBody, BatchRereviewBody, CreateCardBody, DeferDodBody,
    ForceTransitionBody, ListCardsQuery, PmDecisionBody, RedispatchCardBody, ReopenBody,
    RereviewBody, RetryCardBody, UpdateCardBody,
};
use crate::services::provider::ProviderKind;
use crate::services::turn_lifecycle::{TurnLifecycleTarget, force_kill_turn};

// ── Query / Body types ─────────────────────────────────────────

const MIXED_STATUS_FIELD_UPDATE_ERROR: &str = "PATCH /api/kanban-cards/{id} cannot combine status changes with metadata or other field updates. Send metadata/field updates in one request, then send a status-only PATCH request, or use POST /api/kanban-cards/{id}/transition for administrative force transitions.";

fn validate_update_card_fields(body: &UpdateCardBody) -> Result<bool, String> {
    let has_non_status_updates = body.title.is_some()
        || body.priority.is_some()
        || body.assignee_agent_id.is_some()
        || body.repo_id.is_some()
        || body.github_issue_url.is_some()
        || body.description.is_some()
        || body.metadata.is_some()
        || body.metadata_json.is_some()
        || body.review_status.is_some()
        || body.review_notes.is_some();

    if !has_non_status_updates && body.status.is_none() {
        return Err("no fields to update".to_string());
    }

    if body.status.is_some() && has_non_status_updates {
        return Err(MIXED_STATUS_FIELD_UPDATE_ERROR.to_string());
    }

    // #1690: Pre-validate raw metadata_json so invalid JSON returns HTTP 400 from
    // the route layer instead of bubbling up as a Postgres `::jsonb` cast error
    // (HTTP 500) from update_card_fields_pg.
    if let Some(raw) = body.metadata_json.as_deref() {
        if let Err(err) = serde_json::from_str::<serde_json::Value>(raw) {
            return Err(format!("invalid metadata_json: {err}"));
        }
    }

    Ok(has_non_status_updates)
}

fn apply_deferred_dod_changes(current: Option<String>, body: DeferDodBody) -> serde_json::Value {
    let mut dod: serde_json::Value = current
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({"items": [], "verified": []}));

    if let Some(items) = body.items {
        dod["items"] = json!(items);
    }

    if let Some(verify) = body.verify {
        let verified = dod["verified"].as_array().cloned().unwrap_or_default();
        let mut v_set: Vec<serde_json::Value> = verified;
        for item in verify {
            let val = json!(item);
            if !v_set.contains(&val) {
                v_set.push(val);
            }
        }
        dod["verified"] = json!(v_set);
    }

    if let Some(unverify) = body.unverify
        && let Some(arr) = dod["verified"].as_array()
    {
        let filtered: Vec<serde_json::Value> = arr
            .iter()
            .filter(|v| {
                if let Some(s) = v.as_str() {
                    !unverify.contains(&s.to_string())
                } else {
                    true
                }
            })
            .cloned()
            .collect();
        dod["verified"] = json!(filtered);
    }

    if let Some(remove) = body.remove {
        if let Some(arr) = dod["items"].as_array() {
            let filtered: Vec<serde_json::Value> = arr
                .iter()
                .filter(|v| {
                    if let Some(s) = v.as_str() {
                        !remove.contains(&s.to_string())
                    } else {
                        true
                    }
                })
                .cloned()
                .collect();
            dod["items"] = json!(filtered);
        }
        if let Some(arr) = dod["verified"].as_array() {
            let filtered: Vec<serde_json::Value> = arr
                .iter()
                .filter(|v| {
                    if let Some(s) = v.as_str() {
                        !remove.contains(&s.to_string())
                    } else {
                        true
                    }
                })
                .cloned()
                .collect();
            dod["verified"] = json!(filtered);
        }
    }

    dod
}

fn is_allowed_manual_transition(from: &str, to: &str) -> bool {
    (from == "backlog" && to == "ready") || (from != to && to == "backlog")
}

async fn cancel_turn_targets(
    state: &AppState,
    targets: &[kanban_db::ActiveTurnTarget],
    reason: &str,
) {
    for target in targets {
        let tmux_name = target
            .session_key
            .split(':')
            .last()
            .unwrap_or(&target.session_key)
            .to_string();
        let lifecycle = force_kill_turn(
            state.health_registry.as_deref(),
            &TurnLifecycleTarget {
                provider: target.provider.as_deref().and_then(ProviderKind::from_str),
                channel_id: target
                    .thread_channel_id
                    .as_deref()
                    .and_then(|value| value.parse::<u64>().ok())
                    .map(ChannelId::new),
                tmux_name: tmux_name.clone(),
            },
            reason,
            "kanban_backlog_revert",
        )
        .await;

        tracing::info!(
            "[kanban] cancelled live turn during backlog revert: session={}, tmux={}, killed={}, lifecycle={}",
            target.session_key,
            tmux_name,
            lifecycle.tmux_killed,
            lifecycle.lifecycle_path,
        );

        if let Some(pool) = state.pg_pool_ref() {
            kanban_db::clear_session_for_turn_target_pg(pool, &target.session_key)
                .await
                .ok();
        } else {
            tracing::warn!(
                target = %target.session_key,
                "[kanban] cancel_turn_targets skipped session-clear: postgres pool unavailable (#1239)"
            );
        }
    }
}

async fn transition_card_to_backlog_with_cleanup(
    state: &AppState,
    card_id: &str,
    source: &str,
) -> anyhow::Result<crate::kanban::TransitionResult> {
    let pool = state.pg_pool_ref().ok_or_else(|| {
        anyhow::anyhow!("transition_card_to_backlog_with_cleanup requires postgres pool (#1239)")
    })?;
    let turn_targets = kanban_db::load_active_turn_targets_for_card_pg(pool, card_id).await?;
    let result = crate::kanban::transition_status_with_opts_and_allowed_cleanup_pg_only(
        pool,
        &state.engine,
        card_id,
        "backlog",
        source,
        crate::engine::transition::ForceIntent::SystemRecovery,
        crate::kanban::AllowedOnConnMutation::ForceTransitionRevertCleanup,
    )
    .await
    .map(|(result, _)| result)?;
    cancel_turn_targets(state, &turn_targets, "kanban backlog revert").await;
    Ok(result)
}

fn pg_pool_required_error() -> AppError {
    AppError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        ErrorCode::Database,
        "postgres backend required for kanban transition (#1384)",
    )
}

fn database_error(message: impl Into<String>) -> AppError {
    AppError::internal(message).with_code(ErrorCode::Database)
}

fn kanban_error(status: StatusCode, message: impl Into<String>) -> AppError {
    AppError::new(status, ErrorCode::Kanban, message)
}

fn tuple_error(response: (StatusCode, Json<serde_json::Value>)) -> AppError {
    let (status, Json(body)) = response;
    let message = body
        .get("error")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("kanban request failed")
        .to_string();
    kanban_error(status, message)
}

fn pg_pool_required_anyhow() -> anyhow::Error {
    anyhow::anyhow!("postgres backend required for kanban transition (#1384)")
}

fn execute_transition_intents_pg(
    state: &AppState,
    intents: &[crate::engine::transition::TransitionIntent],
) -> anyhow::Result<()> {
    let pool = state.pg_pool_ref().ok_or_else(pg_pool_required_anyhow)?;
    let intents = intents.to_vec();
    crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |pool| async move {
            let mut tx = pool.begin().await.map_err(|error| {
                anyhow::anyhow!("open postgres transition intent transaction: {error}")
            })?;
            for intent in &intents {
                crate::engine::transition_executor_pg::execute_pg_transition_intent(
                    &mut tx, intent,
                )
                .await
                .map_err(anyhow::Error::msg)?;
            }
            tx.commit()
                .await
                .map_err(|error| anyhow::anyhow!("commit postgres transition intents: {error}"))?;
            Ok(())
        },
        anyhow::Error::msg,
    )
}

fn execute_transition_intent_pg(
    state: &AppState,
    intent: &crate::engine::transition::TransitionIntent,
) -> anyhow::Result<()> {
    execute_transition_intents_pg(state, std::slice::from_ref(intent))
}

fn review_state_sync_pg(state: &AppState, payload: serde_json::Value) -> anyhow::Result<String> {
    let pool = state.pg_pool_ref().ok_or_else(pg_pool_required_anyhow)?;
    let result =
        crate::engine::ops::review_state_sync_with_backends(Some(pool), &payload.to_string());
    let parsed = serde_json::from_str::<serde_json::Value>(&result).unwrap_or_else(|_| {
        json!({
            "error": format!("invalid review_state_sync response: {result}")
        })
    });
    if let Some(error) = parsed.get("error").and_then(|value| value.as_str()) {
        return Err(anyhow::anyhow!("{error}"));
    }
    Ok(result)
}

// ── Handlers ───────────────────────────────────────────────────

/// GET /api/kanban-cards
pub async fn list_cards(
    State(state): State<AppState>,
    Query(params): Query<ListCardsQuery>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let service = crate::services::kanban::KanbanService::new(state.pg_pool);
    match service
        .list_cards(crate::services::kanban::ListCardsInput {
            status: params.status,
            repo_id: params.repo_id,
            assigned_agent_id: params.assigned_agent_id,
        })
        .await
    {
        Ok(response) => Ok((StatusCode::OK, Json(json!({"cards": response.cards})))),
        Err(error) => Err(error),
    }
}

/// GET /api/kanban-cards/:id
pub async fn get_card(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    if let Some(pool) = state.pg_pool_ref() {
        return match load_card_json_pg(pool, &id).await {
            Ok(Some(card)) => Ok((StatusCode::OK, Json(json!({"card": card})))),
            Ok(None) => Err(AppError::not_found("card not found")),
            Err(error) => Err(database_error(error)),
        };
    }

    Err(pg_pool_required_error())
}

/// POST /api/kanban-cards
pub async fn create_card(
    State(state): State<AppState>,
    Json(body): Json<CreateCardBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let id = uuid::Uuid::new_v4().to_string();
    let priority = body.priority.unwrap_or_else(|| "medium".to_string());

    if let Some(pool) = state.pg_pool_ref() {
        crate::pipeline::ensure_loaded();
        let initial_state = crate::pipeline::get().initial_state().to_string();

        if let Err(error) = kanban_db::insert_card_pg(
            pool,
            &id,
            body.repo_id.as_deref(),
            &body.title,
            &initial_state,
            &priority,
            body.github_issue_url.as_deref(),
        )
        .await
        {
            return Err(database_error(error));
        }

        return match load_card_json_pg(pool, &id).await {
            Ok(Some(card)) => {
                crate::server::ws::emit_event(
                    &state.broadcast_tx,
                    "kanban_card_created",
                    card.clone(),
                );
                Ok((StatusCode::CREATED, Json(json!({"card": card}))))
            }
            Ok(None) => Err(database_error("failed to read card after create")),
            Err(error) => Err(database_error(error)),
        };
    }

    Err(pg_pool_required_error())
}

/// PATCH /api/kanban-cards/:id
pub async fn update_card(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateCardBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let Some(pool) = state.pg_pool_ref() else {
        return Err(pg_pool_required_error());
    };

    let old_status = match kanban_db::card_status_pg(pool, &id).await {
        Ok(Some(status)) => status,
        Ok(None) => {
            return Err(AppError::not_found("card not found"));
        }
        Err(error) => {
            return Err(database_error(error));
        }
    };

    let has_non_status_updates = match validate_update_card_fields(&body) {
        Ok(has_non_status_updates) => has_non_status_updates,
        Err(error) => {
            return Err(AppError::bad_request(error));
        }
    };

    let new_status = body.status.clone();

    if let Some(new_s) = &new_status {
        if new_s.as_str() != old_status {
            if !is_allowed_manual_transition(&old_status, new_s) {
                return Err(AppError::bad_request(format!(
                    "PATCH /api/kanban-cards/{{id}} only allows manual status transitions backlog -> ready and any -> backlog (requested: {} -> {}). Use POST /api/kanban-cards/{{id}}/transition for administrative force transitions, or POST /api/kanban-cards/{{id}}/rereview for review reruns.",
                    old_status, new_s,
                )));
            }

            let transition_result = if new_s == "backlog" {
                transition_card_to_backlog_with_cleanup(&state, &id, "api:manual-backlog").await
            } else {
                crate::kanban::transition_status_with_opts_pg_only(
                    pool,
                    &state.engine,
                    &id,
                    new_s,
                    "api",
                    crate::engine::transition::ForceIntent::None,
                )
                .await
            };

            match transition_result {
                Ok(_) => {}
                Err(e) => {
                    return Err(AppError::bad_request(format!("{e}")));
                }
            }
        }
    }

    // ── Non-status field updates (only after status transition succeeds) ──
    if has_non_status_updates {
        let metadata_json = body
            .metadata
            .as_ref()
            .map(|metadata| serde_json::to_string(metadata).unwrap_or_default())
            .or(body.metadata_json.clone());

        match kanban_db::update_card_fields_pg(
            pool,
            &id,
            &kanban_db::UpdateCardFields {
                title: body.title.clone(),
                priority: body.priority.clone(),
                assigned_agent_id: body.assignee_agent_id.clone(),
                repo_id: body.repo_id.clone(),
                github_issue_url: body.github_issue_url.clone(),
                description: body.description.clone(),
                metadata_json,
                review_status: body.review_status.clone(),
                review_notes: body.review_notes.clone(),
            },
        )
        .await
        {
            Ok(false) => {
                return Err(AppError::not_found("card not found"));
            }
            Ok(true) => {}
            Err(error) => {
                return Err(database_error(error));
            }
        }
    }

    // #108: Drain pending intents from hooks fired during transition_status_with_opts.
    // fire_dynamic_hooks fires policy hooks that may create dispatch intents, but
    // doesn't drain them itself. drain_hook_side_effects now also queues Discord
    // notifications for created dispatches, replacing the previous latest_dispatch_id
    // re-query that was susceptible to race conditions.
    crate::kanban::drain_hook_side_effects_with_backends(&state.engine);

    match load_card_json_pg(pool, &id).await {
        Ok(Some(card)) => {
            crate::server::ws::emit_event(&state.broadcast_tx, "kanban_card_updated", card.clone());
            Ok((StatusCode::OK, Json(json!({"card": card}))))
        }
        Ok(None) => Err(database_error("failed to read card after update")),
        Err(error) => Err(database_error(error)),
    }
}

/// POST /api/kanban-cards/:id/assign
pub async fn assign_card(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<AssignCardBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    if let Some(pool) = state.pg_pool_ref() {
        let old_status = match kanban_db::card_status_pg(pool, &id).await {
            Ok(Some(status)) => status,
            Ok(None) => {
                return Err(AppError::not_found("card not found"));
            }
            Err(error) => {
                return Err(database_error(error));
            }
        };

        match kanban_db::assign_card_agent_pg(pool, &id, &body.agent_id).await {
            Ok(false) => {
                return Err(AppError::not_found("card not found"));
            }
            Ok(true) => {}
            Err(error) => {
                return Err(database_error(error));
            }
        }

        let transition =
            assign_transition_to_dispatchable_pg(pool, &state.engine, &id, &old_status, "assign")
                .await;

        return match load_card_json_pg(pool, &id).await {
            Ok(Some(card)) => {
                crate::server::ws::emit_event(
                    &state.broadcast_tx,
                    "kanban_card_updated",
                    card.clone(),
                );
                Ok((
                    StatusCode::OK,
                    Json(json!({
                        "card": card,
                        "assignment": {"ok": true, "agent_id": body.agent_id},
                        "transition": transition,
                    })),
                ))
            }
            Ok(None) => Err(database_error("failed to read card after assign")),
            Err(error) => Err(database_error(error)),
        };
    }

    Err(pg_pool_required_error())
}

/// DELETE /api/kanban-cards/:id
pub async fn delete_card(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    if let Some(pool) = state.pg_pool_ref() {
        return match kanban_db::delete_card_pg(pool, &id).await {
            Ok(false) => Err(AppError::not_found("card not found")),
            Ok(true) => {
                crate::server::ws::emit_event(
                    &state.broadcast_tx,
                    "kanban_card_deleted",
                    json!({"id": id}),
                );
                Ok((StatusCode::OK, Json(json!({"ok": true}))))
            }
            Err(error) => Err(database_error(error)),
        };
    }

    Err(pg_pool_required_error())
}

/// POST /api/kanban-cards/:id/retry
///
/// This endpoint is single-call complete. Do NOT chain /transition or
/// /queue/generate after it — that creates duplicate dispatches
/// (see #1442 incident). Inspect `new_dispatch_id` and `next_action` in
/// the response to confirm the new dispatch was created. See
/// `/api/docs/card-lifecycle-ops` for the full decision tree (#1443).
pub async fn retry_card(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<RetryCardBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let Some(pool) = state.pg_pool_ref() else {
        return Err(pg_pool_required_error());
    };
    let retry_spec = match kanban_db::load_retry_dispatch_spec_pg(pool, &id).await {
        Ok(Some(spec)) => spec,
        Ok(None) => {
            return Err(AppError::not_found("card not found"));
        }
        Err(error) => {
            return Err(database_error(error));
        }
    };

    let existing_dispatch_id = match kanban_db::latest_dispatch_id_for_card_pg(pool, &id).await {
        Ok(dispatch_id) => dispatch_id,
        Err(error) => {
            return Err(database_error(error));
        }
    };
    // #1442 (codex P2): only report `cancelled_dispatch_id` when the cancel
    // call actually transitioned a `pending`/`dispatched` row to `cancelled`.
    // The cancel helper returns `Ok(0)` for stale/already-terminal rows
    // (e.g. failed/completed); the typical retry case must not falsely
    // claim a cancellation that did not happen.
    let mut cancelled_dispatch_id: Option<String> = None;
    if let Some(prev_dispatch_id) = existing_dispatch_id.as_deref() {
        match crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_pg(
            pool,
            prev_dispatch_id,
            None,
        )
        .await
        {
            Ok(changed) => {
                if changed > 0 {
                    cancelled_dispatch_id = Some(prev_dispatch_id.to_string());
                }
            }
            Err(error) => {
                return Err(database_error(format!("{error}")));
            }
        }
    }

    use crate::engine::transition::TransitionIntent as TI2;
    let agent_id_for_dispatch = if let Some(agent_id) = body.assignee_agent_id.as_deref() {
        if let Err(error) = kanban_db::assign_card_agent_pg(pool, &id, agent_id).await {
            return Err(database_error(error));
        }
        agent_id.to_string()
    } else {
        match kanban_db::assigned_agent_id_for_card_pg(pool, &id).await {
            Ok(agent_id) => agent_id.unwrap_or_default(),
            Err(error) => {
                return Err(database_error(error));
            }
        }
    };

    if let Err(error) = execute_transition_intent_pg(
        &state,
        &TI2::SetLatestDispatchId {
            card_id: id.clone(),
            dispatch_id: None,
        },
    ) {
        return Err(database_error(format!("{error}")));
    }

    let dispatch_agent_id = if agent_id_for_dispatch.is_empty() {
        retry_spec.agent_id
    } else {
        agent_id_for_dispatch
    };
    let retry_dispatch_type = retry_spec.dispatch_type;
    let retry_title = retry_spec.title;
    let mut new_dispatch_id: Option<String> = None;
    let mut next_action = "none_required".to_string();
    if !dispatch_agent_id.is_empty() {
        match crate::dispatch::create_dispatch_pg_only(
            pool,
            &state.engine,
            &id,
            &dispatch_agent_id,
            &retry_dispatch_type,
            &retry_title,
            &json!({"retry": true, "preserved_dispatch_type": retry_dispatch_type.clone()}),
        ) {
            Ok(dispatch) => {
                // Codex P2: `create_dispatch_pg_only` can return an existing
                // active dispatch tagged `__reused` instead of inserting a
                // new row when a duplicate active dispatch already exists.
                // Surface that explicitly so callers don't believe a fresh
                // retry dispatch was issued.
                let reused = dispatch
                    .get("__reused")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false);
                if reused {
                    next_action = "duplicate_active_dispatch_detected_inspect_card".to_string();
                } else {
                    new_dispatch_id = dispatch
                        .get("id")
                        .and_then(|value| value.as_str())
                        .map(|value| value.to_string());
                }
            }
            Err(error) => {
                return Err(database_error(format!("{error}")));
            }
        }
    } else {
        // No agent assigned — caller must assign an agent before a dispatch can be created.
        next_action = "assign_agent_then_call_retry".to_string();
    }

    match load_card_json_pg(pool, &id).await {
        Ok(Some(card)) => {
            crate::server::ws::emit_event(&state.broadcast_tx, "kanban_card_updated", card.clone());
            Ok((
                StatusCode::OK,
                Json(json!({
                    "card": card,
                    "new_dispatch_id": new_dispatch_id,
                    "cancelled_dispatch_id": cancelled_dispatch_id,
                    "next_action": next_action,
                })),
            ))
        }
        Ok(None) => Err(AppError::not_found("card not found")),
        Err(error) => Err(database_error(error)),
    }
}

/// POST /api/kanban-cards/:id/redispatch
///
/// This endpoint is single-call complete. Do NOT chain /transition or
/// /queue/generate after it — that creates duplicate dispatches
/// (see #1442 incident). Inspect `new_dispatch_id` and `next_action` in
/// the response to confirm the new dispatch was created. See
/// `/api/docs/card-lifecycle-ops` for the full decision tree (#1443).
pub async fn redispatch_card(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(_body): Json<RedispatchCardBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let Some(pool) = state.pg_pool_ref() else {
        return Err(pg_pool_required_error());
    };

    let spec = match kanban_db::load_retry_dispatch_spec_pg(pool, &id).await {
        Ok(Some(spec)) => spec,
        Ok(None) => {
            return Err(AppError::not_found("card not found"));
        }
        Err(error) => {
            return Err(database_error(error));
        }
    };
    let agent_id = spec.agent_id;
    let dispatch_type = spec.dispatch_type;
    let dispatch_title = spec.title;

    let dispatch_id = match kanban_db::latest_dispatch_id_for_card_pg(pool, &id).await {
        Ok(dispatch_id) => dispatch_id,
        Err(error) => {
            return Err(database_error(error));
        }
    };
    // #1442 (codex P2): only report `cancelled_dispatch_id` when the cancel
    // call actually transitioned a `pending`/`dispatched` row to `cancelled`.
    // The cancel helper returns `Ok(0)` for stale/already-terminal rows, and
    // claiming a cancellation that did not happen would defeat the
    // single-call confirmation contract.
    let mut cancelled_dispatch_id: Option<String> = None;
    if let Some(prev_dispatch_id) = dispatch_id.as_deref() {
        match crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_pg(
            pool,
            prev_dispatch_id,
            None,
        )
        .await
        {
            Ok(changed) => {
                if changed > 0 {
                    cancelled_dispatch_id = Some(prev_dispatch_id.to_string());
                }
            }
            Err(error) => {
                return Err(database_error(format!("{error}")));
            }
        }
    }

    use crate::engine::transition::TransitionIntent;
    for intent in [
        TransitionIntent::SetReviewStatus {
            card_id: id.clone(),
            review_status: None,
        },
        TransitionIntent::SetLatestDispatchId {
            card_id: id.clone(),
            dispatch_id: None,
        },
        TransitionIntent::SyncReviewState {
            card_id: id.clone(),
            state: "idle".to_string(),
        },
    ] {
        if let Err(error) = execute_transition_intent_pg(&state, &intent) {
            return Err(database_error(format!("{error}")));
        }
    }

    let mut new_dispatch_id: Option<String> = None;
    let mut next_action = "none_required".to_string();
    if !agent_id.is_empty() {
        match crate::dispatch::create_dispatch_pg_only(
            pool,
            &state.engine,
            &id,
            &agent_id,
            &dispatch_type,
            &dispatch_title,
            &json!({"redispatch": true, "preserved_dispatch_type": dispatch_type.clone()}),
        ) {
            Ok(dispatch) => {
                // Codex P2: `create_dispatch_pg_only` can return an existing
                // active dispatch tagged `__reused` instead of inserting a
                // new row when a duplicate active dispatch already exists.
                // Don't claim that as `new_dispatch_id` — that would falsely
                // confirm a fresh dispatch was created and mask the
                // duplicate-state problem the caller is trying to solve.
                let reused = dispatch
                    .get("__reused")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false);
                if reused {
                    next_action = "duplicate_active_dispatch_detected_inspect_card".to_string();
                } else {
                    new_dispatch_id = dispatch
                        .get("id")
                        .and_then(|value| value.as_str())
                        .map(|value| value.to_string());
                }
            }
            Err(error) => {
                return Err(database_error(format!("{error}")));
            }
        }
    } else {
        // No agent assigned — caller must assign an agent before a dispatch can be created.
        next_action = "assign_agent_then_call_redispatch".to_string();
    }

    match load_card_json_pg(pool, &id).await {
        Ok(Some(card)) => {
            crate::server::ws::emit_event(&state.broadcast_tx, "kanban_card_updated", card.clone());
            Ok((
                StatusCode::OK,
                Json(json!({
                    "card": card,
                    "new_dispatch_id": new_dispatch_id,
                    "cancelled_dispatch_id": cancelled_dispatch_id,
                    "next_action": next_action,
                })),
            ))
        }
        Ok(None) => Err(AppError::not_found("card not found")),
        Err(error) => Err(database_error(error)),
    }
}

/// PATCH /api/kanban-cards/:id/defer-dod
pub async fn defer_dod(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<DeferDodBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let Some(pool) = state.pg_pool_ref() else {
        return Err(pg_pool_required_error());
    };

    let row = match kanban_db::load_dod_state_pg(pool, &id).await {
        Ok(row) => row,
        Err(error) => {
            return Err(database_error(error));
        }
    };

    let Some(dod_state) = row else {
        return Err(AppError::not_found("card not found"));
    };
    let current = dod_state.deferred_dod_json;
    let card_status = dod_state.status;
    let review_status = dod_state.review_status;

    let dod = apply_deferred_dod_changes(current, body);
    let dod_str = serde_json::to_string(&dod).unwrap_or_default();
    if let Err(error) = kanban_db::update_deferred_dod_pg(pool, &id, &dod_str).await {
        return Err(database_error(error));
    }

    let is_review_state = {
        crate::pipeline::ensure_loaded();
        crate::pipeline::try_get()
            .and_then(|p| p.hooks_for_state(&card_status))
            .is_some_and(|h| h.on_enter.iter().any(|n| n == "OnReviewEnter"))
    };
    let all_done = if let (Some(items), Some(verified)) =
        (dod["items"].as_array(), dod["verified"].as_array())
    {
        !items.is_empty() && items.iter().all(|item| verified.contains(item))
    } else {
        false
    };
    let restart_review_state =
        is_review_state && review_status.as_deref() == Some("awaiting_dod") && all_done;

    if restart_review_state {
        use crate::engine::transition::TransitionIntent;
        for intent in [
            TransitionIntent::SetReviewStatus {
                card_id: id.clone(),
                review_status: Some("reviewing".to_string()),
            },
            TransitionIntent::SyncReviewState {
                card_id: id.clone(),
                state: "reviewing".to_string(),
            },
        ] {
            if let Err(error) = execute_transition_intent_pg(&state, &intent) {
                return Err(database_error(format!("{error}")));
            }
        }
        if let Err(error) = kanban_db::update_review_clock_after_dod_pg(pool, &id).await {
            return Err(database_error(error));
        }
    }

    if restart_review_state {
        crate::kanban::fire_enter_hooks_with_backends(&state.engine, &id, &card_status);
        tracing::info!(
            "[dod] Card {} DoD all-complete — restarting review from awaiting_dod",
            id
        );
    }

    match load_card_json_pg(pool, &id).await {
        Ok(Some(mut card)) => {
            card["deferred_dod"] = dod;
            Ok((StatusCode::OK, Json(json!({"card": card}))))
        }
        Ok(None) => Err(AppError::not_found("card not found")),
        Err(error) => Err(database_error(error)),
    }
}

/// GET /api/kanban-cards/:id/review-state
/// #117: Returns the canonical card_review_state record for a card.
pub async fn get_card_review_state(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let Some(pool) = state.pg_pool_ref() else {
        return Err(pg_pool_required_error());
    };

    match kanban_db::load_card_review_state_json_pg(pool, &id).await {
        Ok(Some(state_json)) => Ok((StatusCode::OK, Json(state_json))),
        Ok(None) => Err(AppError::not_found("no review state for this card")),
        Err(error) => Err(database_error(error)),
    }
}

/// GET /api/kanban-cards/:id/reviews
pub async fn list_card_reviews(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let Some(pool) = state.pg_pool_ref() else {
        return Err(pg_pool_required_error());
    };

    match kanban_db::list_card_reviews_json_pg(pool, &id).await {
        Ok(reviews) => Ok((StatusCode::OK, Json(json!({"reviews": reviews})))),
        Err(error) => Err(database_error(error)),
    }
}

/// GET /api/kanban-cards/stalled
pub async fn stalled_cards(
    State(state): State<AppState>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let Some(pool) = state.pg_pool_ref() else {
        return Err(pg_pool_required_error());
    };

    let ids = match kanban_db::stalled_card_ids_pg(pool).await {
        Ok(ids) => ids,
        Err(error) => {
            return Err(database_error(error));
        }
    };

    let mut cards = Vec::with_capacity(ids.len());
    for id in ids {
        match load_card_json_pg(pool, &id).await {
            Ok(Some(card)) => cards.push(card),
            Ok(None) => {}
            Err(error) => {
                return Err(database_error(error));
            }
        }
    }

    Ok((StatusCode::OK, Json(json!(cards))))
}

/// POST /api/kanban-cards/assign-issue
pub async fn assign_issue(
    State(state): State<AppState>,
    Json(body): Json<AssignIssueBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    if let Some(pool) = state.pg_pool_ref() {
        let upserted = match upsert_card_from_issue_pg(
            pool,
            IssueCardUpsert {
                repo_id: body.github_repo.clone(),
                issue_number: body.github_issue_number,
                issue_url: body.github_issue_url.clone(),
                title: body.title.clone(),
                description: body.description.clone(),
                priority: None,
                assigned_agent_id: Some(body.assignee_agent_id.clone()),
                metadata_json: None,
                status_on_create: Some("backlog".to_string()),
            },
        )
        .await
        {
            Ok(result) => result,
            Err(error) => {
                return Err(database_error(error));
            }
        };

        let old_status = if upserted.created {
            "backlog".to_string()
        } else {
            match kanban_db::card_status_pg(pool, &upserted.card_id).await {
                Ok(Some(status)) => status,
                Ok(None) => {
                    return Err(database_error("failed to reload card after upsert"));
                }
                Err(error) => {
                    return Err(database_error(error));
                }
            }
        };

        let transition = assign_transition_to_dispatchable_pg(
            pool,
            &state.engine,
            &upserted.card_id,
            &old_status,
            "assign_issue",
        )
        .await;

        return match load_card_json_pg(pool, &upserted.card_id).await {
            Ok(Some(card)) => {
                let event_name = if upserted.created {
                    "kanban_card_created"
                } else {
                    "kanban_card_updated"
                };
                crate::server::ws::emit_event(&state.broadcast_tx, event_name, card.clone());
                Ok((
                    if upserted.created {
                        StatusCode::CREATED
                    } else {
                        StatusCode::OK
                    },
                    Json(json!({
                        "card": card,
                        "deduplicated": !upserted.created,
                        "assignment": {"ok": true, "agent_id": body.assignee_agent_id},
                        "transition": transition,
                    })),
                ))
            }
            Ok(None) => Err(database_error("failed to read card after assign")),
            Err(error) => Err(database_error(error)),
        };
    }

    Err(pg_pool_required_error())
}

// ── Helpers ────────────────────────────────────────────────────

async fn assign_transition_to_dispatchable_pg(
    pool: &sqlx::PgPool,
    engine: &crate::engine::PolicyEngine,
    card_id: &str,
    old_status: &str,
    source: &str,
) -> serde_json::Value {
    crate::pipeline::ensure_loaded();
    let pipeline = crate::pipeline::get();
    let ready_state = pipeline
        .dispatchable_states()
        .into_iter()
        .next()
        .map(|state| state.to_string())
        .unwrap_or_else(|| {
            tracing::warn!("Pipeline has no dispatchable states, using initial state");
            pipeline.initial_state().to_string()
        });

    if old_status == ready_state {
        return json!({
            "attempted": false,
            "ok": true,
            "from": old_status,
            "to": old_status,
            "target": ready_state,
            "target_status": ready_state,
            "next_action": "none_required",
            "error": null,
            "steps": [],
            "completed_steps": [],
        });
    }

    let steps = pipeline
        .free_path_to_dispatchable(old_status)
        .filter(|path| !path.is_empty())
        .unwrap_or_else(|| vec![ready_state.clone()]);
    let mut completed_steps = Vec::new();
    let mut current_status = old_status.to_string();

    for step in &steps {
        match crate::kanban::transition_status_with_opts_pg_only(
            pool,
            engine,
            card_id,
            step,
            source,
            crate::engine::transition::ForceIntent::None,
        )
        .await
        {
            Ok(result) => {
                current_status = result.to.clone();
                completed_steps.push(json!({
                    "from": result.from,
                    "to": result.to,
                    "changed": result.changed,
                }));
            }
            Err(error) => {
                let error_message = format!("{error}");
                tracing::warn!(
                    "[{source}] postgres assign transition step to '{step}' failed: {error_message}"
                );
                return json!({
                    "attempted": true,
                    "ok": false,
                    "from": old_status,
                    "to": current_status,
                    "target": ready_state,
                    "target_status": ready_state,
                    "next_action": "inspect_transition_error",
                    "steps": steps,
                    "completed_steps": completed_steps,
                    "failed_step": step,
                    "error": error_message,
                });
            }
        }
    }

    json!({
        "attempted": true,
        "ok": true,
        "from": old_status,
        "to": current_status,
        "target": ready_state,
        "target_status": ready_state,
        "next_action": "none_required",
        "error": null,
        "steps": steps,
        "completed_steps": completed_steps,
    })
}

// reason: compatibility wrapper keeping `kanban_db::card_row_to_json`
// reachable from disabled DB callers; PG paths build card JSON directly.
// See #3034 / #3035.

pub(super) async fn load_card_json_pg(
    pool: &sqlx::PgPool,
    card_id: &str,
) -> Result<Option<serde_json::Value>, String> {
    kanban_db::load_card_json_pg(pool, card_id).await
}

// ── Audit Log API ────────────────────────────────────────────

/// GET /api/kanban-cards/:id/audit-log
pub async fn card_audit_log(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let Some(pool) = state.pg_pool_ref() else {
        return Err(pg_pool_required_error());
    };

    let logs = match kanban_db::list_card_audit_logs_json_pg(pool, &id).await {
        Ok(logs) => logs,
        Err(error) => {
            return Err(database_error(error));
        }
    };

    Ok((StatusCode::OK, Json(json!({"logs": logs}))))
}

/// GET /api/kanban-cards/:id/comments
/// Fetch GitHub comments for the linked issue via `gh` CLI.
pub async fn card_github_comments(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let Some(pool) = state.pg_pool_ref() else {
        return Err(pg_pool_required_error());
    };

    let issue_ref = match kanban_db::card_github_issue_ref_pg(pool, &id).await {
        Ok(Some(issue_ref)) => issue_ref,
        Ok(None) => {
            return Err(AppError::not_found("card not found"));
        }
        Err(error) => {
            return Err(database_error(error));
        }
    };

    let repo = match issue_ref.repo_id {
        Some(r) => r,
        None => return Ok((StatusCode::OK, Json(json!({"comments": []})))),
    };
    let number = match issue_ref.issue_number {
        Some(n) => n,
        None => return Ok((StatusCode::OK, Json(json!({"comments": []})))),
    };

    let result =
        tokio::task::spawn_blocking(move || crate::github::fetch_issue_comments(&repo, number))
            .await;

    match result {
        Ok(Ok(issue)) => {
            let comments = serde_json::to_value(issue.comments).unwrap_or_else(|_| json!([]));
            let body = issue.body.unwrap_or_default();

            if let Err(error) =
                kanban_db::update_card_description_if_changed_pg(pool, &id, &body).await
            {
                return Err(database_error(error));
            }

            Ok((
                StatusCode::OK,
                Json(json!({"comments": comments, "body": body})),
            ))
        }
        Ok(Err(e)) => Err(kanban_error(
            StatusCode::BAD_GATEWAY,
            format!("gh issue view failed: {e}"),
        )),
        Err(e) => Err(kanban_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("join: {e}"),
        )),
    }
}

// ── PM Decision API ──────────────────────────────────────────

/// POST /api/pm-decision
/// PM's decision on a manual-intervention card.
/// - resume: return card to in_progress (continue work)
/// - rework: create rework dispatch to assigned agent
/// - dismiss: move card to done (PM decides work is sufficient)
/// - requeue: move card back to ready for re-prioritization
pub async fn pm_decision(
    State(state): State<AppState>,
    Json(body): Json<PmDecisionBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let Some(transition_pool) = state.pg_pool_ref() else {
        return Err(pg_pool_required_error());
    };

    let valid = ["resume", "rework", "dismiss", "requeue"];
    if !valid.contains(&body.decision.as_str()) {
        return Err(AppError::bad_request(format!(
            "decision must be one of: {}",
            valid.join(", ")
        )));
    }

    // Verify card exists and currently requires manual intervention.
    let card_info =
        match kanban_db::load_pm_decision_card_info_pg(transition_pool, &body.card_id).await {
            Ok(row) => row,
            Err(error) => {
                return Err(database_error(error));
            }
        };

    let Some(card_info) = card_info else {
        return Err(AppError::not_found("card not found"));
    };
    let status = card_info.status;
    let review_status = card_info.review_status;
    let blocked_reason = card_info.blocked_reason;
    let agent_id = card_info.agent_id;
    let title = card_info.title;

    let manual_fingerprint = crate::manual_intervention::manual_intervention_fingerprint(
        review_status.as_deref(),
        blocked_reason.as_deref(),
    );
    let legacy_manual_state = matches!(status.as_str(), "pending_decision" | "blocked");
    if !legacy_manual_state && manual_fingerprint.is_none() {
        return Err(AppError::bad_request(format!(
            "card is '{}', which does not currently require manual decision",
            status
        )));
    }

    // Complete any pending pm-decision dispatches (rework handles its own completion after dispatch success)
    if body.decision != "rework" {
        let completion_result = json!({"decision": body.decision, "comment": body.comment});
        let pending_dispatch_ids =
            match kanban_db::pending_pm_decision_dispatch_ids_pg(transition_pool, &body.card_id)
                .await
            {
                Ok(ids) => ids,
                Err(error) => {
                    return Err(database_error(error));
                }
            };
        for dispatch_id in pending_dispatch_ids {
            crate::dispatch::set_dispatch_status_with_backends(
                Some(transition_pool),
                &dispatch_id,
                "completed",
                Some(&completion_result),
                "mark_dispatch_completed",
                Some(&["pending", "dispatched"]),
                true,
            )
            .ok();
        }
    }
    // Clear manual-intervention markers before applying the selected decision.
    if let Err(error) =
        kanban_db::clear_manual_intervention_marker_pg(transition_pool, &body.card_id).await
    {
        return Err(database_error(error));
    }
    if legacy_manual_state || review_status.as_deref() == Some("dilemma_pending") {
        execute_transition_intent_pg(
            &state,
            &crate::engine::transition::TransitionIntent::SetReviewStatus {
                card_id: body.card_id.clone(),
                review_status: None,
            },
        )
        .ok();
        execute_transition_intent_pg(
            &state,
            &crate::engine::transition::TransitionIntent::SyncReviewState {
                card_id: body.card_id.clone(),
                state: "idle".to_string(),
            },
        )
        .ok();
    }

    let message = match body.decision.as_str() {
        "resume" => {
            // Guard: resume requires a live dispatch + working session.
            // Without one the card would be stranded in in_progress with nothing driving it.
            let has_live =
                match kanban_db::has_live_dispatch_session_pg(transition_pool, &body.card_id).await
                {
                    Ok(has_live) => has_live,
                    Err(error) => {
                        return Err(database_error(error));
                    }
                };
            if !has_live {
                return Err(AppError::conflict(
                    "cannot resume: no live dispatch/session for this card. Use 'rework' or 'requeue' instead.",
                ));
            }
            // Pipeline-driven: resume to first dispatchable state
            crate::pipeline::ensure_loaded();
            let pipeline = crate::pipeline::get();
            let resume_target = pipeline
                .dispatchable_states()
                .into_iter()
                .next()
                .map(|s| s.to_string())
                .unwrap_or_else(|| {
                    tracing::warn!("Pipeline has no dispatchable states, using initial state");
                    pipeline.initial_state().to_string()
                });
            if let Err(e) = crate::kanban::transition_status_with_opts_pg_only(
                transition_pool,
                &state.engine,
                &body.card_id,
                &resume_target,
                "pm-decision",
                crate::engine::transition::ForceIntent::OperatorOverride,
            )
            .await
            {
                return Err(database_error(format!("resume transition failed: {e}")));
            }
            "Card resumed"
        }
        "rework" => {
            if agent_id.is_empty() {
                return Err(AppError::bad_request(
                    "card has no assigned agent for rework",
                ));
            }
            // Try dispatch creation FIRST — only transition on success
            match crate::dispatch::create_dispatch_pg_only(
                transition_pool,
                &state.engine,
                &body.card_id,
                &agent_id,
                "rework",
                &format!("[Rework] {}", title),
                &json!({"pm_decision": "rework", "comment": body.comment}),
            ) {
                Ok(_) => {
                    // Dispatch succeeded — now complete pm-decision dispatch + transition
                    let completion_result = json!({"decision": "rework", "comment": body.comment});
                    let pending_dispatch_ids = match kanban_db::pending_pm_decision_dispatch_ids_pg(
                        transition_pool,
                        &body.card_id,
                    )
                    .await
                    {
                        Ok(ids) => ids,
                        Err(error) => {
                            return Err(database_error(error));
                        }
                    };
                    for dispatch_id in pending_dispatch_ids {
                        crate::dispatch::set_dispatch_status_with_backends(
                            Some(transition_pool),
                            &dispatch_id,
                            "completed",
                            Some(&completion_result),
                            "mark_dispatch_completed",
                            Some(&["pending", "dispatched"]),
                            true,
                        )
                        .ok();
                    }
                    // Pipeline-driven: rework target from current state's review_rework gate
                    let rework_status: String =
                        match kanban_db::card_status_pg(transition_pool, &body.card_id).await {
                            Ok(status) => status.unwrap_or_default(),
                            Err(error) => {
                                return Err(database_error(format!("load rework status: {error}")));
                            }
                        };
                    let pipeline = crate::pipeline::get();
                    let rework_target = pipeline
                        .transitions
                        .iter()
                        .find(|t| {
                            t.from == rework_status
                                && t.transition_type
                                    == crate::pipeline::TransitionType::Gated
                                && t.gates.iter().any(|g| g == "review_rework")
                        })
                        .map(|t| t.to.clone())
                        .unwrap_or_else(|| {
                            tracing::warn!("No rework transition found from '{}', using first dispatchable state", rework_status);
                            pipeline.dispatchable_states().first().map(|s| s.to_string())
                                .unwrap_or_else(|| pipeline.initial_state().to_string())
                        });
                    if let Err(e) = crate::kanban::transition_status_with_opts_pg_only(
                        transition_pool,
                        &state.engine,
                        &body.card_id,
                        &rework_target,
                        "pm-decision",
                        crate::engine::transition::ForceIntent::OperatorOverride,
                    )
                    .await
                    {
                        return Err(database_error(format!("rework transition failed: {e}")));
                    }
                    // #155: Use intent for review_status mutation.
                    execute_transition_intent_pg(
                        &state,
                        &crate::engine::transition::TransitionIntent::SetReviewStatus {
                            card_id: body.card_id.clone(),
                            review_status: Some("rework_pending".to_string()),
                        },
                    )
                    .ok();
                    // #117/#158: sync canonical review state via unified entrypoint.
                    review_state_sync_pg(
                        &state,
                        serde_json::json!({"card_id": body.card_id.clone(), "state": "rework_pending", "last_decision": "pm_rework"}),
                    )
                    .ok();
                    "Rework dispatch created"
                }
                Err(e) => {
                    return Err(database_error(format!("rework dispatch failed: {e}")));
                }
            }
        }
        "dismiss" => {
            // Pipeline-driven: dismiss to terminal state
            let pipeline = crate::pipeline::get();
            let terminal = pipeline
                .states
                .iter()
                .find(|s| s.terminal)
                .map(|s| s.id.as_str())
                .expect("Pipeline must have at least one terminal state");
            if let Err(e) = crate::kanban::transition_status_with_opts_pg_only(
                transition_pool,
                &state.engine,
                &body.card_id,
                terminal,
                "pm-decision",
                crate::engine::transition::ForceIntent::OperatorOverride,
            )
            .await
            {
                return Err(database_error(format!("dismiss transition failed: {e}")));
            }
            "Card dismissed"
        }
        "requeue" => {
            // Pipeline-driven: requeue to first dispatchable state
            let pipeline = crate::pipeline::get();
            let requeue_target = pipeline
                .dispatchable_states()
                .into_iter()
                .next()
                .unwrap_or_else(|| {
                    tracing::warn!("Pipeline has no dispatchable states, using initial state");
                    pipeline.initial_state()
                });
            if let Err(e) = crate::kanban::transition_status_with_opts_pg_only(
                transition_pool,
                &state.engine,
                &body.card_id,
                requeue_target,
                "pm-decision",
                crate::engine::transition::ForceIntent::OperatorOverride,
            )
            .await
            {
                return Err(database_error(format!("requeue transition failed: {e}")));
            }
            "Card requeued"
        }
        _ => "Unknown decision",
    };

    // Emit kanban_card_updated for the affected card
    if let Ok(Some(card)) = load_card_json_pg(transition_pool, &body.card_id).await {
        crate::server::ws::emit_event(&state.broadcast_tx, "kanban_card_updated", card);
    }

    Ok((
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "card_id": body.card_id,
            "decision": body.decision,
            "message": message,
        })),
    ))
}

// ── Administrative review recovery helpers ───────────────────────
//
// `require_explicit_bearer_token` / `resolve_requesting_agent_id_with_pg` were
// relocated to `crate::services::kanban` (#3037 service→server backflow). Routes
// below call them through the services facade.
use crate::services::kanban::{require_explicit_bearer_token, resolve_requesting_agent_id_with_pg};

fn request_principal_ref(
    principal: &Option<Extension<RequestPrincipal>>,
) -> Option<&RequestPrincipal> {
    principal.as_ref().map(|Extension(principal)| principal)
}

fn log_kanban_identity_consumption(
    endpoint: &'static str,
    headers: &HeaderMap,
    principal: &Option<Extension<RequestPrincipal>>,
    consumed_agent_id: &str,
) {
    let config = crate::config::load_graceful();
    log_identity_consumption(
        endpoint,
        request_principal_ref(principal),
        Some(consumed_agent_id),
        manager_channel_check_relied_on_claimed_header(
            headers,
            config.kanban.manager_channel_id.as_deref(),
        ),
    );
}

/// POST /api/kanban-cards/:id/rereview
///
/// Recovery endpoint. Forces a card back through counter-model review
/// using the best available execution target for that card's implementation.
pub async fn rereview_card(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    principal: Option<Extension<RequestPrincipal>>,
    Json(body): Json<RereviewBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    if let Err(response) = require_explicit_bearer_token(&headers, "rereview") {
        return Err(tuple_error(response));
    }

    let Some(pool) = state.pg_pool_ref() else {
        return Err(pg_pool_required_error());
    };
    let reason = body.reason.as_deref().unwrap_or("manual rereview");
    let card_info = match kanban_db::load_rereview_card_info_pg(pool, &id).await {
        Ok(Some(values)) => values,
        Ok(None) => {
            return Err(AppError::not_found(format!("card not found: {id}")));
        }
        Err(error) => {
            tracing::warn!(
                card_id = %id,
                %error,
                "[rereview] postgres lookup failed"
            );
            return Err(database_error(error));
        }
    };
    let current_status = card_info.status;
    let assigned_agent_id = card_info.assigned_agent_id;
    let card_title = card_info.title;
    let gh_url = card_info.github_issue_url;
    let caller_source = resolve_requesting_agent_id_with_pg(pool, &headers)
        .await
        .unwrap_or_else(|| "api".to_string());
    log_kanban_identity_consumption(
        "POST /api/kanban-cards/{id}/rereview",
        &headers,
        &principal,
        &caller_source,
    );

    let assigned_agent_id = match assigned_agent_id.filter(|value| !value.is_empty()) {
        Some(value) => value,
        None => {
            return Err(AppError::conflict("card has no assigned agent"));
        }
    };

    let stale_ids = match kanban_db::stale_review_dispatch_ids_pg(pool, &id).await {
        Ok(ids) => ids,
        Err(error) => {
            return Err(database_error(error));
        }
    };

    for stale_id in &stale_ids {
        if let Err(error) = crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_pg(
            pool,
            stale_id,
            Some("superseded_by_rereview"),
        )
        .await
        {
            return Err(database_error(format!("{error}")));
        }
    }

    if let Err(error) = kanban_db::cleanup_rereview_card_pg(pool, &id).await {
        return Err(database_error(error));
    }

    let sync_result = review_state_sync_pg(
        &state,
        json!({
            "card_id": id,
            "state": "idle",
        }),
    );
    if let Err(error) = sync_result {
        tracing::warn!("[kanban] rereview review_state_sync cleanup failed: {error}");
    }

    if let Err(error) = kanban_db::reset_repeated_finding_rounds_pg(pool, &id).await {
        tracing::warn!("[kanban] rereview repeated-finding postgres reset failed: {error}");
    }

    let transitioned_into_review = current_status != "review";

    if transitioned_into_review {
        if let Err(e) = crate::kanban::transition_status_with_opts_pg_only(
            pool,
            &state.engine,
            &id,
            "review",
            &format!("{caller_source}:rereview({reason})"),
            crate::engine::transition::ForceIntent::OperatorOverride,
        )
        .await
        {
            return Err(database_error(format!("{e}")));
        }
    } else {
        crate::kanban::fire_enter_hooks_with_backends(&state.engine, &id, "review");
    }

    let mut review_dispatch_id = kanban_db::find_active_review_dispatch_id_pg(pool, &id).await;

    if review_dispatch_id.is_none() && !transitioned_into_review {
        let _ = state
            .engine
            .fire_hook_by_name_blocking("OnReviewEnter", json!({ "card_id": id }));
        crate::kanban::drain_hook_side_effects_with_backends(&state.engine);
        review_dispatch_id = kanban_db::find_active_review_dispatch_id_pg(pool, &id).await;
    }

    if review_dispatch_id.is_none() {
        let dispatch_result = crate::dispatch::create_dispatch_pg_only(
            pool,
            &state.engine,
            &id,
            &assigned_agent_id,
            "review",
            &card_title,
            &json!({ "rereview": true, "reason": reason }),
        );
        match dispatch_result {
            Ok(dispatch) => {
                review_dispatch_id = dispatch["id"].as_str().map(str::to_string);
            }
            Err(e) => {
                return Err(database_error(format!("{e}")));
            }
        }
    }

    let Some(review_dispatch_id) = review_dispatch_id else {
        return Err(database_error("failed to create fresh review dispatch"));
    };

    crate::kanban::correct_tn_to_fn_on_reopen(state.pg_pool_ref(), &id);

    if let Err(error) = kanban_db::reset_completed_at_pg(pool, &id).await {
        return Err(database_error(error));
    }

    let entry_ids = match kanban_db::active_auto_queue_entry_ids_for_rereview_pg(pool, &id).await {
        Ok(ids) => ids,
        Err(error) => {
            return Err(database_error(error));
        }
    };

    for entry_id in entry_ids {
        if let Err(error) = kanban_db::move_auto_queue_entry_to_dispatched_on_pg(
            pool,
            &entry_id,
            "rereview_dispatch",
            &crate::db::auto_queue::EntryStatusUpdateOptions {
                dispatch_id: Some(review_dispatch_id.clone()),
                slot_index: None,
            },
        )
        .await
        {
            tracing::warn!(
                card_id = %id,
                %error,
                "[rereview] postgres auto-queue entry update failed"
            );
        }
    }

    let card = match load_card_json_pg(pool, &id).await {
        Ok(Some(card)) => card,
        Ok(None) => {
            return Err(AppError::not_found(format!("card not found: {id}")));
        }
        Err(error) => {
            return Err(database_error(format!("{error}")));
        }
    };

    if !crate::pipeline::get().is_terminal("review")
        && crate::pipeline::get().is_terminal(&current_status)
    {
        if let Some(url) = gh_url.as_deref() {
            if let Err(e) = crate::github::reopen_issue_by_url(url).await {
                tracing::warn!("[kanban] Failed to reopen GitHub issue {url}: {e}");
                return Ok((
                    StatusCode::BAD_GATEWAY,
                    Json(json!({
                        "error": format!("github issue reopen failed before rereview response: {e}"),
                        "rereviewed": false,
                        "github_issue_url": url,
                    })),
                ));
            }
        }
    }

    crate::server::ws::emit_event(&state.broadcast_tx, "kanban_card_updated", card.clone());
    Ok((
        StatusCode::OK,
        Json(json!({
            "card": card,
            "rereviewed": true,
            "review_dispatch_id": review_dispatch_id,
            "reason": reason,
        })),
    ))
}

/// POST /api/kanban-cards/batch-rereview (formerly /api/re-review, removed in #1064)
///
/// Batch endpoint. Accepts a list of GitHub issue numbers,
/// looks up each card, and calls the rereview logic for each.
/// Per-item error handling: one failure does not stop others.
pub async fn batch_rereview(
    State(state): State<AppState>,
    headers: HeaderMap,
    principal: Option<Extension<RequestPrincipal>>,
    Json(body): Json<BatchRereviewBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    if let Err(response) = require_explicit_bearer_token(&headers, "batch rereview") {
        return Err(tuple_error(response));
    }

    let reason = body.reason.clone();
    let Some(pool) = state.pg_pool_ref() else {
        return Err(pg_pool_required_error());
    };
    let mut results = Vec::new();

    for issue_number in &body.issues {
        let card_id = match kanban_db::card_id_by_issue_number_pg(pool, *issue_number).await {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(
                    %issue_number,
                    %error,
                    "[batch_rereview] postgres lookup failed"
                );
                results.push(json!({
                    "issue": issue_number,
                    "ok": false,
                    "error": error,
                }));
                continue;
            }
        };

        let card_id = match card_id {
            Some(id) => id,
            None => {
                results.push(json!({
                    "issue": issue_number,
                    "ok": false,
                    "error": format!("card not found for issue #{issue_number}"),
                }));
                continue;
            }
        };

        let rereview_body = RereviewBody {
            reason: reason.clone(),
        };

        let (status, Json(response)) = match rereview_card(
            State(state.clone()),
            Path(card_id),
            headers.clone(),
            principal.clone(),
            Json(rereview_body),
        )
        .await
        {
            Ok(response) => response,
            Err(error) => error.into_json_response(),
        };

        if status == StatusCode::OK {
            results.push(json!({
                "issue": issue_number,
                "ok": true,
                "dispatch_id": response.get("review_dispatch_id"),
            }));
        } else {
            results.push(json!({
                "issue": issue_number,
                "ok": false,
                "error": response.get("error"),
            }));
        }
    }

    Ok((StatusCode::OK, Json(json!({ "results": results }))))
}

/// POST /api/kanban-cards/:id/reopen
///
/// Administrative endpoint. Reopens a done card by transitioning to in_progress,
/// clearing completed_at, and optionally resetting recovery fields.
/// Same explicit Bearer auth as force-transition.
pub async fn reopen_card(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    principal: Option<Extension<RequestPrincipal>>,
    Json(body): Json<ReopenBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let reset_full = body.reset_full.unwrap_or(false);

    if let Err(response) = require_explicit_bearer_token(&headers, "reopen") {
        return Err(tuple_error(response));
    }

    let Some(pool) = state.pg_pool_ref() else {
        return Err(pg_pool_required_error());
    };
    let caller_source = resolve_requesting_agent_id_with_pg(pool, &headers)
        .await
        .unwrap_or_else(|| "api".to_string());
    log_kanban_identity_consumption(
        "POST /api/kanban-cards/{id}/reopen",
        &headers,
        &principal,
        &caller_source,
    );

    // ── Pre-check: card must be in done state ──
    let current_status: String = match kanban_db::card_status_pg(pool, &id).await {
        Ok(Some(status)) => status,
        Ok(None) => {
            return Err(AppError::not_found(format!("card not found: {id}")));
        }
        Err(error) => {
            return Err(database_error(error));
        }
    };

    // Pipeline-driven: reopen only applies to terminal states
    crate::pipeline::ensure_loaded();
    let pipeline = crate::pipeline::get();
    let is_terminal = pipeline.is_terminal(&current_status);
    if !is_terminal {
        return Err(AppError::bad_request(format!(
            "card is not terminal (current: {current_status}), reopen only applies to terminal cards"
        )));
    }

    // Determine reopen target: first dispatchable state that has gated outbound
    let reopen_target = pipeline
        .dispatchable_states()
        .into_iter()
        .next()
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            tracing::warn!("Pipeline has no dispatchable states, using initial state");
            pipeline.initial_state().to_string()
        });
    let reason = body.reason.as_deref().unwrap_or("reopen via API");

    if let Some(pool) = state.pg_pool_ref() {
        if let Err(error) = kanban_db::mark_api_reopen_skip_preflight_on_pg(pool, &id).await {
            return Err(database_error(format!(
                "failed to stage API reopen preflight skip: {error}"
            )));
        }

        let transition_result = if reset_full {
            crate::kanban::transition_status_with_opts_and_allowed_cleanup_pg_only(
                pool,
                &state.engine,
                &id,
                &reopen_target,
                &format!("{caller_source}:reopen({reason})"),
                crate::engine::transition::ForceIntent::OperatorOverride,
                crate::kanban::AllowedOnConnMutation::ForceTransitionRevertCleanup,
            )
            .await
            .map(|(result, counts)| {
                (
                    result.from,
                    result.to,
                    (
                        counts.cancelled_dispatches,
                        counts.skipped_auto_queue_entries,
                    ),
                )
            })
        } else {
            crate::kanban::transition_status_with_opts_pg_only(
                pool,
                &state.engine,
                &id,
                &reopen_target,
                &format!("{caller_source}:reopen({reason})"),
                crate::engine::transition::ForceIntent::OperatorOverride,
            )
            .await
            .map(|result| (result.from, result.to, (0, 0)))
        };

        match transition_result {
            Ok((from_status, to_status, cleanup_counts)) => {
                crate::kanban::correct_tn_to_fn_on_reopen(state.pg_pool_ref(), &id);

                if reset_full {
                    if let Err(error) = kanban_db::clear_all_threads_pg(pool, &id).await {
                        return Err(database_error(format!("{error}")));
                    }
                    if let Err(error) =
                        kanban_db::clear_reopen_preflight_cache_on_pg(pool, &id).await
                    {
                        return Err(database_error(format!(
                            "failed to clear reopen cache: {error}"
                        )));
                    }
                } else if let Err(error) =
                    kanban_db::consume_api_reopen_preflight_skip_on_pg(pool, &id).await
                {
                    return Err(database_error(format!(
                        "failed to persist API reopen preflight skip: {error}"
                    )));
                }

                if let Err(error) = kanban_db::reset_completed_at_pg(pool, &id).await {
                    return Err(database_error(error));
                }

                if let Some(ref rs) = body.review_status
                    && let Err(error) = kanban_db::update_card_review_status_pg(pool, &id, rs).await
                {
                    return Err(database_error(error));
                }

                if let Err(error) =
                    kanban_db::reactivate_done_auto_queue_entries_pg(pool, &id).await
                {
                    return Err(database_error(format!("{error}")));
                }

                let gh_url = match kanban_db::github_issue_url_for_card_pg(pool, &id).await {
                    Ok(value) => value,
                    Err(error) => {
                        return Err(database_error(error));
                    }
                };

                let card = load_card_json_pg(pool, &id).await;

                if let Some(url) = gh_url.as_deref() {
                    if let Err(error) = crate::github::reopen_issue_by_url(url).await {
                        tracing::warn!("[kanban] Failed to reopen GitHub issue {url}: {error}");
                        return Ok((
                            StatusCode::BAD_GATEWAY,
                            Json(json!({
                                "error": format!("github issue reopen failed before reopen response: {error}"),
                                "reopened": false,
                                "github_issue_url": url,
                            })),
                        ));
                    }
                }

                return match card {
                    Ok(Some(card)) => {
                        crate::server::ws::emit_event(
                            &state.broadcast_tx,
                            "kanban_card_updated",
                            card.clone(),
                        );
                        Ok((
                            StatusCode::OK,
                            Json(json!({
                                "card": card,
                                "reopened": true,
                                "reset_full": reset_full,
                                "cancelled_dispatches": cleanup_counts.0,
                                "skipped_auto_queue_entries": cleanup_counts.1,
                                "from": from_status,
                                "to": to_status,
                                "reason": reason,
                            })),
                        ))
                    }
                    Ok(None) => Err(database_error("failed to read card after reopen")),
                    Err(error) => Err(database_error(error)),
                };
            }
            Err(error) => {
                let _ = kanban_db::clear_api_reopen_skip_preflight_on_pg(pool, &id).await;
                return Err(database_error(format!("{error}")));
            }
        }
    }

    Err(pg_pool_required_error())
}

// ── Administrative force transition ──────────────────────────────

fn force_transition_needs_cleanup(target_status: &str, cancel_dispatches: Option<bool>) -> bool {
    matches!(target_status, "backlog" | "ready") && cancel_dispatches.unwrap_or(true)
}

/// #1444: returns true if the caller has explicitly opted into cancelling an
/// existing active dispatch on a `target=ready` transition. Either the new
/// `force` flag or the legacy `cancel_dispatches=true` field qualifies.
fn force_transition_force_intent_present(body: &ForceTransitionBody) -> bool {
    body.force.unwrap_or(false) || body.cancel_dispatches.unwrap_or(false)
}

// reason: compatibility wrappers retained so the disabled DB path keeps a
// single call surface mirroring the `_pg` route paths; the PG production paths
// call `kanban_db::*` directly, so these read as dead in the default lib build.
// They keep the `db::kanban_cards` `_on_conn` test helpers reachable from one
// place. See #3034 / #3035.

/// POST /api/kanban-cards/:id/force-transition
///
/// Administrative endpoint. Bypasses dispatch validation.
/// Requires an explicit Bearer token (no same-origin bypass).
///
/// This endpoint is single-call complete. Do NOT chain /redispatch,
/// /retry, or /queue/generate after it — that creates duplicate
/// dispatches (see #1442 incident). Inspect `cancelled_dispatch_ids`,
/// `created_dispatch_id`, and `next_action_hint` in the response to
/// determine whether any follow-up is genuinely required. See
/// `/api/docs/card-lifecycle-ops` for the full decision tree (#1443).
pub async fn force_transition(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    principal: Option<Extension<RequestPrincipal>>,
    Json(body): Json<ForceTransitionBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    if let Err(response) = require_explicit_bearer_token(&headers, "force-transition") {
        return Err(tuple_error(response));
    }

    // #1444 codex iter-2 P2: `force=true` is documented as opting into the
    // "cancel + transition" recovery, so it must drive the cleanup decision
    // alongside the legacy `cancel_dispatches` flag. Otherwise a caller
    // sending `{"status":"ready","force":true,"cancel_dispatches":false}`
    // would bypass the 409 guard but skip the cleanup, leaving the active
    // dispatch in place — the exact regression the guard exists to prevent.
    let force_intent_present = force_transition_force_intent_present(&body);
    let cleanup_opt_in = force_intent_present || body.cancel_dispatches.unwrap_or(true);
    let needs_cleanup = force_transition_needs_cleanup(&body.status, Some(cleanup_opt_in));
    let target_status = body.status;
    let mut cleanup_counts = (0, 0);
    let pool = match state.pg_pool_ref() {
        Some(pool) => pool,
        None => {
            return Err(AppError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorCode::Database,
                "force-transition requires postgres pool (#1239)",
            ));
        }
    };
    let caller_source = resolve_requesting_agent_id_with_pg(pool, &headers)
        .await
        .unwrap_or_else(|| "api".to_string());
    log_kanban_identity_consumption(
        "POST /api/kanban-cards/{id}/transition",
        &headers,
        &principal,
        &caller_source,
    );
    // Snapshot active dispatch IDs before transition so we can report which
    // were cancelled by the cleanup paths (#1442). The cleanup helpers report
    // counts but not IDs; we reconcile by querying the post-transition status
    // of each pre-existing active dispatch and surfacing the ones now in
    // `cancelled` state.
    let pre_active_dispatch_ids: Vec<String> =
        kanban_db::active_dispatch_ids_for_card_pg(pool, &id)
            .await
            .unwrap_or_default();

    // #1444 idempotency guard: when the caller asks to transition a card to
    // `ready` while it already has a pending/dispatched dispatch, refuse with
    // 409 unless `force=true` (or legacy `cancel_dispatches=true`) is
    // explicitly set. This stops the #1442 incident pattern where a caller
    // chains `/redispatch` + `/transition` + `/queue/generate` and
    // accidentally creates duplicate dispatches.
    if target_status == "ready" && !force_intent_present && !pre_active_dispatch_ids.is_empty() {
        let active_id = pre_active_dispatch_ids.first().cloned().unwrap_or_default();
        return Ok((
            StatusCode::CONFLICT,
            Json(json!({
                "error": format!(
                    "card has active dispatch {active_id}; pass force=true to cancel and re-transition"
                ),
                "active_dispatch_id": active_id,
                "active_dispatch_ids": pre_active_dispatch_ids,
                "next_action_hint": "card already has a live dispatch — inspect /api/dispatches/{id}; pass force=true (or legacy cancel_dispatches=true) on /transition to cancel + re-transition",
            })),
        ));
    }
    let pre_latest_dispatch_id: Option<String> =
        match kanban_db::latest_dispatch_id_for_card_pg(pool, &id).await {
            Ok(value) => value,
            Err(error) => {
                return Err(database_error(error));
            }
        };
    let terminal_cleanup = match kanban_db::load_card_pipeline_context_pg(pool, &id).await {
        Ok(Some(card_context)) => {
            match crate::kanban::resolve_pipeline_with_pg(
                pool,
                card_context.repo_id.as_deref(),
                card_context.assigned_agent_id.as_deref(),
            )
            .await
            {
                Ok(effective) => {
                    // #1444 codex iter-2 P2: same `force || cancel_dispatches`
                    // unification as the ready/backlog cleanup decision so
                    // `force=true` drives terminal cleanup too.
                    effective.is_terminal(&target_status) && cleanup_opt_in
                }
                Err(error) => {
                    return Err(database_error(format!("{error}")));
                }
            }
        }
        Ok(None) => false,
        Err(error) => {
            return Err(database_error(error));
        }
    };

    let transition_result = if needs_cleanup {
        crate::kanban::transition_status_with_opts_and_allowed_cleanup_pg_only(
            pool,
            &state.engine,
            &id,
            &target_status,
            &caller_source,
            crate::engine::transition::ForceIntent::OperatorOverride,
            crate::kanban::AllowedOnConnMutation::ForceTransitionRevertCleanup,
        )
        .await
        .map(|(result, counts)| {
            cleanup_counts = (
                counts.cancelled_dispatches,
                counts.skipped_auto_queue_entries,
            );
            result
        })
    } else if terminal_cleanup {
        crate::kanban::transition_status_with_opts_and_allowed_cleanup_pg_only(
            pool,
            &state.engine,
            &id,
            &target_status,
            &caller_source,
            crate::engine::transition::ForceIntent::OperatorOverride,
            crate::kanban::AllowedOnConnMutation::ForceTransitionTerminalCleanup,
        )
        .await
        .map(|(result, counts)| {
            cleanup_counts = (
                counts.cancelled_dispatches,
                counts.skipped_auto_queue_entries,
            );
            result
        })
    } else {
        crate::kanban::transition_status_with_opts_pg_only(
            pool,
            &state.engine,
            &id,
            &target_status,
            &caller_source,
            crate::engine::transition::ForceIntent::OperatorOverride,
        )
        .await
    };

    match transition_result {
        Ok(result) => {
            let (mut cancelled_dispatches, mut skipped_auto_queue_entries) = cleanup_counts;
            crate::kanban::drain_hook_side_effects_with_backends(&state.engine);

            // #1444 codex iter-2 P1: when target_status equals current status
            // (transition is a NoOp at the FSM level) AND the caller forced
            // the call to clear an active dispatch, the cleanup helpers
            // never ran because the FSM short-circuited. Run the SAME
            // `ForceTransitionRevertCleanup` cleanup the normal ready
            // transition path applies — cancel dispatches AND skip live
            // auto-queue entries AND clear `latest_dispatch_id`/review
            // fields on the card AND clear stale session bindings — so
            // the documented force-recovery actually leaves the card in a
            // clean state. (Iter-1 used the generic per-dispatch cancel
            // which RESET queue entries to pending, leaving them eligible
            // for redispatch and the dispatch pointer stale on the card.)
            if !result.changed
                && force_intent_present
                && target_status == "ready"
                && !pre_active_dispatch_ids.is_empty()
            {
                match crate::kanban::force_transition_revert_cleanup_pg_only(
                    pool,
                    &id,
                    &target_status,
                )
                .await
                {
                    Ok(noop_counts) => {
                        cancelled_dispatches += noop_counts.cancelled_dispatches;
                        skipped_auto_queue_entries += noop_counts.skipped_auto_queue_entries;
                    }
                    Err(error) => {
                        return Err(database_error(format!(
                            "force-transition no-op cleanup failed for card {id}: {error}"
                        )));
                    }
                }
            }

            // Reconcile the pre-transition active dispatch snapshot against
            // current state to surface concrete cancelled IDs (#1442).
            let cancelled_dispatch_ids =
                if cancelled_dispatches > 0 && !pre_active_dispatch_ids.is_empty() {
                    kanban_db::cancelled_dispatch_ids_among_pg(pool, &pre_active_dispatch_ids)
                        .await
                        .unwrap_or_default()
                } else {
                    Vec::new()
                };

            // Detect a brand-new dispatch that may have been kicked off by
            // hooks fired during the transition (e.g. on_enter).
            let post_latest_dispatch_id: Option<String> =
                kanban_db::latest_dispatch_id_for_card_pg(pool, &id)
                    .await
                    .unwrap_or_default();
            let created_dispatch_id = match (
                pre_latest_dispatch_id.as_deref(),
                post_latest_dispatch_id.as_deref(),
            ) {
                (Some(prev), Some(curr)) if prev != curr => Some(curr.to_string()),
                (None, Some(curr)) => Some(curr.to_string()),
                _ => None,
            };

            // Only suggest /queue/generate when the target state is
            // actually enqueueable by that endpoint AND no live dispatch
            // remains for the card (codex P2). Suggesting generate while a
            // pending/dispatched dispatch is still in flight queues a card
            // that's already running — the original #1442 failure mode.
            // Cleanup only runs for `backlog`/`ready` (or with
            // cancel_dispatches=true on terminal targets), so callers can
            // also reach `requested`/`ready` while a live dispatch persists.
            let post_active_dispatch_count = kanban_db::active_dispatch_ids_for_card_pg(pool, &id)
                .await
                .map(|ids| ids.len())
                .unwrap_or(0);
            let next_action_hint = if created_dispatch_id.is_some() {
                "none_required".to_string()
            } else if matches!(result.to.as_str(), "ready" | "requested")
                && post_active_dispatch_count == 0
            {
                // Caller may want to dispatch the now-ready card via the
                // queue. The transition itself is complete; this hint surfaces
                // the natural follow-up so callers do not silently chain
                // /redispatch (which would create duplicates — #1442).
                "call /api/queue/generate to dispatch newly-ready card".to_string()
            } else {
                "none_required".to_string()
            };

            let card = load_card_json_pg(pool, &id)
                .await
                .map_err(|error| format!("{error}"))
                .and_then(|card| {
                    card.ok_or_else(|| "card not found after force-transition".to_string())
                });
            match card {
                Ok(c) => {
                    crate::server::ws::emit_event(
                        &state.broadcast_tx,
                        "kanban_card_updated",
                        c.clone(),
                    );
                    Ok((
                        StatusCode::OK,
                        Json(json!({
                            "card": c,
                            "forced": true,
                            "from": result.from,
                            "to": result.to,
                            "cancelled_dispatches": cancelled_dispatches,
                            "cancelled_dispatch_ids": cancelled_dispatch_ids,
                            "created_dispatch_id": created_dispatch_id,
                            "next_action_hint": next_action_hint,
                            "skipped_auto_queue_entries": skipped_auto_queue_entries
                        })),
                    ))
                }
                Err(e) => Err(database_error(format!("{e}"))),
            }
        }
        Err(e) => Err(AppError::bad_request(format!("{e}"))),
    }
}

#[cfg(test)]
mod update_card_validation_tests {
    use super::{MIXED_STATUS_FIELD_UPDATE_ERROR, UpdateCardBody, validate_update_card_fields};

    #[test]
    fn update_card_validation_rejects_status_with_metadata_json() {
        let body: UpdateCardBody =
            serde_json::from_str(r#"{"status":"ready","metadata_json":"{}"}"#)
                .expect("payload should deserialize");

        let error = validate_update_card_fields(&body).expect_err("mixed update must be rejected");

        assert_eq!(error, MIXED_STATUS_FIELD_UPDATE_ERROR);
    }

    #[test]
    fn update_card_validation_allows_status_only_or_valid_metadata_only() {
        let status_only: UpdateCardBody =
            serde_json::from_str(r#"{"status":"ready"}"#).expect("payload should deserialize");
        assert_eq!(validate_update_card_fields(&status_only), Ok(false));

        // #1690: a metadata_only payload with valid JSON must still pass validation.
        let metadata_only: UpdateCardBody =
            serde_json::from_str(r#"{"metadata_json":"{\"k\":\"v\"}"}"#)
                .expect("payload should deserialize");
        assert_eq!(validate_update_card_fields(&metadata_only), Ok(true));
    }

    #[test]
    fn update_card_validation_rejects_invalid_metadata_json() {
        // #1690: invalid metadata_json must be rejected at the route layer
        // (HTTP 400) instead of forwarding to update_card_fields_pg, which
        // would return HTTP 500 from a Postgres ::jsonb cast error.
        let body: UpdateCardBody = serde_json::from_str(r#"{"metadata_json":"not-json"}"#)
            .expect("payload should deserialize");

        let error =
            validate_update_card_fields(&body).expect_err("invalid metadata_json must be rejected");

        assert!(
            error.starts_with("invalid metadata_json:"),
            "error must call out invalid metadata_json, got: {error}"
        );
    }

    #[test]
    fn update_card_validation_accepts_valid_metadata_json_object() {
        // #1690: valid JSON object metadata_json must still pass.
        let body: UpdateCardBody = serde_json::from_str(r#"{"metadata_json":"{\"k\":\"v\"}"}"#)
            .expect("payload should deserialize");
        assert_eq!(validate_update_card_fields(&body), Ok(true));
    }

    #[test]
    fn update_card_validation_accepts_valid_metadata_json_scalar() {
        // serde_json::from_str accepts any valid JSON value (including bare
        // strings like `"foo"`); guard that the validator does not falsely
        // reject scalar JSON values.
        let body: UpdateCardBody = serde_json::from_str(r#"{"metadata_json":"\"foo\""}"#)
            .expect("payload should deserialize");
        assert_eq!(validate_update_card_fields(&body), Ok(true));
    }
}

// ── #1065 param standardization tests ────────────────────────────────
// UpdateCardBody canonical field is `assignee_agent_id` (snake_case).
// Legacy `assigned_agent_id` still accepted via serde alias during migration.
