use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
};
use poise::serenity_prelude::ChannelId;
use serde::Deserialize;
use serde_json::json;
use sqlx::Row as SqlxRow;

use super::AppState;
use crate::db::kanban::{IssueCardUpsert, upsert_card_from_issue_pg};
use crate::services::provider::ProviderKind;
use crate::services::turn_lifecycle::{TurnLifecycleTarget, force_kill_turn};

/// Common kanban card SELECT columns with dispatch metadata via LEFT JOIN.
pub(super) const CARD_SELECT: &str = "SELECT kc.id, kc.repo_id, kc.title, kc.status, kc.priority, kc.assigned_agent_id, \
    kc.github_issue_url, kc.github_issue_number, kc.latest_dispatch_id, kc.review_round, kc.metadata, \
    kc.created_at, kc.updated_at, \
    td.status AS d_status, td.dispatch_type AS d_type, td.title AS d_title, td.chain_depth AS d_depth, \
    td.result AS d_result, td.context AS d_context, \
    kc.description, kc.blocked_reason, kc.review_notes, kc.review_status, \
    kc.started_at, kc.requested_at, kc.completed_at, kc.pipeline_stage_id, \
    kc.owner_agent_id, kc.requester_agent_id, kc.parent_card_id, kc.sort_order, kc.depth, kc.review_entered_at \
    FROM kanban_cards kc LEFT JOIN task_dispatches td ON td.id = kc.latest_dispatch_id";

/// Latest meaningful activity for in-progress stall detection.
///
/// We intentionally consider the newest of:
/// - latest dispatch creation time (fresh dispatch / redispatch)
/// - card.updated_at (manual or pipeline-driven re-entry to in_progress)
/// - started_at fallback for legacy rows
pub(crate) const STALLED_ACTIVITY_AT_SQL: &str =
    "MAX(COALESCE(td.created_at, ''), COALESCE(kc.updated_at, ''), COALESCE(kc.started_at, ''))";

// ── Query / Body types ─────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ListCardsQuery {
    pub status: Option<String>,
    pub repo_id: Option<String>,
    pub assigned_agent_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateCardBody {
    pub title: String,
    pub repo_id: Option<String>,
    pub priority: Option<String>,
    pub github_issue_url: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateCardBody {
    pub title: Option<String>,
    pub status: Option<String>,
    pub priority: Option<String>,
    /// Canonical: `assignee_agent_id`.
    /// Legacy `assigned_agent_id` still accepted via serde alias during migration (#1065).
    #[serde(default, alias = "assigned_agent_id")]
    pub assignee_agent_id: Option<String>,
    pub repo_id: Option<String>,
    pub github_issue_url: Option<String>,
    pub metadata: Option<serde_json::Value>,
    pub description: Option<String>,
    pub metadata_json: Option<String>,
    pub review_status: Option<String>,
    pub review_notes: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AssignCardBody {
    pub agent_id: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct RetryCardBody {
    pub assignee_agent_id: Option<String>,
    pub request_now: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct RedispatchCardBody {
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DeferDodBody {
    pub items: Option<Vec<String>>,
    pub verify: Option<Vec<String>>,
    pub unverify: Option<Vec<String>>,
    pub remove: Option<Vec<String>>,
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

#[derive(Debug, Deserialize)]
pub struct BulkActionBody {
    pub action: String,
    pub card_ids: Vec<String>,
    /// Target status for "transition" action (e.g. "ready", "backlog").
    pub target_status: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AssignIssueBody {
    pub github_repo: String,
    pub github_issue_number: i64,
    pub github_issue_url: Option<String>,
    pub title: String,
    pub description: Option<String>,
    pub assignee_agent_id: String,
}

#[derive(Debug, Clone)]
struct ActiveTurnTarget {
    session_key: String,
    provider: Option<String>,
    thread_channel_id: Option<String>,
}

fn is_allowed_manual_transition(from: &str, to: &str) -> bool {
    (from == "backlog" && to == "ready") || (from != to && to == "backlog")
}

async fn load_active_turn_targets_for_card_pg(
    pool: &sqlx::PgPool,
    card_id: &str,
) -> anyhow::Result<Vec<ActiveTurnTarget>> {
    let rows = sqlx::query(
        "SELECT DISTINCT session_key, provider, thread_channel_id
         FROM sessions
         WHERE active_dispatch_id IN (
             SELECT id
             FROM task_dispatches
             WHERE kanban_card_id = $1
               AND status IN ('pending', 'dispatched')
         )",
    )
    .bind(card_id)
    .fetch_all(pool)
    .await
    .map_err(|error| anyhow::anyhow!("load postgres active turn targets for {card_id}: {error}"))?;

    rows.into_iter()
        .map(|row| {
            Ok(ActiveTurnTarget {
                session_key: row.try_get("session_key").map_err(|error| {
                    anyhow::anyhow!("decode session_key for {card_id}: {error}")
                })?,
                provider: row
                    .try_get("provider")
                    .map_err(|error| anyhow::anyhow!("decode provider for {card_id}: {error}"))?,
                thread_channel_id: row.try_get("thread_channel_id").map_err(|error| {
                    anyhow::anyhow!("decode thread_channel_id for {card_id}: {error}")
                })?,
            })
        })
        .collect()
}

async fn cancel_turn_targets(state: &AppState, targets: &[ActiveTurnTarget], reason: &str) {
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
            sqlx::query(
                "UPDATE sessions
                 SET status = 'disconnected',
                     active_dispatch_id = NULL,
                     claude_session_id = NULL
                 WHERE session_key = $1",
            )
            .bind(&target.session_key)
            .execute(pool)
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
    let turn_targets = load_active_turn_targets_for_card_pg(pool, card_id).await?;
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

fn pg_pool_required_error() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"error": "postgres backend required for kanban transition (#1384)"})),
    )
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
        crate::engine::ops::review_state_sync_with_backends(None, Some(pool), &payload.to_string());
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

async fn load_retry_dispatch_spec_pg(
    pool: &sqlx::PgPool,
    card_id: &str,
) -> Result<Option<(String, String, String)>, sqlx::Error> {
    let Some((card_agent_id, card_title, latest_dispatch_id)) =
        sqlx::query_as::<_, (Option<String>, String, Option<String>)>(
            "SELECT assigned_agent_id, title, latest_dispatch_id
             FROM kanban_cards
             WHERE id = $1",
        )
        .bind(card_id)
        .fetch_optional(pool)
        .await?
    else {
        return Ok(None);
    };

    let latest_dispatch = if let Some(dispatch_id) = latest_dispatch_id.as_deref() {
        sqlx::query_as::<_, (Option<String>, Option<String>, Option<String>)>(
            "SELECT to_agent_id, dispatch_type, title
             FROM task_dispatches
             WHERE id = $1",
        )
        .bind(dispatch_id)
        .fetch_optional(pool)
        .await?
    } else {
        None
    };
    let latest_dispatch = match latest_dispatch {
        Some(row) => Some(row),
        None => {
            sqlx::query_as::<_, (Option<String>, Option<String>, Option<String>)>(
                "SELECT to_agent_id, dispatch_type, title
             FROM task_dispatches
             WHERE kanban_card_id = $1
             ORDER BY created_at DESC, id DESC
             LIMIT 1",
            )
            .bind(card_id)
            .fetch_optional(pool)
            .await?
        }
    };

    let (dispatch_agent_id, dispatch_type, dispatch_title) =
        latest_dispatch.unwrap_or((None, None, None));

    let effective_agent_id = dispatch_agent_id.or(card_agent_id).unwrap_or_default();
    let effective_dispatch_type = dispatch_type
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "implementation".to_string());
    let effective_title = dispatch_title
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(card_title);

    Ok(Some((
        effective_agent_id,
        effective_dispatch_type,
        effective_title,
    )))
}

// ── Handlers ───────────────────────────────────────────────────

/// GET /api/kanban-cards
pub async fn list_cards(
    State(state): State<AppState>,
    Query(params): Query<ListCardsQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let service = crate::services::kanban::KanbanService::new(state.pg_pool);
    match service
        .list_cards(crate::services::kanban::ListCardsInput {
            status: params.status,
            repo_id: params.repo_id,
            assigned_agent_id: params.assigned_agent_id,
        })
        .await
    {
        Ok(response) => (StatusCode::OK, Json(json!({"cards": response.cards}))),
        Err(error) => error.into_json_response(),
    }
}

/// GET /api/kanban-cards/:id
pub async fn get_card(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(pool) = state.pg_pool_ref() {
        return match load_card_json_pg(pool, &id).await {
            Ok(Some(card)) => (StatusCode::OK, Json(json!({"card": card}))),
            Ok(None) => (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "card not found"})),
            ),
            Err(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            ),
        };
    }

    pg_pool_required_error()
}

/// POST /api/kanban-cards
pub async fn create_card(
    State(state): State<AppState>,
    Json(body): Json<CreateCardBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let id = uuid::Uuid::new_v4().to_string();
    let priority = body.priority.unwrap_or_else(|| "medium".to_string());

    if let Some(pool) = state.pg_pool_ref() {
        crate::pipeline::ensure_loaded();
        let initial_state = crate::pipeline::get().initial_state().to_string();

        let result = sqlx::query(
            "INSERT INTO kanban_cards (
                id,
                repo_id,
                title,
                status,
                priority,
                github_issue_url,
                created_at,
                updated_at
             ) VALUES ($1, $2, $3, $4, $5, $6, NOW(), NOW())",
        )
        .bind(&id)
        .bind(&body.repo_id)
        .bind(&body.title)
        .bind(&initial_state)
        .bind(&priority)
        .bind(&body.github_issue_url)
        .execute(pool)
        .await;

        if let Err(error) = result {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{error}")})),
            );
        }

        return match load_card_json_pg(pool, &id).await {
            Ok(Some(card)) => {
                crate::server::ws::emit_event(
                    &state.broadcast_tx,
                    "kanban_card_created",
                    card.clone(),
                );
                (StatusCode::CREATED, Json(json!({"card": card})))
            }
            Ok(None) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "failed to read card after create"})),
            ),
            Err(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            ),
        };
    }

    pg_pool_required_error()
}

/// PATCH /api/kanban-cards/:id
pub async fn update_card(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateCardBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_pool_required_error();
    };

    let old_status = match sqlx::query_scalar::<_, String>(
        "SELECT status FROM kanban_cards WHERE id = $1 LIMIT 1",
    )
    .bind(&id)
    .fetch_optional(pool)
    .await
    {
        Ok(Some(status)) => status,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "card not found"})),
            );
        }
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{error}")})),
            );
        }
    };

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
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "no fields to update"})),
        );
    }

    let new_status = body.status.clone();

    if let Some(new_s) = &new_status {
        if new_s.as_str() != old_status {
            if !is_allowed_manual_transition(&old_status, new_s) {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "error": format!(
                            "PATCH /api/kanban-cards/{{id}} only allows manual status transitions backlog -> ready and any -> backlog (requested: {} -> {}). Use POST /api/kanban-cards/{{id}}/transition for administrative force transitions, or POST /api/kanban-cards/{{id}}/rereview for review reruns.",
                            old_status,
                            new_s,
                        ),
                    })),
                );
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
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({"error": format!("{e}")})),
                    );
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

        match sqlx::query(
            "UPDATE kanban_cards
             SET title = COALESCE($1, title),
                 priority = COALESCE($2, priority),
                 assigned_agent_id = COALESCE($3, assigned_agent_id),
                 repo_id = COALESCE($4, repo_id),
                 github_issue_url = COALESCE($5, github_issue_url),
                 description = COALESCE($6, description),
                 metadata = COALESCE($7::jsonb, metadata),
                 review_status = COALESCE($8, review_status),
                 review_notes = COALESCE($9, review_notes),
                 updated_at = NOW()
             WHERE id = $10",
        )
        .bind(body.title.as_deref())
        .bind(body.priority.as_deref())
        .bind(body.assignee_agent_id.as_deref())
        .bind(body.repo_id.as_deref())
        .bind(body.github_issue_url.as_deref())
        .bind(body.description.as_deref())
        .bind(metadata_json.as_deref())
        .bind(body.review_status.as_deref())
        .bind(body.review_notes.as_deref())
        .bind(&id)
        .execute(pool)
        .await
        {
            Ok(result) if result.rows_affected() == 0 => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "card not found"})),
                );
            }
            Ok(_) => {}
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{error}")})),
                );
            }
        }
    }

    // #108: Drain pending intents from hooks fired during transition_status_with_opts.
    // fire_dynamic_hooks fires policy hooks that may create dispatch intents, but
    // doesn't drain them itself. drain_hook_side_effects now also queues Discord
    // notifications for created dispatches, replacing the previous latest_dispatch_id
    // re-query that was susceptible to race conditions.
    crate::kanban::drain_hook_side_effects_with_backends(None, &state.engine);

    match load_card_json_pg(pool, &id).await {
        Ok(Some(card)) => {
            crate::server::ws::emit_event(&state.broadcast_tx, "kanban_card_updated", card.clone());
            (StatusCode::OK, Json(json!({"card": card})))
        }
        Ok(None) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "failed to read card after update"})),
        ),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        ),
    }
}

/// POST /api/kanban-cards/:id/assign
pub async fn assign_card(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<AssignCardBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(pool) = state.pg_pool_ref() {
        let old_status = match sqlx::query_scalar::<_, String>(
            "SELECT status FROM kanban_cards WHERE id = $1 LIMIT 1",
        )
        .bind(&id)
        .fetch_optional(pool)
        .await
        {
            Ok(Some(status)) => status,
            Ok(None) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "card not found"})),
                );
            }
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{error}")})),
                );
            }
        };

        crate::pipeline::ensure_loaded();
        let pipeline = crate::pipeline::get();
        let ready_state = pipeline
            .dispatchable_states()
            .into_iter()
            .next()
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                tracing::warn!("Pipeline has no dispatchable states, using initial state");
                pipeline.initial_state().to_string()
            });

        match sqlx::query(
            "UPDATE kanban_cards
             SET assigned_agent_id = $1,
                 updated_at = NOW()
             WHERE id = $2",
        )
        .bind(&body.agent_id)
        .bind(&id)
        .execute(pool)
        .await
        {
            Ok(result) if result.rows_affected() == 0 => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "card not found"})),
                );
            }
            Ok(_) => {}
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{error}")})),
                );
            }
        }

        if old_status != ready_state {
            if let Some(path) = pipeline.free_path_to_dispatchable(&old_status) {
                for step in &path {
                    if let Err(error) = crate::kanban::transition_status_with_opts_pg_only(
                        pool,
                        &state.engine,
                        &id,
                        step,
                        "assign",
                        crate::engine::transition::ForceIntent::None,
                    )
                    .await
                    {
                        tracing::warn!(
                            "[assign_card] postgres walk step to '{step}' failed: {error}"
                        );
                        break;
                    }
                }
            } else if let Err(error) = crate::kanban::transition_status_with_opts_pg_only(
                pool,
                &state.engine,
                &id,
                &ready_state,
                "assign",
                crate::engine::transition::ForceIntent::None,
            )
            .await
            {
                tracing::warn!("[assign_card] postgres transition failed: {error}");
            }
        }

        return match load_card_json_pg(pool, &id).await {
            Ok(Some(card)) => {
                crate::server::ws::emit_event(
                    &state.broadcast_tx,
                    "kanban_card_updated",
                    card.clone(),
                );
                (StatusCode::OK, Json(json!({"card": card})))
            }
            Ok(None) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "failed to read card after assign"})),
            ),
            Err(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            ),
        };
    }

    pg_pool_required_error()
}

/// DELETE /api/kanban-cards/:id
pub async fn delete_card(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(pool) = state.pg_pool_ref() {
        return match sqlx::query("DELETE FROM kanban_cards WHERE id = $1")
            .bind(&id)
            .execute(pool)
            .await
        {
            Ok(result) if result.rows_affected() == 0 => (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "card not found"})),
            ),
            Ok(_) => {
                crate::server::ws::emit_event(
                    &state.broadcast_tx,
                    "kanban_card_deleted",
                    json!({"id": id}),
                );
                (StatusCode::OK, Json(json!({"ok": true})))
            }
            Err(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{error}")})),
            ),
        };
    }

    pg_pool_required_error()
}

/// POST /api/kanban-cards/:id/retry
///
/// This endpoint is single-call complete. Do NOT chain /transition or
/// /auto-queue/generate after it — that creates duplicate dispatches
/// (see #1442 incident). Inspect `new_dispatch_id` and `next_action` in
/// the response to confirm the new dispatch was created.
pub async fn retry_card(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<RetryCardBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_pool_required_error();
    };
    let (stored_agent_id, retry_dispatch_type, retry_title) =
        match load_retry_dispatch_spec_pg(pool, &id).await {
            Ok(Some(spec)) => spec,
            Ok(None) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "card not found"})),
                );
            }
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{error}")})),
                );
            }
        };

    let existing_dispatch_id = sqlx::query_scalar::<_, Option<String>>(
        "SELECT latest_dispatch_id FROM kanban_cards WHERE id = $1",
    )
    .bind(&id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .flatten();
    let cancelled_dispatch_id: Option<String> = existing_dispatch_id.clone();
    if let Some(dispatch_id) = existing_dispatch_id.as_deref()
        && let Err(error) =
            crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_pg(pool, dispatch_id, None)
                .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{error}")})),
        );
    }

    use crate::engine::transition::TransitionIntent as TI2;
    let agent_id_for_dispatch = if let Some(agent_id) = body.assignee_agent_id.as_deref() {
        if let Err(error) = sqlx::query(
            "UPDATE kanban_cards
             SET assigned_agent_id = $1,
                 updated_at = NOW()
             WHERE id = $2",
        )
        .bind(agent_id)
        .bind(&id)
        .execute(pool)
        .await
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{error}")})),
            );
        }
        agent_id.to_string()
    } else {
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT assigned_agent_id FROM kanban_cards WHERE id = $1",
        )
        .bind(&id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .flatten()
        .unwrap_or_default()
    };

    if let Err(error) = execute_transition_intent_pg(
        &state,
        &TI2::SetLatestDispatchId {
            card_id: id.clone(),
            dispatch_id: None,
        },
    ) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{error}")})),
        );
    }

    let dispatch_agent_id = if agent_id_for_dispatch.is_empty() {
        stored_agent_id
    } else {
        agent_id_for_dispatch
    };
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
                new_dispatch_id = dispatch
                    .get("id")
                    .and_then(|value| value.as_str())
                    .map(|value| value.to_string());
            }
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{error}")})),
                );
            }
        }
    } else {
        // No agent assigned — caller must assign an agent before a dispatch can be created.
        next_action = "assign_agent_then_call_retry".to_string();
    }

    match load_card_json_pg(pool, &id).await {
        Ok(Some(card)) => {
            crate::server::ws::emit_event(&state.broadcast_tx, "kanban_card_updated", card.clone());
            (
                StatusCode::OK,
                Json(json!({
                    "card": card,
                    "new_dispatch_id": new_dispatch_id,
                    "cancelled_dispatch_id": cancelled_dispatch_id,
                    "next_action": next_action,
                })),
            )
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "card not found"})),
        ),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        ),
    }
}

/// POST /api/kanban-cards/:id/redispatch
///
/// This endpoint is single-call complete. Do NOT chain /transition or
/// /auto-queue/generate after it — that creates duplicate dispatches
/// (see #1442 incident). Inspect `new_dispatch_id` and `next_action` in
/// the response to confirm the new dispatch was created.
pub async fn redispatch_card(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(_body): Json<RedispatchCardBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_pool_required_error();
    };

    let (agent_id, dispatch_type, dispatch_title) =
        match load_retry_dispatch_spec_pg(pool, &id).await {
            Ok(Some(spec)) => spec,
            Ok(None) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "card not found"})),
                );
            }
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{error}")})),
                );
            }
        };

    let dispatch_id = sqlx::query_scalar::<_, Option<String>>(
        "SELECT latest_dispatch_id FROM kanban_cards WHERE id = $1",
    )
    .bind(&id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .flatten();
    let cancelled_dispatch_id: Option<String> = dispatch_id.clone();
    if let Some(dispatch_id) = dispatch_id.as_deref()
        && let Err(error) =
            crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_pg(pool, dispatch_id, None)
                .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{error}")})),
        );
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
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{error}")})),
            );
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
                new_dispatch_id = dispatch
                    .get("id")
                    .and_then(|value| value.as_str())
                    .map(|value| value.to_string());
            }
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{error}")})),
                );
            }
        }
    } else {
        // No agent assigned — caller must assign an agent before a dispatch can be created.
        next_action = "assign_agent_then_call_redispatch".to_string();
    }

    match load_card_json_pg(pool, &id).await {
        Ok(Some(card)) => {
            crate::server::ws::emit_event(&state.broadcast_tx, "kanban_card_updated", card.clone());
            (
                StatusCode::OK,
                Json(json!({
                    "card": card,
                    "new_dispatch_id": new_dispatch_id,
                    "cancelled_dispatch_id": cancelled_dispatch_id,
                    "next_action": next_action,
                })),
            )
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "card not found"})),
        ),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        ),
    }
}

/// PATCH /api/kanban-cards/:id/defer-dod
pub async fn defer_dod(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<DeferDodBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_pool_required_error();
    };

    let row = match sqlx::query_as::<_, (Option<String>, String, Option<String>)>(
        "SELECT deferred_dod_json, status, review_status
         FROM kanban_cards
         WHERE id = $1",
    )
    .bind(&id)
    .fetch_optional(pool)
    .await
    {
        Ok(row) => row,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("load postgres DoD state: {error}")})),
            );
        }
    };

    let Some((current, card_status, review_status)) = row else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "card not found"})),
        );
    };

    let dod = apply_deferred_dod_changes(current, body);
    let dod_str = serde_json::to_string(&dod).unwrap_or_default();
    if let Err(error) = sqlx::query(
        "UPDATE kanban_cards
         SET deferred_dod_json = $1, updated_at = NOW()
         WHERE id = $2",
    )
    .bind(&dod_str)
    .bind(&id)
    .execute(pool)
    .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("update postgres DoD state: {error}")})),
        );
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
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{error}")})),
                );
            }
        }
        if let Err(error) = sqlx::query(
            "UPDATE kanban_cards
             SET review_entered_at = NOW(), awaiting_dod_at = NULL
             WHERE id = $1",
        )
        .bind(&id)
        .execute(pool)
        .await
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("update postgres review clock: {error}")})),
            );
        }
    }

    if restart_review_state {
        crate::kanban::fire_enter_hooks_with_backends(None, &state.engine, &id, &card_status);
        tracing::info!(
            "[dod] Card {} DoD all-complete — restarting review from awaiting_dod",
            id
        );
    }

    match load_card_json_pg(pool, &id).await {
        Ok(Some(mut card)) => {
            card["deferred_dod"] = dod;
            (StatusCode::OK, Json(json!({"card": card})))
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "card not found"})),
        ),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        ),
    }
}

/// GET /api/kanban-cards/:id/review-state
/// #117: Returns the canonical card_review_state record for a card.
pub async fn get_card_review_state(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_pool_required_error();
    };

    match sqlx::query(
        "SELECT
            card_id,
            review_round::BIGINT AS review_round,
            state,
            pending_dispatch_id,
            last_verdict,
            last_decision,
            decided_by,
            decided_at::text AS decided_at,
            review_entered_at::text AS review_entered_at,
            updated_at::text AS updated_at
         FROM card_review_state
         WHERE card_id = $1",
    )
    .bind(&id)
    .fetch_optional(pool)
    .await
    {
        Ok(Some(row)) => (
            StatusCode::OK,
            Json(json!({
                "card_id": row.try_get::<String, _>("card_id").unwrap_or_else(|_| id.clone()),
                "review_round": row.try_get::<i64, _>("review_round").unwrap_or(0),
                "state": row.try_get::<String, _>("state").unwrap_or_else(|_| "idle".to_string()),
                "pending_dispatch_id": row.try_get::<Option<String>, _>("pending_dispatch_id").ok().flatten(),
                "last_verdict": row.try_get::<Option<String>, _>("last_verdict").ok().flatten(),
                "last_decision": row.try_get::<Option<String>, _>("last_decision").ok().flatten(),
                "decided_by": row.try_get::<Option<String>, _>("decided_by").ok().flatten(),
                "decided_at": row.try_get::<Option<String>, _>("decided_at").ok().flatten(),
                "review_entered_at": row.try_get::<Option<String>, _>("review_entered_at").ok().flatten(),
                "updated_at": row.try_get::<Option<String>, _>("updated_at").ok().flatten(),
            })),
        ),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no review state for this card"})),
        ),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{error}")})),
        ),
    }
}

/// GET /api/kanban-cards/:id/reviews
pub async fn list_card_reviews(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_pool_required_error();
    };

    match sqlx::query(
        "SELECT
            id::BIGINT AS id,
            kanban_card_id,
            dispatch_id,
            item_index::BIGINT AS item_index,
            decision,
            decided_at::text AS decided_at
         FROM review_decisions
         WHERE kanban_card_id = $1
         ORDER BY id",
    )
    .bind(&id)
    .fetch_all(pool)
    .await
    {
        Ok(rows) => {
            let reviews = rows
                .into_iter()
                .map(|row| {
                    json!({
                        "id": row.try_get::<i64, _>("id").unwrap_or(0),
                        "kanban_card_id": row.try_get::<Option<String>, _>("kanban_card_id").ok().flatten(),
                        "dispatch_id": row.try_get::<Option<String>, _>("dispatch_id").ok().flatten(),
                        "item_index": row.try_get::<Option<i64>, _>("item_index").ok().flatten(),
                        "decision": row.try_get::<Option<String>, _>("decision").ok().flatten(),
                        "decided_at": row.try_get::<Option<String>, _>("decided_at").ok().flatten(),
                    })
                })
                .collect::<Vec<_>>();
            (StatusCode::OK, Json(json!({"reviews": reviews})))
        }
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("query: {error}")})),
        ),
    }
}

/// GET /api/kanban-cards/stalled
pub async fn stalled_cards(State(state): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_pool_required_error();
    };

    let rows = match sqlx::query(
        "SELECT kc.id
         FROM kanban_cards kc
         LEFT JOIN task_dispatches td ON td.id = kc.latest_dispatch_id
         WHERE kc.status = 'in_progress'
           AND GREATEST(
               COALESCE(td.created_at, '-infinity'::timestamptz),
               COALESCE(kc.updated_at, '-infinity'::timestamptz),
               COALESCE(kc.started_at, '-infinity'::timestamptz)
           ) < NOW() - INTERVAL '2 hours'
           AND (
               NOT EXISTS (SELECT 1 FROM github_repos)
               OR kc.repo_id IN (SELECT id FROM github_repos)
           )
         ORDER BY GREATEST(
               COALESCE(td.created_at, '-infinity'::timestamptz),
               COALESCE(kc.updated_at, '-infinity'::timestamptz),
               COALESCE(kc.started_at, '-infinity'::timestamptz)
           ) ASC",
    )
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("query postgres stalled cards: {error}")})),
            );
        }
    };

    let mut cards = Vec::with_capacity(rows.len());
    for row in rows {
        let id = match row.try_get::<String, _>("id") {
            Ok(id) => id,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("decode postgres stalled card id: {error}")})),
                );
            }
        };
        match load_card_json_pg(pool, &id).await {
            Ok(Some(card)) => cards.push(card),
            Ok(None) => {}
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": error})),
                );
            }
        }
    }

    (StatusCode::OK, Json(json!(cards)))
}

/// POST /api/kanban-cards/bulk-action
pub async fn bulk_action(
    State(state): State<AppState>,
    Json(body): Json<BulkActionBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    // Pipeline-driven target status for bulk actions
    crate::pipeline::ensure_loaded();
    let pipeline = crate::pipeline::get();
    let terminal_state = pipeline
        .states
        .iter()
        .find(|s| s.terminal)
        .map(|s| s.id.as_str())
        .expect("Pipeline must have at least one terminal state");
    let initial_state = pipeline.initial_state();
    let target_status = match body.action.as_str() {
        "pass" => terminal_state.to_string(),
        "reset" => initial_state.to_string(),
        "cancel" => terminal_state.to_string(),
        "transition" => match body.target_status {
            Some(ref s) => s.clone(),
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": "transition action requires target_status field"})),
                );
            }
        },
        other => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("unknown action: {other}")})),
            );
        }
    };

    let mut results: Vec<serde_json::Value> = Vec::new();
    for card_id in &body.card_ids {
        let Some(pool) = state.pg_pool_ref() else {
            return pg_pool_required_error();
        };
        let transition_result = if target_status == "backlog" {
            transition_card_to_backlog_with_cleanup(&state, card_id, "bulk-action:backlog").await
        } else {
            crate::kanban::transition_status_with_opts_pg_only(
                pool,
                &state.engine,
                card_id,
                &target_status,
                "bulk-action",
                crate::engine::transition::ForceIntent::OperatorOverride,
            )
            .await
        };

        match transition_result {
            Ok(_) => {
                // Emit updated card for each successful transition
                if let Ok(Some(card)) = load_card_json_pg(pool, card_id).await {
                    crate::server::ws::emit_event(&state.broadcast_tx, "kanban_card_updated", card);
                }
                results.push(json!({"id": card_id, "ok": true}));
            }
            Err(e) => results.push(json!({"id": card_id, "ok": false, "error": format!("{e}")})),
        }
    }

    (
        StatusCode::OK,
        Json(json!({"action": body.action, "results": results})),
    )
}

/// POST /api/kanban-cards/assign-issue
pub async fn assign_issue(
    State(state): State<AppState>,
    Json(body): Json<AssignIssueBody>,
) -> (StatusCode, Json<serde_json::Value>) {
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
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": error})),
                );
            }
        };

        let old_status = if upserted.created {
            "backlog".to_string()
        } else {
            match sqlx::query_scalar::<_, String>(
                "SELECT status FROM kanban_cards WHERE id = $1 LIMIT 1",
            )
            .bind(&upserted.card_id)
            .fetch_optional(pool)
            .await
            {
                Ok(Some(status)) => status,
                Ok(None) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": "failed to reload card after upsert"})),
                    );
                }
                Err(error) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": format!("{error}")})),
                    );
                }
            }
        };

        crate::pipeline::ensure_loaded();
        let pipeline = crate::pipeline::get();
        let ready_state = pipeline
            .dispatchable_states()
            .into_iter()
            .next()
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                tracing::warn!("Pipeline has no dispatchable states, using initial state");
                pipeline.initial_state().to_string()
            });

        if old_status != ready_state {
            if let Some(path) = pipeline.free_path_to_dispatchable(&old_status) {
                for step in &path {
                    if let Err(error) = crate::kanban::transition_status_with_opts_pg_only(
                        pool,
                        &state.engine,
                        &upserted.card_id,
                        step,
                        "assign",
                        crate::engine::transition::ForceIntent::None,
                    )
                    .await
                    {
                        tracing::warn!(
                            "[assign_issue] postgres walk step to '{step}' failed: {error}"
                        );
                        break;
                    }
                }
            } else if let Err(error) = crate::kanban::transition_status_with_opts_pg_only(
                pool,
                &state.engine,
                &upserted.card_id,
                &ready_state,
                "assign",
                crate::engine::transition::ForceIntent::None,
            )
            .await
            {
                tracing::warn!("[assign_issue] postgres transition failed: {error}");
            }
        }

        return match load_card_json_pg(pool, &upserted.card_id).await {
            Ok(Some(card)) => {
                let event_name = if upserted.created {
                    "kanban_card_created"
                } else {
                    "kanban_card_updated"
                };
                crate::server::ws::emit_event(&state.broadcast_tx, event_name, card.clone());
                (
                    if upserted.created {
                        StatusCode::CREATED
                    } else {
                        StatusCode::OK
                    },
                    Json(json!({
                        "card": card,
                        "deduplicated": !upserted.created,
                    })),
                )
            }
            Ok(None) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "failed to read card after assign"})),
            ),
            Err(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            ),
        };
    }

    pg_pool_required_error()
}

// ── Helpers ────────────────────────────────────────────────────

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(super) fn card_row_to_json(row: &sqlite_test::Row) -> sqlite_test::Result<serde_json::Value> {
    let repo_id = row.get::<_, Option<String>>(1)?;
    let assigned_agent_id = row.get::<_, Option<String>>(5)?;
    let metadata_raw = row.get::<_, Option<String>>(10).unwrap_or(None);
    let metadata_parsed = metadata_raw
        .as_ref()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok());
    let latest_dispatch_status = row.get::<_, Option<String>>(13).unwrap_or(None);
    let latest_dispatch_type = row.get::<_, Option<String>>(14).unwrap_or(None);
    let latest_dispatch_result_raw = row.get::<_, Option<String>>(17).unwrap_or(None);
    let latest_dispatch_context_raw = row.get::<_, Option<String>>(18).unwrap_or(None);
    let latest_dispatch_result_summary = crate::dispatch::summarize_dispatch_from_text(
        latest_dispatch_type.as_deref(),
        latest_dispatch_status.as_deref(),
        latest_dispatch_result_raw.as_deref(),
        latest_dispatch_context_raw.as_deref(),
    );

    // Extended columns (indices 19-31)
    let description = row.get::<_, Option<String>>(19).unwrap_or(None);
    let blocked_reason = row.get::<_, Option<String>>(20).unwrap_or(None);
    let review_notes = row.get::<_, Option<String>>(21).unwrap_or(None);
    let review_status = row.get::<_, Option<String>>(22).unwrap_or(None);
    let started_at = row.get::<_, Option<String>>(23).unwrap_or(None);
    let requested_at = row.get::<_, Option<String>>(24).unwrap_or(None);
    let completed_at = row.get::<_, Option<String>>(25).unwrap_or(None);
    let pipeline_stage_id = row.get::<_, Option<String>>(26).unwrap_or(None);
    let owner_agent_id = row.get::<_, Option<String>>(27).unwrap_or(None);
    let requester_agent_id = row.get::<_, Option<String>>(28).unwrap_or(None);
    let parent_card_id = row.get::<_, Option<String>>(29).unwrap_or(None);
    let sort_order = row.get::<_, i64>(30).unwrap_or(0);
    let depth = row.get::<_, i64>(31).unwrap_or(0);
    let review_entered_at = row.get::<_, Option<String>>(32).unwrap_or(None);

    Ok(json!({
        "id": row.get::<_, String>(0)?,
        // existing fields
        "repo_id": repo_id,
        "title": row.get::<_, String>(2)?,
        "status": row.get::<_, String>(3)?,
        "priority": row.get::<_, String>(4)?,
        "assigned_agent_id": assigned_agent_id,
        "github_issue_url": row.get::<_, Option<String>>(6)?,
        "github_issue_number": row.get::<_, Option<i64>>(7)?,
        "latest_dispatch_id": row.get::<_, Option<String>>(8)?,
        "review_round": row.get::<_, i64>(9).unwrap_or(0),
        "metadata": metadata_parsed,
        "created_at": row.get::<_, Option<String>>(11).ok().flatten().or_else(|| row.get::<_, Option<i64>>(11).ok().flatten().map(|v| v.to_string())),
        "updated_at": row.get::<_, Option<String>>(12).ok().flatten().or_else(|| row.get::<_, Option<i64>>(12).ok().flatten().map(|v| v.to_string())),
        // alias fields for frontend compatibility
        "github_repo": repo_id,
        "assignee_agent_id": assigned_agent_id,
        "metadata_json": metadata_raw,
        // extended fields from DB
        "description": description,
        "blocked_reason": blocked_reason,
        "review_notes": review_notes,
        "review_status": review_status,
        "started_at": started_at,
        "requested_at": requested_at,
        "completed_at": completed_at,
        "pipeline_stage_id": pipeline_stage_id,
        "owner_agent_id": owner_agent_id,
        "requester_agent_id": requester_agent_id,
        "parent_card_id": parent_card_id,
        "sort_order": sort_order,
        "depth": depth,
        "review_entered_at": review_entered_at,
        // dispatch join fields
        "latest_dispatch_status": latest_dispatch_status.clone(),
        "latest_dispatch_title": row.get::<_, Option<String>>(15).unwrap_or(None),
        "latest_dispatch_type": latest_dispatch_type.clone(),
        "latest_dispatch_result_summary": latest_dispatch_result_summary,
        "latest_dispatch_chain_depth": row.get::<_, Option<i64>>(16).unwrap_or(None),
        "child_count": 0,
    }))
}

pub(super) async fn load_card_json_pg(
    pool: &sqlx::PgPool,
    card_id: &str,
) -> Result<Option<serde_json::Value>, String> {
    let row = sqlx::query(
        "SELECT
            kc.id,
            kc.repo_id,
            kc.title,
            kc.status,
            COALESCE(kc.priority, 'medium') AS priority,
            kc.assigned_agent_id,
            kc.github_issue_url,
            kc.github_issue_number::BIGINT AS github_issue_number,
            kc.latest_dispatch_id,
            COALESCE(kc.review_round, 0)::BIGINT AS review_round,
            kc.metadata::text AS metadata,
            kc.created_at::text AS created_at,
            kc.updated_at::text AS updated_at,
            td.status AS d_status,
            td.dispatch_type AS d_type,
            td.title AS d_title,
            td.chain_depth::BIGINT AS d_depth,
            td.result AS d_result,
            td.context AS d_context,
            kc.description,
            kc.blocked_reason,
            kc.review_notes,
            kc.review_status,
            kc.started_at::text AS started_at,
            kc.requested_at::text AS requested_at,
            kc.completed_at::text AS completed_at,
            kc.pipeline_stage_id,
            kc.owner_agent_id,
            kc.requester_agent_id,
            kc.parent_card_id,
            COALESCE(kc.sort_order, 0)::BIGINT AS sort_order,
            COALESCE(kc.depth, 0)::BIGINT AS depth,
            kc.review_entered_at::text AS review_entered_at
         FROM kanban_cards kc
         LEFT JOIN task_dispatches td ON td.id = kc.latest_dispatch_id
         WHERE kc.id = $1",
    )
    .bind(card_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres card {card_id}: {error}"))?;

    let Some(row) = row else {
        return Ok(None);
    };

    let repo_id: Option<String> = row
        .try_get("repo_id")
        .map_err(|error| format!("decode repo_id for {card_id}: {error}"))?;
    let assigned_agent_id: Option<String> = row
        .try_get("assigned_agent_id")
        .map_err(|error| format!("decode assigned_agent_id for {card_id}: {error}"))?;
    let metadata_raw: Option<String> = row
        .try_get("metadata")
        .map_err(|error| format!("decode metadata for {card_id}: {error}"))?;
    let metadata_parsed = metadata_raw
        .as_ref()
        .and_then(|value| serde_json::from_str::<serde_json::Value>(value).ok());
    let latest_dispatch_status: Option<String> = row
        .try_get("d_status")
        .map_err(|error| format!("decode d_status for {card_id}: {error}"))?;
    let latest_dispatch_type: Option<String> = row
        .try_get("d_type")
        .map_err(|error| format!("decode d_type for {card_id}: {error}"))?;
    let latest_dispatch_result_raw: Option<String> = row
        .try_get("d_result")
        .map_err(|error| format!("decode d_result for {card_id}: {error}"))?;
    let latest_dispatch_context_raw: Option<String> = row
        .try_get("d_context")
        .map_err(|error| format!("decode d_context for {card_id}: {error}"))?;
    let latest_dispatch_result_summary = crate::dispatch::summarize_dispatch_from_text(
        latest_dispatch_type.as_deref(),
        latest_dispatch_status.as_deref(),
        latest_dispatch_result_raw.as_deref(),
        latest_dispatch_context_raw.as_deref(),
    );

    Ok(Some(json!({
        "id": row.try_get::<String, _>("id").map_err(|error| format!("decode id for {card_id}: {error}"))?,
        "repo_id": repo_id,
        "title": row.try_get::<String, _>("title").map_err(|error| format!("decode title for {card_id}: {error}"))?,
        "status": row.try_get::<String, _>("status").map_err(|error| format!("decode status for {card_id}: {error}"))?,
        "priority": row.try_get::<String, _>("priority").map_err(|error| format!("decode priority for {card_id}: {error}"))?,
        "assigned_agent_id": assigned_agent_id,
        "github_issue_url": row.try_get::<Option<String>, _>("github_issue_url").map_err(|error| format!("decode github_issue_url for {card_id}: {error}"))?,
        "github_issue_number": row.try_get::<Option<i64>, _>("github_issue_number").map_err(|error| format!("decode github_issue_number for {card_id}: {error}"))?,
        "latest_dispatch_id": row.try_get::<Option<String>, _>("latest_dispatch_id").map_err(|error| format!("decode latest_dispatch_id for {card_id}: {error}"))?,
        "review_round": row.try_get::<i64, _>("review_round").map_err(|error| format!("decode review_round for {card_id}: {error}"))?,
        "metadata": metadata_parsed,
        "created_at": row.try_get::<Option<String>, _>("created_at").map_err(|error| format!("decode created_at for {card_id}: {error}"))?,
        "updated_at": row.try_get::<Option<String>, _>("updated_at").map_err(|error| format!("decode updated_at for {card_id}: {error}"))?,
        "github_repo": repo_id,
        "assignee_agent_id": assigned_agent_id,
        "metadata_json": metadata_raw,
        "description": row.try_get::<Option<String>, _>("description").map_err(|error| format!("decode description for {card_id}: {error}"))?,
        "blocked_reason": row.try_get::<Option<String>, _>("blocked_reason").map_err(|error| format!("decode blocked_reason for {card_id}: {error}"))?,
        "review_notes": row.try_get::<Option<String>, _>("review_notes").map_err(|error| format!("decode review_notes for {card_id}: {error}"))?,
        "review_status": row.try_get::<Option<String>, _>("review_status").map_err(|error| format!("decode review_status for {card_id}: {error}"))?,
        "started_at": row.try_get::<Option<String>, _>("started_at").map_err(|error| format!("decode started_at for {card_id}: {error}"))?,
        "requested_at": row.try_get::<Option<String>, _>("requested_at").map_err(|error| format!("decode requested_at for {card_id}: {error}"))?,
        "completed_at": row.try_get::<Option<String>, _>("completed_at").map_err(|error| format!("decode completed_at for {card_id}: {error}"))?,
        "pipeline_stage_id": row.try_get::<Option<String>, _>("pipeline_stage_id").map_err(|error| format!("decode pipeline_stage_id for {card_id}: {error}"))?,
        "owner_agent_id": row.try_get::<Option<String>, _>("owner_agent_id").map_err(|error| format!("decode owner_agent_id for {card_id}: {error}"))?,
        "requester_agent_id": row.try_get::<Option<String>, _>("requester_agent_id").map_err(|error| format!("decode requester_agent_id for {card_id}: {error}"))?,
        "parent_card_id": row.try_get::<Option<String>, _>("parent_card_id").map_err(|error| format!("decode parent_card_id for {card_id}: {error}"))?,
        "sort_order": row.try_get::<i64, _>("sort_order").map_err(|error| format!("decode sort_order for {card_id}: {error}"))?,
        "depth": row.try_get::<i64, _>("depth").map_err(|error| format!("decode depth for {card_id}: {error}"))?,
        "review_entered_at": row.try_get::<Option<String>, _>("review_entered_at").map_err(|error| format!("decode review_entered_at for {card_id}: {error}"))?,
        "latest_dispatch_status": latest_dispatch_status.clone(),
        "latest_dispatch_title": row.try_get::<Option<String>, _>("d_title").map_err(|error| format!("decode d_title for {card_id}: {error}"))?,
        "latest_dispatch_type": latest_dispatch_type.clone(),
        "latest_dispatch_result_summary": latest_dispatch_result_summary,
        "latest_dispatch_chain_depth": row.try_get::<Option<i64>, _>("d_depth").map_err(|error| format!("decode d_depth for {card_id}: {error}"))?,
        "child_count": 0,
    })))
}

// ── Audit Log API ────────────────────────────────────────────

/// GET /api/kanban-cards/:id/audit-log
pub async fn card_audit_log(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_pool_required_error();
    };

    let rows = match sqlx::query(
        "SELECT id::BIGINT AS id, card_id, from_status, to_status, source, result, created_at::text AS created_at
         FROM kanban_audit_logs
         WHERE card_id = $1
         ORDER BY created_at DESC
         LIMIT 50",
    )
    .bind(&id)
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("query postgres card audit log: {error}")})),
            );
        }
    };

    let logs: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|row| {
            json!({
                "id": row.try_get::<i64, _>("id").unwrap_or_default(),
                "card_id": row.try_get::<String, _>("card_id").unwrap_or_default(),
                "from_status": row.try_get::<Option<String>, _>("from_status").ok().flatten(),
                "to_status": row.try_get::<Option<String>, _>("to_status").ok().flatten(),
                "source": row.try_get::<Option<String>, _>("source").ok().flatten(),
                "result": row.try_get::<Option<String>, _>("result").ok().flatten(),
                "created_at": row.try_get::<Option<String>, _>("created_at").ok().flatten(),
            })
        })
        .collect();

    (StatusCode::OK, Json(json!({"logs": logs})))
}

/// GET /api/kanban-cards/:id/comments
/// Fetch GitHub comments for the linked issue via `gh` CLI.
pub async fn card_github_comments(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_pool_required_error();
    };

    let (repo_id, issue_number) = match sqlx::query(
        "SELECT repo_id, github_issue_number::BIGINT AS github_issue_number
         FROM kanban_cards
         WHERE id = $1",
    )
    .bind(&id)
    .fetch_optional(pool)
    .await
    {
        Ok(Some(row)) => {
            let repo_id = match row.try_get::<Option<String>, _>("repo_id") {
                Ok(repo_id) => repo_id,
                Err(error) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": format!("decode postgres card repo_id: {error}")})),
                    );
                }
            };
            let issue_number = match row.try_get::<Option<i64>, _>("github_issue_number") {
                Ok(issue_number) => issue_number,
                Err(error) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(
                            json!({"error": format!("decode postgres card github_issue_number: {error}")}),
                        ),
                    );
                }
            };
            (repo_id, issue_number)
        }
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "card not found"})),
            );
        }
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("query postgres card github issue: {error}")})),
            );
        }
    };

    let repo = match repo_id {
        Some(r) => r,
        None => return (StatusCode::OK, Json(json!({"comments": []}))),
    };
    let number = match issue_number {
        Some(n) => n,
        None => return (StatusCode::OK, Json(json!({"comments": []}))),
    };

    let result =
        tokio::task::spawn_blocking(move || crate::github::fetch_issue_comments(&repo, number))
            .await;

    match result {
        Ok(Ok(issue)) => {
            let comments = serde_json::to_value(issue.comments).unwrap_or_else(|_| json!([]));
            let body = issue.body.unwrap_or_default();

            if let Err(error) = sqlx::query(
                "UPDATE kanban_cards
                 SET description = $1,
                     updated_at = NOW()
                 WHERE id = $2
                   AND (description IS DISTINCT FROM $1 OR description IS NULL)",
            )
            .bind(&body)
            .bind(&id)
            .execute(pool)
            .await
            {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("update postgres card description: {error}")})),
                );
            }

            (
                StatusCode::OK,
                Json(json!({"comments": comments, "body": body})),
            )
        }
        Ok(Err(e)) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": format!("gh issue view failed: {e}")})),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("join: {e}")})),
        ),
    }
}

// ── PM Decision API ──────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct PmDecisionBody {
    pub card_id: String,
    pub decision: String, // "resume", "rework", "dismiss", "requeue"
    pub comment: Option<String>,
}

/// POST /api/pm-decision
/// PM's decision on a manual-intervention card.
/// - resume: return card to in_progress (continue work)
/// - rework: create rework dispatch to assigned agent
/// - dismiss: move card to done (PM decides work is sufficient)
/// - requeue: move card back to ready for re-prioritization
pub async fn pm_decision(
    State(state): State<AppState>,
    Json(body): Json<PmDecisionBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(transition_pool) = state.pg_pool_ref() else {
        return pg_pool_required_error();
    };

    let valid = ["resume", "rework", "dismiss", "requeue"];
    if !valid.contains(&body.decision.as_str()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("decision must be one of: {}", valid.join(", "))})),
        );
    }

    // Verify card exists and currently requires manual intervention.
    let card_info: Option<(String, Option<String>, Option<String>, String, String)> =
        match sqlx::query_as(
            "SELECT COALESCE(status, ''), review_status, blocked_reason, COALESCE(assigned_agent_id, ''), title
             FROM kanban_cards
             WHERE id = $1",
        )
        .bind(&body.card_id)
        .fetch_optional(transition_pool)
        .await
        {
            Ok(row) => row,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("load card for pm decision: {error}")})),
                );
            }
        };

    let Some((status, review_status, blocked_reason, agent_id, title)) = card_info else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "card not found"})),
        );
    };

    let manual_fingerprint = crate::manual_intervention::manual_intervention_fingerprint(
        review_status.as_deref(),
        blocked_reason.as_deref(),
    );
    let legacy_manual_state = matches!(status.as_str(), "pending_decision" | "blocked");
    if !legacy_manual_state && manual_fingerprint.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(
                json!({"error": format!("card is '{}', which does not currently require manual decision", status)}),
            ),
        );
    }

    // Complete any pending pm-decision dispatches (rework handles its own completion after dispatch success)
    if body.decision != "rework" {
        let completion_result = json!({"decision": body.decision, "comment": body.comment});
        let pending_dispatch_ids: Vec<String> = match sqlx::query_scalar(
            "SELECT id FROM task_dispatches
             WHERE kanban_card_id = $1
               AND dispatch_type = 'pm-decision'
               AND status = 'pending'",
        )
        .bind(&body.card_id)
        .fetch_all(transition_pool)
        .await
        {
            Ok(ids) => ids,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("load pending pm-decision dispatches: {error}")})),
                );
            }
        };
        for dispatch_id in pending_dispatch_ids {
            crate::dispatch::set_dispatch_status_with_backends(
                None,
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
    if let Err(error) = sqlx::query(
        "UPDATE kanban_cards SET blocked_reason = NULL, updated_at = NOW() WHERE id = $1",
    )
    .bind(&body.card_id)
    .execute(transition_pool)
    .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("clear manual intervention marker: {error}")})),
        );
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
            let has_live = match sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*)::BIGINT
                 FROM task_dispatches td
                 JOIN sessions s ON s.active_dispatch_id = td.id
                    AND s.status IN ('turn_active', 'awaiting_bg', 'awaiting_user', 'working', 'idle')
                 WHERE td.kanban_card_id = $1
                   AND td.status IN ('pending', 'dispatched')",
            )
            .bind(&body.card_id)
            .fetch_one(transition_pool)
            .await
            {
                Ok(count) => count > 0,
                Err(error) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": format!("check live dispatch/session: {error}")})),
                    );
                }
            };
            if !has_live {
                return (
                    StatusCode::CONFLICT,
                    Json(
                        json!({"error": "cannot resume: no live dispatch/session for this card. Use 'rework' or 'requeue' instead."}),
                    ),
                );
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
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("resume transition failed: {e}")})),
                );
            }
            "Card resumed"
        }
        "rework" => {
            if agent_id.is_empty() {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": "card has no assigned agent for rework"})),
                );
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
                    let pending_dispatch_ids: Vec<String> = match sqlx::query_scalar(
                        "SELECT id FROM task_dispatches
                         WHERE kanban_card_id = $1
                           AND dispatch_type = 'pm-decision'
                           AND status = 'pending'",
                    )
                    .bind(&body.card_id)
                    .fetch_all(transition_pool)
                    .await
                    {
                        Ok(ids) => ids,
                        Err(error) => {
                            return (
                                StatusCode::INTERNAL_SERVER_ERROR,
                                Json(
                                    json!({"error": format!("load pending pm-decision dispatches: {error}")}),
                                ),
                            );
                        }
                    };
                    for dispatch_id in pending_dispatch_ids {
                        crate::dispatch::set_dispatch_status_with_backends(
                            None,
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
                        match sqlx::query_scalar("SELECT status FROM kanban_cards WHERE id = $1")
                            .bind(&body.card_id)
                            .fetch_optional(transition_pool)
                            .await
                        {
                            Ok(status) => status.unwrap_or_default(),
                            Err(error) => {
                                return (
                                    StatusCode::INTERNAL_SERVER_ERROR,
                                    Json(json!({"error": format!("load rework status: {error}")})),
                                );
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
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({"error": format!("rework transition failed: {e}")})),
                        );
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
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": format!("rework dispatch failed: {}", e)})),
                    );
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
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("dismiss transition failed: {e}")})),
                );
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
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("requeue transition failed: {e}")})),
                );
            }
            "Card requeued"
        }
        _ => "Unknown decision",
    };

    // Emit kanban_card_updated for the affected card
    if let Ok(Some(card)) = load_card_json_pg(transition_pool, &body.card_id).await {
        crate::server::ws::emit_event(&state.broadcast_tx, "kanban_card_updated", card);
    }

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "card_id": body.card_id,
            "decision": body.decision,
            "message": message,
        })),
    )
}

// ── Administrative review recovery helpers ───────────────────────

#[derive(Debug, Deserialize)]
pub struct RereviewBody {
    pub reason: Option<String>,
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn find_active_review_dispatch_id(conn: &sqlite_test::Connection, card_id: &str) -> Option<String> {
    conn.query_row(
        "SELECT id FROM task_dispatches
         WHERE kanban_card_id = ?1
           AND dispatch_type = 'review'
           AND status IN ('pending', 'dispatched')
         ORDER BY updated_at DESC, rowid DESC
         LIMIT 1",
        [card_id],
        |row| row.get(0),
    )
    .ok()
}

async fn find_active_review_dispatch_id_pg(pool: &sqlx::PgPool, card_id: &str) -> Option<String> {
    sqlx::query_scalar::<_, String>(
        "SELECT id FROM task_dispatches
         WHERE kanban_card_id = $1
           AND dispatch_type = 'review'
           AND status IN ('pending', 'dispatched')
         ORDER BY updated_at DESC, id DESC
         LIMIT 1",
    )
    .bind(card_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
}

fn trimmed_header_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

pub(crate) fn require_explicit_bearer_token(
    headers: &HeaderMap,
    operation: &str,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    let config = crate::config::load_graceful();
    if let Some(expected_token) = config.server.auth_token.as_deref() {
        if !expected_token.is_empty() {
            let provided = trimmed_header_value(headers, "authorization")
                .and_then(|value| value.strip_prefix("Bearer "))
                .map(str::trim);
            if provided != Some(expected_token) {
                return Err((
                    StatusCode::UNAUTHORIZED,
                    Json(json!({"error": format!("{operation} requires explicit Bearer token")})),
                ));
            }
        }
    }

    if let Some(expected_channel_id) = config
        .kanban
        .manager_channel_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let provided_channel_id = trimmed_header_value(headers, "x-channel-id");
        if provided_channel_id != Some(expected_channel_id) {
            return Err((
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": format!("{operation} requires PMD channel authorization")})),
            ));
        }
    }

    Ok(())
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn resolve_agent_id_from_channel_id_on_conn(
    conn: &sqlite_test::Connection,
    channel_id: &str,
) -> Option<String> {
    conn.query_row(
        "SELECT id FROM agents
         WHERE discord_channel_id = ?1
            OR discord_channel_alt = ?1
            OR discord_channel_cc = ?1
            OR discord_channel_cdx = ?1
         LIMIT 1",
        [channel_id],
        |row| row.get(0),
    )
    .ok()
}

async fn resolve_agent_id_from_channel_id_with_pg(
    pool: &sqlx::PgPool,
    channel_id: &str,
) -> Option<String> {
    sqlx::query(
        "SELECT id FROM agents
         WHERE discord_channel_id = $1
            OR discord_channel_alt = $1
            OR discord_channel_cc = $1
            OR discord_channel_cdx = $1
         LIMIT 1",
    )
    .bind(channel_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .and_then(|row| row.try_get::<String, _>("id").ok())
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(super) fn resolve_requesting_agent_id_on_conn(
    conn: &sqlite_test::Connection,
    headers: &HeaderMap,
) -> Option<String> {
    if let Some(agent_id) = trimmed_header_value(headers, "x-agent-id") {
        return conn
            .query_row(
                "SELECT id FROM agents WHERE id = ?1 LIMIT 1",
                [agent_id],
                |row| row.get(0),
            )
            .ok()
            .or_else(|| Some(agent_id.to_string()));
    }

    trimmed_header_value(headers, "x-channel-id")
        .and_then(|channel_id| resolve_agent_id_from_channel_id_on_conn(conn, channel_id))
}

pub(crate) async fn resolve_requesting_agent_id_with_pg(
    pool: &sqlx::PgPool,
    headers: &HeaderMap,
) -> Option<String> {
    if let Some(agent_id) = trimmed_header_value(headers, "x-agent-id") {
        return sqlx::query("SELECT id FROM agents WHERE id = $1 LIMIT 1")
            .bind(agent_id)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten()
            .and_then(|row| row.try_get::<String, _>("id").ok())
            .or_else(|| Some(agent_id.to_string()));
    }

    match trimmed_header_value(headers, "x-channel-id") {
        Some(channel_id) => resolve_agent_id_from_channel_id_with_pg(pool, channel_id).await,
        None => None,
    }
}

/// POST /api/kanban-cards/:id/rereview
///
/// Recovery endpoint. Forces a card back through counter-model review
/// using the best available execution target for that card's implementation.
pub async fn rereview_card(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<RereviewBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Err(response) = require_explicit_bearer_token(&headers, "rereview") {
        return response;
    }

    let Some(pool) = state.pg_pool_ref() else {
        return pg_pool_required_error();
    };
    let reason = body.reason.as_deref().unwrap_or("manual rereview");
    let (current_status, assigned_agent_id, card_title, gh_url) =
        match sqlx::query_as::<_, (String, Option<String>, String, Option<String>)>(
            "SELECT status, assigned_agent_id, title, github_issue_url
             FROM kanban_cards WHERE id = $1",
        )
        .bind(&id)
        .fetch_optional(pool)
        .await
        {
            Ok(Some(values)) => values,
            Ok(None) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": format!("card not found: {id}")})),
                );
            }
            Err(error) => {
                tracing::warn!(
                    card_id = %id,
                    %error,
                    "[rereview] postgres lookup failed"
                );
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("postgres lookup failed: {error}")})),
                );
            }
        };
    let caller_source = resolve_requesting_agent_id_with_pg(pool, &headers)
        .await
        .unwrap_or_else(|| "api".to_string());

    let assigned_agent_id = match assigned_agent_id.filter(|value| !value.is_empty()) {
        Some(value) => value,
        None => {
            return (
                StatusCode::CONFLICT,
                Json(json!({"error": "card has no assigned agent"})),
            );
        }
    };

    let stale_ids = match sqlx::query_scalar::<_, String>(
        "SELECT id FROM task_dispatches
         WHERE kanban_card_id = $1
           AND dispatch_type IN ('review', 'review-decision')
           AND status IN ('pending', 'dispatched')",
    )
    .bind(&id)
    .fetch_all(pool)
    .await
    {
        Ok(ids) => ids,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("postgres stale dispatch lookup failed: {error}")})),
            );
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
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{error}")})),
            );
        }
    }

    if let Err(error) = sqlx::query(
        "UPDATE kanban_cards
         SET review_status = NULL,
             suggestion_pending_at = NULL,
             review_entered_at = NULL,
             awaiting_dod_at = NULL,
             updated_at = NOW()
         WHERE id = $1",
    )
    .bind(&id)
    .execute(pool)
    .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("postgres rereview cleanup failed: {error}")})),
        );
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

    if let Err(error) = sqlx::query(
        "UPDATE card_review_state
         SET approach_change_round = NULL,
             session_reset_round = NULL,
             updated_at = NOW()
         WHERE card_id = $1",
    )
    .bind(&id)
    .execute(pool)
    .await
    {
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
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    } else {
        crate::kanban::fire_enter_hooks_with_backends(None, &state.engine, &id, "review");
    }

    let mut review_dispatch_id = find_active_review_dispatch_id_pg(pool, &id).await;

    if review_dispatch_id.is_none() && !transitioned_into_review {
        let _ = state
            .engine
            .fire_hook_by_name_blocking("OnReviewEnter", json!({ "card_id": id }));
        crate::kanban::drain_hook_side_effects_with_backends(None, &state.engine);
        review_dispatch_id = find_active_review_dispatch_id_pg(pool, &id).await;
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
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
        }
    }

    let Some(review_dispatch_id) = review_dispatch_id else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "failed to create fresh review dispatch"})),
        );
    };

    crate::kanban::correct_tn_to_fn_on_reopen(None, state.pg_pool_ref(), &id);

    if let Err(error) = sqlx::query(
        "UPDATE kanban_cards
         SET completed_at = NULL, updated_at = NOW()
         WHERE id = $1",
    )
    .bind(&id)
    .execute(pool)
    .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("postgres completed_at reset failed: {error}")})),
        );
    }

    let entry_ids = match sqlx::query_scalar::<_, String>(
        "SELECT id FROM auto_queue_entries
         WHERE kanban_card_id = $1
           AND status IN ('pending', 'dispatched', 'done')
           AND run_id IN (
               SELECT id FROM auto_queue_runs WHERE status IN ('active', 'paused')
           )",
    )
    .bind(&id)
    .fetch_all(pool)
    .await
    {
        Ok(ids) => ids,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("postgres auto-queue entry lookup failed: {error}")})),
            );
        }
    };

    for entry_id in entry_ids {
        if let Err(error) = move_auto_queue_entry_to_dispatched_on_pg(
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
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": format!("card not found: {id}")})),
            );
        }
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{error}")})),
            );
        }
    };

    if !crate::pipeline::get().is_terminal("review")
        && crate::pipeline::get().is_terminal(&current_status)
    {
        if let Some(url) = gh_url.as_deref() {
            if let Err(e) = crate::github::reopen_issue_by_url(url).await {
                tracing::warn!("[kanban] Failed to reopen GitHub issue {url}: {e}");
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({
                        "error": format!("github issue reopen failed before rereview response: {e}"),
                        "rereviewed": false,
                        "github_issue_url": url,
                    })),
                );
            }
        }
    }

    crate::server::ws::emit_event(&state.broadcast_tx, "kanban_card_updated", card.clone());
    (
        StatusCode::OK,
        Json(json!({
            "card": card,
            "rereviewed": true,
            "review_dispatch_id": review_dispatch_id,
            "reason": reason,
        })),
    )
}

#[derive(Debug, Deserialize)]
pub struct BatchRereviewBody {
    pub issues: Vec<i64>,
    pub reason: Option<String>,
}

/// POST /api/kanban-cards/batch-rereview (formerly /api/re-review, removed in #1064)
///
/// Batch endpoint. Accepts a list of GitHub issue numbers,
/// looks up each card, and calls the rereview logic for each.
/// Per-item error handling: one failure does not stop others.
pub async fn batch_rereview(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<BatchRereviewBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Err(response) = require_explicit_bearer_token(&headers, "batch rereview") {
        return response;
    }

    let reason = body.reason.clone();
    let Some(pool) = state.pg_pool_ref() else {
        return pg_pool_required_error();
    };
    let mut results = Vec::new();

    for issue_number in &body.issues {
        let card_id = match sqlx::query_scalar::<_, String>(
            "SELECT id FROM kanban_cards WHERE github_issue_number = $1",
        )
        .bind(*issue_number)
        .fetch_optional(pool)
        .await
        {
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
                    "error": format!("postgres lookup failed: {error}"),
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

        let (status, Json(response)) = rereview_card(
            State(state.clone()),
            Path(card_id),
            headers.clone(),
            Json(rereview_body),
        )
        .await;

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

    (StatusCode::OK, Json(json!({ "results": results })))
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct ReopenBody {
    pub review_status: Option<String>,
    pub dispatch_type: Option<String>,
    pub reason: Option<String>,
    pub reset_full: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct BatchTransitionBody {
    pub issue_numbers: Option<Vec<i64>>,
    pub card_ids: Option<Vec<String>>,
    pub status: String,
    pub cancel_dispatches: Option<bool>,
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
    Json(body): Json<ReopenBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let reset_full = body.reset_full.unwrap_or(false);

    if let Err(response) = require_explicit_bearer_token(&headers, "reopen") {
        return response;
    }

    let Some(pool) = state.pg_pool_ref() else {
        return pg_pool_required_error();
    };
    let caller_source = resolve_requesting_agent_id_with_pg(pool, &headers)
        .await
        .unwrap_or_else(|| "api".to_string());

    // ── Pre-check: card must be in done state ──
    let current_status: String =
        match sqlx::query_scalar::<_, String>("SELECT status FROM kanban_cards WHERE id = $1")
            .bind(&id)
            .fetch_optional(pool)
            .await
        {
            Ok(Some(status)) => status,
            Ok(None) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": format!("card not found: {id}")})),
                );
            }
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{error}")})),
                );
            }
        };

    // Pipeline-driven: reopen only applies to terminal states
    crate::pipeline::ensure_loaded();
    let pipeline = crate::pipeline::get();
    let is_terminal = pipeline.is_terminal(&current_status);
    if !is_terminal {
        return (
            StatusCode::BAD_REQUEST,
            Json(
                json!({"error": format!("card is not terminal (current: {current_status}), reopen only applies to terminal cards")}),
            ),
        );
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
        if let Err(error) = mark_api_reopen_skip_preflight_on_pg(pool, &id).await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    json!({"error": format!("failed to stage API reopen preflight skip: {error}")}),
                ),
            );
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
                crate::kanban::correct_tn_to_fn_on_reopen(None, state.pg_pool_ref(), &id);

                if reset_full {
                    if let Err(error) = clear_all_threads_pg(pool, &id).await {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({"error": format!("{error}")})),
                        );
                    }
                    if let Err(error) = clear_reopen_preflight_cache_on_pg(pool, &id).await {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(
                                json!({"error": format!("failed to clear reopen cache: {error}")}),
                            ),
                        );
                    }
                } else if let Err(error) = consume_api_reopen_preflight_skip_on_pg(pool, &id).await
                {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(
                            json!({"error": format!("failed to persist API reopen preflight skip: {error}")}),
                        ),
                    );
                }

                if let Err(error) = sqlx::query(
                    "UPDATE kanban_cards
                     SET completed_at = NULL,
                         updated_at = NOW()
                     WHERE id = $1",
                )
                .bind(&id)
                .execute(pool)
                .await
                {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": format!("{error}")})),
                    );
                }

                if let Some(ref rs) = body.review_status
                    && let Err(error) = sqlx::query(
                        "UPDATE kanban_cards
                         SET review_status = $1,
                             updated_at = NOW()
                         WHERE id = $2",
                    )
                    .bind(rs)
                    .bind(&id)
                    .execute(pool)
                    .await
                {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": format!("{error}")})),
                    );
                }

                if let Err(error) = reactivate_done_auto_queue_entries_pg(pool, &id).await {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": format!("{error}")})),
                    );
                }

                let gh_url = match sqlx::query_scalar::<_, Option<String>>(
                    "SELECT github_issue_url FROM kanban_cards WHERE id = $1",
                )
                .bind(&id)
                .fetch_optional(pool)
                .await
                {
                    Ok(value) => value.flatten(),
                    Err(error) => {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({"error": format!("{error}")})),
                        );
                    }
                };

                let card = load_card_json_pg(pool, &id).await;

                if let Some(url) = gh_url.as_deref() {
                    if let Err(error) = crate::github::reopen_issue_by_url(url).await {
                        tracing::warn!("[kanban] Failed to reopen GitHub issue {url}: {error}");
                        return (
                            StatusCode::BAD_GATEWAY,
                            Json(json!({
                                "error": format!("github issue reopen failed before reopen response: {error}"),
                                "reopened": false,
                                "github_issue_url": url,
                            })),
                        );
                    }
                }

                return match card {
                    Ok(Some(card)) => {
                        crate::server::ws::emit_event(
                            &state.broadcast_tx,
                            "kanban_card_updated",
                            card.clone(),
                        );
                        (
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
                        )
                    }
                    Ok(None) => (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": "failed to read card after reopen"})),
                    ),
                    Err(error) => (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": error})),
                    ),
                };
            }
            Err(error) => {
                let _ = clear_api_reopen_skip_preflight_on_pg(pool, &id).await;
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{error}")})),
                );
            }
        }
    }

    pg_pool_required_error()
}

/// POST /api/kanban-cards/batch-transition
///
/// Administrative endpoint. Applies the same force semantics as force-transition to
/// multiple cards, resolving targets by either explicit card IDs or GitHub
/// issue numbers. Returns per-card success/failure details.
pub async fn batch_transition(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<BatchTransitionBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Err(response) = require_explicit_bearer_token(&headers, "batch-transition") {
        return response;
    }

    let has_issue_numbers = body
        .issue_numbers
        .as_ref()
        .is_some_and(|nums| !nums.is_empty());
    let has_card_ids = body.card_ids.as_ref().is_some_and(|ids| !ids.is_empty());
    if !has_issue_numbers && !has_card_ids {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "batch-transition requires issue_numbers or card_ids"})),
        );
    }

    let Some(pg_pool) = state.pg_pool_ref() else {
        return pg_pool_required_error();
    };
    let caller_source = resolve_requesting_agent_id_with_pg(pg_pool, &headers)
        .await
        .unwrap_or_else(|| "api".to_string());
    let batch_transition_source = format!("{caller_source}:batch-transition");
    let mut targets: Vec<(String, Option<i64>)> = Vec::new();
    let mut results = Vec::new();

    if let Some(card_ids) = body.card_ids.as_ref() {
        for card_id in card_ids {
            targets.push((card_id.clone(), None));
        }
    }

    if let Some(issue_numbers) = body.issue_numbers.as_ref() {
        for issue_number in issue_numbers {
            let card_ids: Vec<String> = match sqlx::query_scalar::<_, String>(
                "SELECT id
                 FROM kanban_cards
                 WHERE github_issue_number = $1
                 ORDER BY id ASC",
            )
            .bind(issue_number)
            .fetch_all(pg_pool)
            .await
            {
                Ok(ids) => ids,
                Err(error) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": format!("{error}")})),
                    );
                }
            };
            if card_ids.is_empty() {
                results.push(json!({
                    "issue_number": issue_number,
                    "ok": false,
                    "error": format!("card not found for issue #{issue_number}"),
                }));
                continue;
            }
            for card_id in card_ids {
                targets.push((card_id, Some(*issue_number)));
            }
        }
    }

    for (card_id, issue_number) in targets {
        let pool = pg_pool;
        let terminal_cleanup =
            match sqlx::query("SELECT repo_id, assigned_agent_id FROM kanban_cards WHERE id = $1")
                .bind(&card_id)
                .fetch_optional(pool)
                .await
            {
                Ok(Some(row)) => {
                    let card_repo_id: Option<String> = row.try_get("repo_id").unwrap_or_default();
                    let card_agent_id: Option<String> =
                        row.try_get("assigned_agent_id").unwrap_or_default();
                    match crate::kanban::resolve_pipeline_with_pg(
                        pool,
                        card_repo_id.as_deref(),
                        card_agent_id.as_deref(),
                    )
                    .await
                    {
                        Ok(effective) => {
                            effective.is_terminal(&body.status)
                                && body.cancel_dispatches.unwrap_or(true)
                        }
                        Err(error) => {
                            results.push(json!({
                                "card_id": card_id,
                                "issue_number": issue_number,
                                "ok": false,
                                "error": format!("{error}"),
                            }));
                            continue;
                        }
                    }
                }
                Ok(None) => false,
                Err(error) => {
                    results.push(json!({
                        "card_id": card_id,
                        "issue_number": issue_number,
                        "ok": false,
                        "error": format!("{error}"),
                    }));
                    continue;
                }
            };

        let transition_result =
            if force_transition_needs_cleanup(&body.status, body.cancel_dispatches) {
                crate::kanban::transition_status_with_opts_and_allowed_cleanup_pg_only(
                    pool,
                    &state.engine,
                    &card_id,
                    &body.status,
                    &batch_transition_source,
                    crate::engine::transition::ForceIntent::OperatorOverride,
                    crate::kanban::AllowedOnConnMutation::ForceTransitionRevertCleanup,
                )
                .await
                .map(|(result, counts)| {
                    (
                        result,
                        (
                            counts.cancelled_dispatches,
                            counts.skipped_auto_queue_entries,
                        ),
                    )
                })
            } else if terminal_cleanup {
                crate::kanban::transition_status_with_opts_and_allowed_cleanup_pg_only(
                    pool,
                    &state.engine,
                    &card_id,
                    &body.status,
                    &batch_transition_source,
                    crate::engine::transition::ForceIntent::OperatorOverride,
                    crate::kanban::AllowedOnConnMutation::ForceTransitionTerminalCleanup,
                )
                .await
                .map(|(result, counts)| {
                    (
                        result,
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
                    &card_id,
                    &body.status,
                    &batch_transition_source,
                    crate::engine::transition::ForceIntent::OperatorOverride,
                )
                .await
                .map(|result| (result, (0, 0)))
            };

        match transition_result {
            Ok(result) => {
                crate::kanban::drain_hook_side_effects_with_backends(None, &state.engine);
                let card = match load_card_json_pg(pg_pool, &card_id).await {
                    Ok(Some(card)) => {
                        crate::server::ws::emit_event(
                            &state.broadcast_tx,
                            "kanban_card_updated",
                            card.clone(),
                        );
                        Some(card)
                    }
                    Ok(None) => None,
                    Err(error) => {
                        results.push(json!({
                            "card_id": card_id,
                            "issue_number": issue_number,
                            "ok": false,
                            "error": error,
                        }));
                        continue;
                    }
                };
                results.push(json!({
                    "card_id": card_id,
                    "issue_number": issue_number,
                    "ok": true,
                    "from": result.0.from,
                    "to": result.0.to,
                    "cancelled_dispatches": result.1.0,
                    "skipped_auto_queue_entries": result.1.1,
                    "card": card,
                }));
            }
            Err(e) => {
                results.push(json!({
                    "card_id": card_id,
                    "issue_number": issue_number,
                    "ok": false,
                    "error": format!("{e}"),
                }));
            }
        }
    }

    (StatusCode::OK, Json(json!({ "results": results })))
}

// ── Administrative force transition ──────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ForceTransitionBody {
    pub status: String,
    pub cancel_dispatches: Option<bool>,
}

fn force_transition_needs_cleanup(target_status: &str, cancel_dispatches: Option<bool>) -> bool {
    matches!(target_status, "backlog" | "ready") && cancel_dispatches.unwrap_or(true)
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn count_live_auto_queue_entries_for_card_on_conn(
    conn: &sqlite_test::Connection,
    card_id: &str,
) -> anyhow::Result<usize> {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*)
             FROM auto_queue_entries
             WHERE kanban_card_id = ?1
               AND status IN ('pending', 'dispatched')
               AND run_id IN (
                   SELECT id FROM auto_queue_runs WHERE status IN ('active', 'paused')
               )",
            [card_id],
            |row| row.get(0),
        )
        .map_err(|error| anyhow::anyhow!("count live auto-queue entries for {card_id}: {error}"))?;
    Ok(count.max(0) as usize)
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn clear_force_transition_terminalized_links_on_conn(
    conn: &sqlite_test::Connection,
    card_id: &str,
) -> anyhow::Result<()> {
    conn.execute(
        "UPDATE auto_queue_entries
         SET dispatch_id = NULL,
             slot_index = NULL,
             dispatched_at = NULL,
             completed_at = COALESCE(completed_at, datetime('now'))
         WHERE kanban_card_id = ?1
           AND status = 'skipped'
           AND dispatch_id IS NOT NULL
           AND run_id IN (
               SELECT id FROM auto_queue_runs WHERE status IN ('active', 'paused')
           )",
        [card_id],
    )
    .map_err(|error| {
        anyhow::anyhow!(
            "clear force-transition terminalized auto-queue links for {card_id}: {error}"
        )
    })?;
    Ok(())
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn cleanup_force_transition_revert_on_conn(
    conn: &sqlite_test::Connection,
    card_id: &str,
    target_status: &str,
) -> anyhow::Result<(usize, usize)> {
    let reason = format!("force-transition to {target_status}");
    // Model 2: generic cancel keeps the dispatch pointer for provenance and to
    // avoid silently re-queuing abandoned work. Force-transition cleanup owns
    // the explicit terminal cleanup, so it snapshots the live-entry count up
    // front and then clears any skipped entry links left behind by cancel's
    // terminal side-effect.
    let skipped_auto_queue_entries = count_live_auto_queue_entries_for_card_on_conn(conn, card_id)?;
    let cancelled_dispatches =
        crate::dispatch::cancel_active_dispatches_for_card_on_conn(conn, card_id, Some(&reason))?;
    skip_live_auto_queue_entries_for_card_legacy(conn, card_id)?;
    clear_force_transition_terminalized_links_on_conn(conn, card_id)?;
    crate::kanban::cleanup_force_transition_revert_fields_on_conn(conn, card_id)?;

    Ok((cancelled_dispatches, skipped_auto_queue_entries))
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn skip_live_auto_queue_entries_for_card_legacy(
    conn: &sqlite_test::Connection,
    card_id: &str,
) -> sqlite_test::Result<usize> {
    let mut stmt = conn.prepare(
        "SELECT id FROM auto_queue_entries
         WHERE kanban_card_id = ?1
           AND status IN ('pending', 'dispatched')
           AND run_id IN (SELECT id FROM auto_queue_runs WHERE status IN ('active', 'paused'))",
    )?;
    let entry_ids: Vec<String> = stmt
        .query_map([card_id], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    drop(stmt);

    let mut changed = 0usize;
    for entry_id in entry_ids {
        if conn.execute(
            "UPDATE auto_queue_entries
                 SET status = 'skipped',
                     updated_at = datetime('now'),
                     completed_at = COALESCE(completed_at, datetime('now'))
                 WHERE id = ?1 AND status IN ('pending', 'dispatched')",
            [&entry_id],
        )? > 0
        {
            changed += 1;
        }
    }

    Ok(changed)
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn move_auto_queue_entry_to_dispatched_on_conn(
    conn: &sqlite_test::Connection,
    entry_id: &str,
    trigger_source: &str,
    options: &crate::db::auto_queue::EntryStatusUpdateOptions,
) -> sqlite_test::Result<()> {
    conn.execute(
        "UPDATE auto_queue_entries
         SET status = 'dispatched',
             dispatch_id = COALESCE(?2, dispatch_id),
             slot_index = COALESCE(?3, slot_index),
             dispatched_at = COALESCE(dispatched_at, datetime('now')),
             completed_at = NULL,
             updated_at = datetime('now')
         WHERE id = ?1 AND status IN ('pending', 'dispatched', 'done')",
        sqlite_test::params![entry_id, options.dispatch_id, options.slot_index],
    )?;
    let _ = trigger_source;
    Ok(())
}

async fn move_auto_queue_entry_to_dispatched_on_pg(
    pool: &sqlx::PgPool,
    entry_id: &str,
    trigger_source: &str,
    options: &crate::db::auto_queue::EntryStatusUpdateOptions,
) -> Result<(), String> {
    crate::db::auto_queue::reactivate_done_entry_on_pg(pool, entry_id, trigger_source, options)
        .await
        .map(|_| ())
}

async fn reactivate_done_auto_queue_entries_pg(
    pool: &sqlx::PgPool,
    card_id: &str,
) -> anyhow::Result<()> {
    let entry_ids = sqlx::query_scalar::<_, String>(
        "SELECT id
         FROM auto_queue_entries
         WHERE kanban_card_id = $1
           AND status = 'done'",
    )
    .bind(card_id)
    .fetch_all(pool)
    .await
    .map_err(|error| {
        anyhow::anyhow!("load postgres done auto-queue entries for {card_id}: {error}")
    })?;

    for entry_id in entry_ids {
        move_auto_queue_entry_to_dispatched_on_pg(
            pool,
            &entry_id,
            "api_reopen",
            &crate::db::auto_queue::EntryStatusUpdateOptions::default(),
        )
        .await
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    }

    Ok(())
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn load_card_metadata_map_on_conn(
    conn: &sqlite_test::Connection,
    card_id: &str,
) -> anyhow::Result<serde_json::Map<String, serde_json::Value>> {
    let metadata_raw: Option<String> = conn.query_row(
        "SELECT metadata FROM kanban_cards WHERE id = ?1",
        [card_id],
        |row| row.get(0),
    )?;

    match metadata_raw {
        Some(raw) if !raw.trim().is_empty() => {
            let value: serde_json::Value = serde_json::from_str(&raw)?;
            Ok(value.as_object().cloned().unwrap_or_default())
        }
        _ => Ok(serde_json::Map::new()),
    }
}

async fn load_card_metadata_map_pg(
    pool: &sqlx::PgPool,
    card_id: &str,
) -> anyhow::Result<serde_json::Map<String, serde_json::Value>> {
    let metadata_raw = sqlx::query_scalar::<_, Option<String>>(
        "SELECT metadata::text FROM kanban_cards WHERE id = $1",
    )
    .bind(card_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| anyhow::anyhow!("load postgres metadata for {card_id}: {error}"))?
    .flatten();

    match metadata_raw {
        Some(raw) if !raw.trim().is_empty() => {
            let value: serde_json::Value = serde_json::from_str(&raw)?;
            Ok(value.as_object().cloned().unwrap_or_default())
        }
        _ => Ok(serde_json::Map::new()),
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn save_card_metadata_map_on_conn(
    conn: &sqlite_test::Connection,
    card_id: &str,
    metadata: &serde_json::Map<String, serde_json::Value>,
) -> anyhow::Result<()> {
    if metadata.is_empty() {
        conn.execute(
            "UPDATE kanban_cards SET metadata = NULL WHERE id = ?1",
            [card_id],
        )?;
    } else {
        conn.execute(
            "UPDATE kanban_cards SET metadata = ?1 WHERE id = ?2",
            sqlite_test::params![serde_json::to_string(metadata)?, card_id],
        )?;
    }
    Ok(())
}

async fn save_card_metadata_map_pg(
    pool: &sqlx::PgPool,
    card_id: &str,
    metadata: &serde_json::Map<String, serde_json::Value>,
) -> anyhow::Result<()> {
    if metadata.is_empty() {
        sqlx::query(
            "UPDATE kanban_cards
             SET metadata = NULL,
                 updated_at = NOW()
             WHERE id = $1",
        )
        .bind(card_id)
        .execute(pool)
        .await
        .map_err(|error| anyhow::anyhow!("clear postgres metadata for {card_id}: {error}"))?;
    } else {
        sqlx::query(
            "UPDATE kanban_cards
             SET metadata = $1::jsonb,
                 updated_at = NOW()
             WHERE id = $2",
        )
        .bind(serde_json::to_string(metadata)?)
        .bind(card_id)
        .execute(pool)
        .await
        .map_err(|error| anyhow::anyhow!("save postgres metadata for {card_id}: {error}"))?;
    }
    Ok(())
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn mark_api_reopen_skip_preflight_on_conn(
    conn: &sqlite_test::Connection,
    card_id: &str,
) -> anyhow::Result<()> {
    let mut metadata = load_card_metadata_map_on_conn(conn, card_id)?;
    metadata.insert(
        "skip_preflight_once".to_string(),
        serde_json::Value::String("api_reopen".to_string()),
    );
    save_card_metadata_map_on_conn(conn, card_id, &metadata)
}

async fn mark_api_reopen_skip_preflight_on_pg(
    pool: &sqlx::PgPool,
    card_id: &str,
) -> anyhow::Result<()> {
    let mut metadata = load_card_metadata_map_pg(pool, card_id).await?;
    metadata.insert(
        "skip_preflight_once".to_string(),
        serde_json::Value::String("api_reopen".to_string()),
    );
    save_card_metadata_map_pg(pool, card_id, &metadata).await
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn clear_api_reopen_skip_preflight_on_conn(
    conn: &sqlite_test::Connection,
    card_id: &str,
) -> anyhow::Result<()> {
    let mut metadata = load_card_metadata_map_on_conn(conn, card_id)?;
    metadata.remove("skip_preflight_once");
    save_card_metadata_map_on_conn(conn, card_id, &metadata)
}

async fn clear_api_reopen_skip_preflight_on_pg(
    pool: &sqlx::PgPool,
    card_id: &str,
) -> anyhow::Result<()> {
    let mut metadata = load_card_metadata_map_pg(pool, card_id).await?;
    metadata.remove("skip_preflight_once");
    save_card_metadata_map_pg(pool, card_id, &metadata).await
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn consume_api_reopen_preflight_skip_on_conn(
    conn: &sqlite_test::Connection,
    card_id: &str,
) -> anyhow::Result<()> {
    let mut metadata = load_card_metadata_map_on_conn(conn, card_id)?;
    if matches!(
        metadata
            .get("skip_preflight_once")
            .and_then(|value| value.as_str()),
        Some("api_reopen") | Some("pmd_reopen")
    ) {
        metadata.remove("skip_preflight_once");
        metadata.insert(
            "preflight_status".to_string(),
            serde_json::Value::String("skipped".to_string()),
        );
        metadata.insert(
            "preflight_summary".to_string(),
            serde_json::Value::String("Skipped for API reopen".to_string()),
        );
        metadata.insert(
            "preflight_checked_at".to_string(),
            serde_json::Value::String(chrono::Utc::now().to_rfc3339()),
        );
        save_card_metadata_map_on_conn(conn, card_id, &metadata)?;
    }
    Ok(())
}

async fn consume_api_reopen_preflight_skip_on_pg(
    pool: &sqlx::PgPool,
    card_id: &str,
) -> anyhow::Result<()> {
    let mut metadata = load_card_metadata_map_pg(pool, card_id).await?;
    if matches!(
        metadata
            .get("skip_preflight_once")
            .and_then(|value| value.as_str()),
        Some("api_reopen") | Some("pmd_reopen")
    ) {
        metadata.remove("skip_preflight_once");
        metadata.insert(
            "preflight_status".to_string(),
            serde_json::Value::String("skipped".to_string()),
        );
        metadata.insert(
            "preflight_summary".to_string(),
            serde_json::Value::String("Skipped for API reopen".to_string()),
        );
        metadata.insert(
            "preflight_checked_at".to_string(),
            serde_json::Value::String(chrono::Utc::now().to_rfc3339()),
        );
        save_card_metadata_map_pg(pool, card_id, &metadata).await?;
    }
    Ok(())
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn clear_reopen_preflight_cache_on_conn(
    conn: &sqlite_test::Connection,
    card_id: &str,
) -> anyhow::Result<()> {
    let mut metadata = load_card_metadata_map_on_conn(conn, card_id)?;
    for key in [
        "skip_preflight_once",
        "preflight_status",
        "preflight_summary",
        "preflight_checked_at",
        "consultation_status",
        "consultation_result",
    ] {
        metadata.remove(key);
    }
    save_card_metadata_map_on_conn(conn, card_id, &metadata)
}

async fn clear_reopen_preflight_cache_on_pg(
    pool: &sqlx::PgPool,
    card_id: &str,
) -> anyhow::Result<()> {
    let mut metadata = load_card_metadata_map_pg(pool, card_id).await?;
    for key in [
        "skip_preflight_once",
        "preflight_status",
        "preflight_summary",
        "preflight_checked_at",
        "consultation_status",
        "consultation_result",
    ] {
        metadata.remove(key);
    }
    save_card_metadata_map_pg(pool, card_id, &metadata).await
}

/// Snapshot active dispatches (status IN ('pending','dispatched')) for a card.
///
/// Used by /transition (#1442) to report which dispatch IDs were cancelled by
/// the cleanup paths in the response payload.
async fn active_dispatch_ids_for_card_pg(
    pool: &sqlx::PgPool,
    card_id: &str,
) -> anyhow::Result<Vec<String>> {
    let rows = sqlx::query_scalar::<_, String>(
        "SELECT id
         FROM task_dispatches
         WHERE kanban_card_id = $1
           AND status IN ('pending', 'dispatched')",
    )
    .bind(card_id)
    .fetch_all(pool)
    .await
    .map_err(|error| anyhow::anyhow!("load active dispatches for {card_id}: {error}"))?;
    Ok(rows)
}

/// Filter the input dispatch IDs down to the ones currently in `cancelled`
/// status. Used after a /transition cleanup to confirm which previously-active
/// dispatches were actually terminated by the cleanup path (#1442).
async fn cancelled_dispatch_ids_among_pg(
    pool: &sqlx::PgPool,
    dispatch_ids: &[String],
) -> anyhow::Result<Vec<String>> {
    if dispatch_ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query_scalar::<_, String>(
        "SELECT id
         FROM task_dispatches
         WHERE id = ANY($1)
           AND status = 'cancelled'",
    )
    .bind(dispatch_ids)
    .fetch_all(pool)
    .await
    .map_err(|error| anyhow::anyhow!("filter cancelled dispatches: {error}"))?;
    Ok(rows)
}

async fn clear_all_threads_pg(pool: &sqlx::PgPool, card_id: &str) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE kanban_cards
         SET channel_thread_map = NULL,
             active_thread_id = NULL,
             updated_at = NOW()
         WHERE id = $1",
    )
    .bind(card_id)
    .execute(pool)
    .await
    .map_err(|error| anyhow::anyhow!("clear postgres thread state for {card_id}: {error}"))?;
    Ok(())
}

/// POST /api/kanban-cards/:id/force-transition
///
/// Administrative endpoint. Bypasses dispatch validation.
/// Requires an explicit Bearer token (no same-origin bypass).
///
/// This endpoint is single-call complete. Do NOT chain /redispatch,
/// /retry, or /auto-queue/generate after it — that creates duplicate
/// dispatches (see #1442 incident). Inspect `cancelled_dispatch_ids`,
/// `created_dispatch_id`, and `next_action_hint` in the response to
/// determine whether any follow-up is genuinely required.
pub async fn force_transition(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<ForceTransitionBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Err(response) = require_explicit_bearer_token(&headers, "force-transition") {
        return response;
    }

    let needs_cleanup = force_transition_needs_cleanup(&body.status, body.cancel_dispatches);
    let target_status = body.status;
    let mut cleanup_counts = (0, 0);
    let pool = match state.pg_pool_ref() {
        Some(pool) => pool,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "force-transition requires postgres pool (#1239)"})),
            );
        }
    };
    let caller_source = resolve_requesting_agent_id_with_pg(pool, &headers)
        .await
        .unwrap_or_else(|| "api".to_string());
    // Snapshot active dispatch IDs before transition so we can report which
    // were cancelled by the cleanup paths (#1442). The cleanup helpers report
    // counts but not IDs; we reconcile by querying the post-transition status
    // of each pre-existing active dispatch and surfacing the ones now in
    // `cancelled` state.
    let pre_active_dispatch_ids: Vec<String> = active_dispatch_ids_for_card_pg(pool, &id)
        .await
        .unwrap_or_default();
    let pre_latest_dispatch_id: Option<String> = sqlx::query_scalar::<_, Option<String>>(
        "SELECT latest_dispatch_id FROM kanban_cards WHERE id = $1",
    )
    .bind(&id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .flatten();
    let terminal_cleanup =
        match sqlx::query("SELECT repo_id, assigned_agent_id FROM kanban_cards WHERE id = $1")
            .bind(&id)
            .fetch_optional(pool)
            .await
        {
            Ok(Some(row)) => {
                let card_repo_id: Option<String> = row.try_get("repo_id").unwrap_or_default();
                let card_agent_id: Option<String> =
                    row.try_get("assigned_agent_id").unwrap_or_default();
                match crate::kanban::resolve_pipeline_with_pg(
                    pool,
                    card_repo_id.as_deref(),
                    card_agent_id.as_deref(),
                )
                .await
                {
                    Ok(effective) => {
                        effective.is_terminal(&target_status)
                            && body.cancel_dispatches.unwrap_or(true)
                    }
                    Err(error) => {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({"error": format!("{error}")})),
                        );
                    }
                }
            }
            Ok(None) => false,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{error}")})),
                );
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
            let (cancelled_dispatches, skipped_auto_queue_entries) = cleanup_counts;
            crate::kanban::drain_hook_side_effects_with_backends(None, &state.engine);

            // Reconcile the pre-transition active dispatch snapshot against
            // current state to surface concrete cancelled IDs (#1442).
            let cancelled_dispatch_ids =
                if cancelled_dispatches > 0 && !pre_active_dispatch_ids.is_empty() {
                    cancelled_dispatch_ids_among_pg(pool, &pre_active_dispatch_ids)
                        .await
                        .unwrap_or_default()
                } else {
                    Vec::new()
                };

            // Detect a brand-new dispatch that may have been kicked off by
            // hooks fired during the transition (e.g. on_enter).
            let post_latest_dispatch_id: Option<String> = sqlx::query_scalar::<_, Option<String>>(
                "SELECT latest_dispatch_id FROM kanban_cards WHERE id = $1",
            )
            .bind(&id)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten()
            .flatten();
            let created_dispatch_id = match (
                pre_latest_dispatch_id.as_deref(),
                post_latest_dispatch_id.as_deref(),
            ) {
                (Some(prev), Some(curr)) if prev != curr => Some(curr.to_string()),
                (None, Some(curr)) => Some(curr.to_string()),
                _ => None,
            };

            let next_action_hint = if created_dispatch_id.is_some() {
                "none_required".to_string()
            } else if matches!(result.to.as_str(), "ready" | "requested" | "in_progress") {
                // Caller may want to dispatch the now-ready card via the
                // queue. The transition itself is complete; this hint surfaces
                // the natural follow-up so callers do not silently chain
                // /redispatch (which would create duplicates — #1442).
                "call /api/auto-queue/generate to dispatch newly-ready card".to_string()
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
                    (
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
                    )
                }
                Err(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                ),
            }
        }
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("{e}")})),
        ),
    }
}

// ── #1065 param standardization tests ────────────────────────────────
// UpdateCardBody canonical field is `assignee_agent_id` (snake_case).
// Legacy `assigned_agent_id` still accepted via serde alias during migration.
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod param_standardization_tests {
    use super::UpdateCardBody;

    #[test]
    fn param_standardization_update_card_body_accepts_assignee_agent_id() {
        let payload = r#"{"assignee_agent_id":"ch-td"}"#;
        let body: UpdateCardBody =
            serde_json::from_str(payload).expect("canonical assignee_agent_id must parse");
        assert_eq!(body.assignee_agent_id.as_deref(), Some("ch-td"));
    }

    #[test]
    fn param_standardization_update_card_body_accepts_legacy_assigned_agent_id_alias() {
        let payload = r#"{"assigned_agent_id":"ch-td"}"#;
        let body: UpdateCardBody = serde_json::from_str(payload)
            .expect("legacy assigned_agent_id payload must still parse via serde alias");
        assert_eq!(body.assignee_agent_id.as_deref(), Some("ch-td"));
    }

    #[test]
    fn param_standardization_update_card_body_no_duplicate_agent_fields() {
        // Canonical path is single-field: one struct field with one alias.
        // This guards against re-introducing a separate `assigned_agent_id` field.
        let payload = r#"{"assignee_agent_id":"canonical"}"#;
        let body: UpdateCardBody = serde_json::from_str(payload).expect("must parse");
        assert_eq!(body.assignee_agent_id.as_deref(), Some("canonical"));
    }
}
