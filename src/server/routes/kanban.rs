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
    pub assigned_agent_id: Option<String>,
    /// Alias for assigned_agent_id (frontend sends this name)
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

fn load_active_turn_targets_for_card(
    db: &crate::db::Db,
    card_id: &str,
) -> anyhow::Result<Vec<ActiveTurnTarget>> {
    let conn = db.lock().map_err(|error| anyhow::anyhow!("{error}"))?;
    let mut stmt = conn.prepare(
        "SELECT DISTINCT session_key, provider, thread_channel_id
         FROM sessions
         WHERE active_dispatch_id IN (
             SELECT id FROM task_dispatches
             WHERE kanban_card_id = ?1 AND status IN ('pending', 'dispatched')
         )",
    )?;
    let rows = stmt.query_map([card_id], |row| {
        Ok(ActiveTurnTarget {
            session_key: row.get(0)?,
            provider: row.get(1)?,
            thread_channel_id: row.get(2)?,
        })
    })?;
    Ok(rows.filter_map(|row| row.ok()).collect())
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

        if let Some(pool) = state.pg_pool.as_ref() {
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
        }

        if let Ok(conn) = state.db.lock() {
            conn.execute(
                "UPDATE sessions
                 SET status = 'disconnected', active_dispatch_id = NULL, claude_session_id = NULL
                 WHERE session_key = ?1",
                [&target.session_key],
            )
            .ok();
        }
    }
}

async fn transition_card_to_backlog_with_cleanup(
    state: &AppState,
    card_id: &str,
    source: &str,
) -> anyhow::Result<crate::kanban::TransitionResult> {
    let turn_targets = if let Some(pool) = state.pg_pool.as_ref() {
        load_active_turn_targets_for_card_pg(pool, card_id).await?
    } else {
        load_active_turn_targets_for_card(&state.db, card_id)?
    };
    let result = if let Some(pool) = state.pg_pool.as_ref() {
        crate::kanban::transition_status_with_opts_and_allowed_cleanup_pg(
            Some(&state.db),
            pool,
            &state.engine,
            card_id,
            "backlog",
            source,
            crate::engine::transition::ForceIntent::SystemRecovery,
            crate::kanban::AllowedOnConnMutation::ForceTransitionRevertCleanup,
        )
        .await
        .map(|(result, _)| result)?
    } else {
        crate::kanban::transition_status_with_opts_and_on_conn(
            &state.db,
            &state.engine,
            card_id,
            "backlog",
            source,
            crate::engine::transition::ForceIntent::SystemRecovery,
            // allowed: backlog rewind must clear review/dispatch residue atomically.
            crate::kanban::AllowedOnConnMutation::ForceTransitionRevertCleanup,
            |conn| {
                cleanup_force_transition_revert_on_conn(conn, card_id, "backlog")?;
                Ok(())
            },
        )?
    };
    cancel_turn_targets(state, &turn_targets, "kanban backlog revert").await;
    Ok(result)
}

fn load_retry_dispatch_spec(
    conn: &libsql_rusqlite::Connection,
    card_id: &str,
) -> Option<(String, String, String)> {
    let card_row: (Option<String>, String, Option<String>) = conn
        .query_row(
            "SELECT assigned_agent_id, title, latest_dispatch_id FROM kanban_cards WHERE id = ?1",
            [card_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .ok()?;

    let (card_agent_id, card_title, latest_dispatch_id) = card_row;
    let latest_dispatch = latest_dispatch_id
        .as_deref()
        .and_then(|dispatch_id| {
            conn.query_row(
                "SELECT to_agent_id, dispatch_type, title FROM task_dispatches WHERE id = ?1",
                [dispatch_id],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .ok()
        })
        .or_else(|| {
            conn.query_row(
                "SELECT to_agent_id, dispatch_type, title
             FROM task_dispatches
             WHERE kanban_card_id = ?1
             ORDER BY datetime(created_at) DESC, rowid DESC
             LIMIT 1",
                [card_id],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .ok()
        });

    let (dispatch_agent_id, dispatch_type, dispatch_title) =
        latest_dispatch.unwrap_or((None, None, None));

    let effective_agent_id = dispatch_agent_id.or(card_agent_id).unwrap_or_default();
    let effective_dispatch_type = dispatch_type
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "implementation".to_string());
    let effective_title = dispatch_title
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(card_title);

    Some((effective_agent_id, effective_dispatch_type, effective_title))
}

// ── Handlers ───────────────────────────────────────────────────

/// GET /api/kanban-cards
pub async fn list_cards(
    State(state): State<AppState>,
    Query(params): Query<ListCardsQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state
        .kanban_service()
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
    if let Some(pool) = state.pg_pool.as_ref() {
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

    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    match conn.query_row(&format!("{CARD_SELECT} WHERE kc.id = ?1"), [&id], |row| {
        card_row_to_json(row)
    }) {
        Ok(card) => (StatusCode::OK, Json(json!({"card": card}))),
        Err(libsql_rusqlite::Error::QueryReturnedNoRows) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "card not found"})),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        ),
    }
}

/// POST /api/kanban-cards
pub async fn create_card(
    State(state): State<AppState>,
    Json(body): Json<CreateCardBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let id = uuid::Uuid::new_v4().to_string();
    let priority = body.priority.unwrap_or_else(|| "medium".to_string());

    if let Some(pool) = state.pg_pool.as_ref() {
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

    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    // Pipeline-driven initial state
    crate::pipeline::ensure_loaded();
    let initial_state = crate::pipeline::get().initial_state().to_string();
    let result = conn.execute(
        "INSERT INTO kanban_cards (id, repo_id, title, status, priority, github_issue_url, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, datetime('now'), datetime('now'))",
        libsql_rusqlite::params![id, body.repo_id, body.title, initial_state, priority, body.github_issue_url],
    );

    if let Err(e) = result {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        );
    }

    match conn.query_row(&format!("{CARD_SELECT} WHERE kc.id = ?1"), [&id], |row| {
        card_row_to_json(row)
    }) {
        Ok(card) => {
            crate::server::ws::emit_event(&state.broadcast_tx, "kanban_card_created", card.clone());
            (StatusCode::CREATED, Json(json!({"card": card})))
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        ),
    }
}

/// PATCH /api/kanban-cards/:id
pub async fn update_card(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateCardBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let old_status: Option<String> = {
        let conn = match state.db.lock() {
            Ok(c) => c,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
        };

        conn.query_row(
            "SELECT status FROM kanban_cards WHERE id = ?1",
            [&id],
            |row| row.get(0),
        )
        .ok()
    };

    if old_status.is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "card not found"})),
        );
    }
    let old_status = old_status.unwrap();

    let has_non_status_updates = body.title.is_some()
        || body.priority.is_some()
        || body.assigned_agent_id.is_some()
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
                            "Manual status transitions only allow backlog → ready and any → backlog (requested: {} → {})",
                            old_status,
                            new_s,
                        ),
                    })),
                );
            }

            let transition_result = if new_s == "backlog" {
                transition_card_to_backlog_with_cleanup(&state, &id, "api:manual-backlog").await
            } else if let Some(pool) = state.pg_pool.as_ref() {
                crate::kanban::transition_status_with_opts_pg(
                    Some(&state.db),
                    pool,
                    &state.engine,
                    &id,
                    new_s,
                    "api",
                    crate::engine::transition::ForceIntent::None,
                )
                .await
            } else {
                crate::kanban::transition_status_with_opts(
                    &state.db,
                    &state.engine,
                    &id,
                    new_s,
                    "api",
                    crate::engine::transition::ForceIntent::None,
                )
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
        let mut sets: Vec<String> = Vec::new();
        let mut values: Vec<Box<dyn libsql_rusqlite::types::ToSql>> = Vec::new();
        let mut idx = 1;

        macro_rules! push_field {
            ($field:expr, $val:expr) => {
                if let Some(ref v) = $val {
                    sets.push(format!("{} = ?{}", $field, idx));
                    values.push(Box::new(v.clone()));
                    idx += 1;
                }
            };
        }

        push_field!("title", body.title);
        push_field!("priority", body.priority);
        let agent_id = body.assigned_agent_id.or(body.assignee_agent_id);
        push_field!("assigned_agent_id", agent_id);
        push_field!("repo_id", body.repo_id);
        push_field!("github_issue_url", body.github_issue_url);
        push_field!("description", body.description);

        let meta_str = body
            .metadata
            .as_ref()
            .map(|metadata| serde_json::to_string(metadata).unwrap_or_default())
            .or(body.metadata_json);
        if let Some(ref metadata) = meta_str {
            sets.push(format!("metadata = ?{}", idx));
            values.push(Box::new(metadata.clone()));
            idx += 1;
        }

        push_field!("review_status", body.review_status);
        push_field!("review_notes", body.review_notes);

        sets.push(format!("updated_at = datetime('now')"));
        let sql = format!(
            "UPDATE kanban_cards SET {} WHERE id = ?{}",
            sets.join(", "),
            idx
        );
        values.push(Box::new(id.clone()));

        let conn = match state.db.lock() {
            Ok(c) => c,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
        };

        let params_ref: Vec<&dyn libsql_rusqlite::types::ToSql> =
            values.iter().map(|v| v.as_ref()).collect();
        match conn.execute(&sql, params_ref.as_slice()) {
            Ok(0) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "card not found"})),
                );
            }
            Ok(_) => {}
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
        }
    }

    let conn = state.db.lock().unwrap();
    let card = conn.query_row(&format!("{CARD_SELECT} WHERE kc.id = ?1"), [&id], |row| {
        card_row_to_json(row)
    });
    drop(conn);

    // #108: Drain pending intents from hooks fired during transition_status_with_opts.
    // fire_dynamic_hooks fires policy hooks that may create dispatch intents, but
    // doesn't drain them itself. drain_hook_side_effects now also queues Discord
    // notifications for created dispatches, replacing the previous latest_dispatch_id
    // re-query that was susceptible to race conditions.
    crate::kanban::drain_hook_side_effects(&state.db, &state.engine);

    match card {
        Ok(c) => {
            crate::server::ws::emit_event(&state.broadcast_tx, "kanban_card_updated", c.clone());
            (StatusCode::OK, Json(json!({"card": c})))
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        ),
    }
}

/// POST /api/kanban-cards/:id/assign
pub async fn assign_card(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<AssignCardBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(pool) = state.pg_pool.as_ref() {
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
                    if let Err(error) = crate::kanban::transition_status_with_opts_pg(
                        Some(&state.db),
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
            } else if let Err(error) = crate::kanban::transition_status_with_opts_pg(
                Some(&state.db),
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

    let old_status: Option<String> = {
        let conn = match state.db.lock() {
            Ok(c) => c,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
        };
        conn.query_row(
            "SELECT status FROM kanban_cards WHERE id = ?1",
            [&id],
            |row| row.get(0),
        )
        .ok()
    };

    if old_status.is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "card not found"})),
        );
    }
    let old_status = old_status.unwrap();

    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    // Pipeline-driven: assign to the first dispatchable state (or second state)
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
    // #155: Split into assignee update (metadata) + status transition via reducer
    match conn.execute(
        "UPDATE kanban_cards SET assigned_agent_id = ?1, updated_at = datetime('now') WHERE id = ?2",
        libsql_rusqlite::params![body.agent_id, id],
    ) {
        Ok(0) => {
            return (StatusCode::NOT_FOUND, Json(json!({"error": "card not found"})));
        }
        Ok(_) => {}
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    }
    drop(conn);

    // #255: Walk through free transitions to reach the dispatchable state.
    // e.g., backlog → ready → requested (each step fires hooks, clocks, audit).
    if old_status != ready_state {
        if let Some(path) = pipeline.free_path_to_dispatchable(&old_status) {
            for step in &path {
                if let Err(e) = crate::kanban::transition_status_with_opts(
                    &state.db,
                    &state.engine,
                    &id,
                    step,
                    "assign",
                    crate::engine::transition::ForceIntent::None,
                ) {
                    tracing::warn!("[assign_card] walk step to '{step}' failed: {e}");
                    break;
                }
            }
        } else {
            // Direct transition (already dispatchable or single hop)
            if let Err(e) = crate::kanban::transition_status_with_opts(
                &state.db,
                &state.engine,
                &id,
                &ready_state,
                "assign",
                crate::engine::transition::ForceIntent::None,
            ) {
                tracing::warn!("[assign_card] transition failed: {e}");
            }
        }
    }

    let card = state.db.lock().ok().and_then(|conn| {
        conn.query_row(&format!("{CARD_SELECT} WHERE kc.id = ?1"), [&id], |row| {
            card_row_to_json(row)
        })
        .ok()
    });

    match card {
        Some(c) => {
            crate::server::ws::emit_event(&state.broadcast_tx, "kanban_card_updated", c.clone());
            (StatusCode::OK, Json(json!({"card": c})))
        }
        None => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "failed to read card after assign"})),
        ),
    }
}

/// DELETE /api/kanban-cards/:id
pub async fn delete_card(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(pool) = state.pg_pool.as_ref() {
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

    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    match conn.execute("DELETE FROM kanban_cards WHERE id = ?1", [&id]) {
        Ok(0) => (
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
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        ),
    }
}

/// POST /api/kanban-cards/:id/retry
pub async fn retry_card(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<RetryCardBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    // 1. Update assignee if provided
    {
        let conn = match state.db.lock() {
            Ok(c) => c,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
        };

        let exists: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM kanban_cards WHERE id = ?1",
                [&id],
                |row| row.get(0),
            )
            .unwrap_or(false);
        if !exists {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "card not found"})),
            );
        }

        let (stored_agent_id, retry_dispatch_type, retry_title) =
            match load_retry_dispatch_spec(&conn, &id) {
                Some(spec) => spec,
                None => {
                    return (
                        StatusCode::NOT_FOUND,
                        Json(json!({"error": "card not found"})),
                    );
                }
            };

        conn.execute_batch("BEGIN").ok();

        // Cancel existing pending/dispatched dispatch
        let existing_dispatch_id: Option<String> = conn
            .query_row(
                "SELECT latest_dispatch_id FROM kanban_cards WHERE id = ?1",
                [&id],
                |row| row.get(0),
            )
            .ok()
            .flatten();
        if let Some(ref did) = existing_dispatch_id {
            if let Err(e) =
                crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_conn(&conn, did, None)
            {
                conn.execute_batch("ROLLBACK").ok();
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
        }

        // #155: Clear latest_dispatch_id via intent, assignee via direct (not CardState)
        use crate::engine::transition::{TransitionIntent as TI2, execute_intent_on_conn as exec2};
        let agent_id_for_dispatch: String = if let Some(ref agent_id) = body.assignee_agent_id {
            if let Err(e) = conn.execute(
                "UPDATE kanban_cards SET assigned_agent_id = ?1, updated_at = datetime('now') WHERE id = ?2",
                libsql_rusqlite::params![agent_id, id],
            ) {
                conn.execute_batch("ROLLBACK").ok();
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
            if let Err(e) = exec2(
                &conn,
                &TI2::SetLatestDispatchId {
                    card_id: id.clone(),
                    dispatch_id: None,
                },
            ) {
                conn.execute_batch("ROLLBACK").ok();
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
            agent_id.clone()
        } else {
            let current: String = conn
                .query_row(
                    "SELECT COALESCE(assigned_agent_id, '') FROM kanban_cards WHERE id = ?1",
                    [&id],
                    |row| row.get(0),
                )
                .unwrap_or_default();
            if let Err(e) = exec2(
                &conn,
                &TI2::SetLatestDispatchId {
                    card_id: id.clone(),
                    dispatch_id: None,
                },
            ) {
                conn.execute_batch("ROLLBACK").ok();
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
            current
        };
        if let Err(e) = conn.execute_batch("COMMIT") {
            conn.execute_batch("ROLLBACK").ok();
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
        // Note: status → 'requested' is handled by create_dispatch() below

        let dispatch_agent_id = if agent_id_for_dispatch.is_empty() {
            stored_agent_id
        } else {
            agent_id_for_dispatch
        };
        let card_id_owned = id.clone();
        drop(conn);

        // Create dispatch directly (bypass policy to avoid from===requested skip)
        if !dispatch_agent_id.is_empty() {
            let _retry_result = crate::dispatch::create_dispatch(
                &state.db,
                &state.engine,
                &card_id_owned,
                &dispatch_agent_id,
                &retry_dispatch_type,
                &retry_title,
                &json!({"retry": true, "preserved_dispatch_type": retry_dispatch_type.clone()}),
            );
        }
    } // drop conn lock

    // Return updated card
    let conn = state.db.lock().unwrap();
    match conn.query_row(&format!("{CARD_SELECT} WHERE kc.id = ?1"), [&id], |row| {
        card_row_to_json(row)
    }) {
        Ok(card) => {
            crate::server::ws::emit_event(&state.broadcast_tx, "kanban_card_updated", card.clone());
            (StatusCode::OK, Json(json!({"card": card})))
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        ),
    }
}

/// POST /api/kanban-cards/:id/redispatch
pub async fn redispatch_card(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(_body): Json<RedispatchCardBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    // 1. Cancel current dispatch, then transition to "requested"
    // The OnCardTransition hook (kanban-rules.js) creates the new dispatch + Discord message
    {
        let conn = match state.db.lock() {
            Ok(c) => c,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
        };

        let (agent_id, dispatch_type, dispatch_title) = match load_retry_dispatch_spec(&conn, &id) {
            Some(spec) => spec,
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "card not found"})),
                );
            }
        };

        conn.execute_batch("BEGIN").ok();

        // Cancel existing dispatch
        let dispatch_id: Option<String> = conn
            .query_row(
                "SELECT latest_dispatch_id FROM kanban_cards WHERE id = ?1",
                [&id],
                |row| row.get(0),
            )
            .ok()
            .flatten();
        if let Some(ref did) = dispatch_id {
            if let Err(e) =
                crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_conn(&conn, did, None)
            {
                conn.execute_batch("ROLLBACK").ok();
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
        }

        // #155: Clear review_status and latest_dispatch_id via intents (executor boundary)
        use crate::engine::transition::{TransitionIntent, execute_intent_on_conn};
        let clear_intents = vec![
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
        ];
        for intent in &clear_intents {
            if let Err(e) = execute_intent_on_conn(&conn, intent) {
                conn.execute_batch("ROLLBACK").ok();
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
        }
        if let Err(e) = conn.execute_batch("COMMIT") {
            conn.execute_batch("ROLLBACK").ok();
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }

        let card_id_owned = id.clone();
        drop(conn);

        // Create dispatch directly (bypass policy to avoid from===requested skip)
        if !agent_id.is_empty() {
            let _redispatch_result = crate::dispatch::create_dispatch(
                &state.db,
                &state.engine,
                &card_id_owned,
                &agent_id,
                &dispatch_type,
                &dispatch_title,
                &json!({"redispatch": true, "preserved_dispatch_type": dispatch_type.clone()}),
            );
        }
    }

    // 2. Return updated card
    let conn = state.db.lock().unwrap();
    match conn.query_row(&format!("{CARD_SELECT} WHERE kc.id = ?1"), [&id], |row| {
        card_row_to_json(row)
    }) {
        Ok(card) => {
            crate::server::ws::emit_event(&state.broadcast_tx, "kanban_card_updated", card.clone());
            (StatusCode::OK, Json(json!({"card": card})))
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        ),
    }
}

/// PATCH /api/kanban-cards/:id/defer-dod
pub async fn defer_dod(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<DeferDodBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    // Check card exists
    let exists: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM kanban_cards WHERE id = ?1",
            [&id],
            |row| row.get(0),
        )
        .unwrap_or(false);
    if !exists {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "card not found"})),
        );
    }

    // Read current deferred_dod_json
    let current: Option<String> = conn
        .query_row(
            "SELECT deferred_dod_json FROM kanban_cards WHERE id = ?1",
            [&id],
            |row| row.get(0),
        )
        .unwrap_or(None);

    let mut dod: serde_json::Value = current
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({"items": [], "verified": []}));

    // Apply items (replace entire list)
    if let Some(items) = body.items {
        dod["items"] = json!(items);
    }

    // Verify items
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

    // Unverify items
    if let Some(unverify) = body.unverify {
        if let Some(arr) = dod["verified"].as_array() {
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
    }

    // Remove items
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
        // Also remove from verified
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

    let dod_str = serde_json::to_string(&dod).unwrap_or_default();
    conn.execute(
        "UPDATE kanban_cards SET deferred_dod_json = ?1, updated_at = datetime('now') WHERE id = ?2",
        libsql_rusqlite::params![dod_str, id],
    ).ok();

    // #128: Check if all DoD items are now complete AND card is awaiting_dod.
    // If so, clear awaiting_dod and restart review (fire on_enter hooks).
    let restart_review_state: Option<String>;
    {
        let (card_status, review_status): (String, Option<String>) = conn
            .query_row(
                "SELECT status, review_status FROM kanban_cards WHERE id = ?1",
                [&id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap_or(("".to_string(), None));

        // Pipeline-driven: check if state has OnReviewEnter hook (review-like state)
        let is_review_state = {
            crate::pipeline::ensure_loaded();
            crate::pipeline::try_get()
                .and_then(|p| p.hooks_for_state(&card_status))
                .map_or(false, |h| h.on_enter.iter().any(|n| n == "OnReviewEnter"))
        };
        if is_review_state && review_status.as_deref() == Some("awaiting_dod") {
            // Check if all DoD items are verified.
            // Format: { items: ["task1", "task2"], verified: ["task1", "task2"] }
            let all_done = if let (Some(items), Some(verified)) =
                (dod["items"].as_array(), dod["verified"].as_array())
            {
                !items.is_empty() && items.iter().all(|item| verified.contains(item))
            } else {
                false
            };
            if all_done {
                // #155: Use intents for review_status mutation
                use crate::engine::transition::{TransitionIntent, execute_intent_on_conn};
                let dod_intents = vec![
                    TransitionIntent::SetReviewStatus {
                        card_id: id.clone(),
                        review_status: Some("reviewing".to_string()),
                    },
                    TransitionIntent::SyncReviewState {
                        card_id: id.clone(),
                        state: "reviewing".to_string(),
                    },
                ];
                for intent in &dod_intents {
                    execute_intent_on_conn(&conn, intent).ok();
                }
                // Clock fields not covered by intents yet — direct write for review_entered_at/awaiting_dod_at
                conn.execute(
                    "UPDATE kanban_cards SET review_entered_at = datetime('now'), awaiting_dod_at = NULL WHERE id = ?1",
                    [&id],
                ).ok();
                restart_review_state = Some(card_status);
                true
            } else {
                restart_review_state = None;
                false
            }
        } else {
            restart_review_state = None;
            false
        }
    };

    // Must drop conn before firing hooks (hooks may re-acquire DB lock)
    let card_result = conn.query_row(&format!("{CARD_SELECT} WHERE kc.id = ?1"), [&id], |row| {
        card_row_to_json(row)
    });
    drop(conn);

    // Fire on_enter hooks for the review state to trigger review dispatch creation (#134)
    if let Some(ref review_state) = restart_review_state {
        crate::kanban::fire_enter_hooks(&state.db, &state.engine, &id, review_state);
        tracing::info!(
            "[dod] Card {} DoD all-complete — restarting review from awaiting_dod",
            id
        );
    }

    match card_result {
        Ok(mut card) => {
            card["deferred_dod"] = dod;
            (StatusCode::OK, Json(json!({"card": card})))
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        ),
    }
}

/// GET /api/kanban-cards/:id/review-state
/// #117: Returns the canonical card_review_state record for a card.
pub async fn get_card_review_state(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(pool) = state.pg_pool.as_ref() {
        return match sqlx::query(
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
        };
    }

    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    match conn.query_row(
        "SELECT card_id, review_round, state, pending_dispatch_id, last_verdict, \
         last_decision, decided_by, decided_at, review_entered_at, updated_at \
         FROM card_review_state WHERE card_id = ?1",
        [&id],
        |row| {
            Ok(json!({
                "card_id": row.get::<_, String>(0)?,
                "review_round": row.get::<_, i64>(1)?,
                "state": row.get::<_, String>(2)?,
                "pending_dispatch_id": row.get::<_, Option<String>>(3)?,
                "last_verdict": row.get::<_, Option<String>>(4)?,
                "last_decision": row.get::<_, Option<String>>(5)?,
                "decided_by": row.get::<_, Option<String>>(6)?,
                "decided_at": row.get::<_, Option<String>>(7)?,
                "review_entered_at": row.get::<_, Option<String>>(8)?,
                "updated_at": row.get::<_, Option<String>>(9)?,
            }))
        },
    ) {
        Ok(state) => (StatusCode::OK, Json(state)),
        Err(_) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no review state for this card"})),
        ),
    }
}

/// GET /api/kanban-cards/:id/reviews
pub async fn list_card_reviews(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(pool) = state.pg_pool.as_ref() {
        return match sqlx::query(
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
        };
    }

    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    let mut stmt = match conn.prepare(
        "SELECT id, kanban_card_id, dispatch_id, item_index, decision, decided_at
         FROM review_decisions
         WHERE kanban_card_id = ?1
         ORDER BY id",
    ) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("prepare: {e}")})),
            );
        }
    };

    let rows = stmt
        .query_map([&id], |row| {
            Ok(json!({
                "id": row.get::<_, i64>(0)?,
                "kanban_card_id": row.get::<_, Option<String>>(1)?,
                "dispatch_id": row.get::<_, Option<String>>(2)?,
                "item_index": row.get::<_, Option<i64>>(3)?,
                "decision": row.get::<_, Option<String>>(4)?,
                "decided_at": row.get::<_, Option<String>>(5)?,
            }))
        })
        .ok();

    let reviews: Vec<serde_json::Value> = match rows {
        Some(iter) => iter.filter_map(|r| r.ok()).collect(),
        None => Vec::new(),
    };

    (StatusCode::OK, Json(json!({"reviews": reviews})))
}

/// GET /api/kanban-cards/stalled
pub async fn stalled_cards(State(state): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    // Only include registered repos
    let registered_repos: Vec<String> = {
        match conn.prepare("SELECT id FROM github_repos") {
            Ok(mut s) => s
                .query_map([], |row| row.get::<_, String>(0))
                .ok()
                .map(|iter| iter.filter_map(|r| r.ok()).collect())
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    };
    let repo_filter = if registered_repos.is_empty() {
        String::new()
    } else {
        let quoted: Vec<String> = registered_repos
            .iter()
            .map(|r| format!("'{}'", r.replace('\'', "''")))
            .collect();
        format!(" AND kc.repo_id IN ({})", quoted.join(","))
    };

    let mut stmt = match conn.prepare(&format!(
        "{CARD_SELECT}
         WHERE kc.status = 'in_progress' AND {STALLED_ACTIVITY_AT_SQL} < datetime('now', '-2 hours'){}
         ORDER BY {STALLED_ACTIVITY_AT_SQL} ASC",
        repo_filter
    )) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("prepare: {e}")})),
            );
        }
    };

    let rows = stmt.query_map([], |row| card_row_to_json(row)).ok();

    let cards: Vec<serde_json::Value> = match rows {
        Some(iter) => iter.filter_map(|r| r.ok()).collect(),
        None => Vec::new(),
    };

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
        let transition_result = if target_status == "backlog" {
            transition_card_to_backlog_with_cleanup(&state, card_id, "bulk-action:backlog").await
        } else {
            crate::kanban::transition_status_with_opts(
                &state.db,
                &state.engine,
                card_id,
                &target_status,
                "bulk-action",
                crate::engine::transition::ForceIntent::OperatorOverride,
            )
        };

        match transition_result {
            Ok(_) => {
                // Emit updated card for each successful transition
                if let Ok(conn) = state.db.lock() {
                    if let Ok(card) = conn.query_row(
                        &format!("{CARD_SELECT} WHERE kc.id = ?1"),
                        [card_id],
                        |row| card_row_to_json(row),
                    ) {
                        crate::server::ws::emit_event(
                            &state.broadcast_tx,
                            "kanban_card_updated",
                            card,
                        );
                    }
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
    if let Some(pool) = state.pg_pool.as_ref() {
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
                    if let Err(error) = crate::kanban::transition_status_with_opts_pg(
                        Some(&state.db),
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
            } else if let Err(error) = crate::kanban::transition_status_with_opts_pg(
                Some(&state.db),
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

    let id = uuid::Uuid::new_v4().to_string();

    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    // Check for existing card with same github_issue_number + repo_id
    if let Ok(existing_id) = conn.query_row(
        "SELECT id FROM kanban_cards WHERE github_issue_number = ?1 AND repo_id = ?2",
        libsql_rusqlite::params![body.github_issue_number, body.github_repo],
        |row| row.get::<_, String>(0),
    ) {
        // Update existing card instead of creating duplicate
        // COALESCE: preserve existing description when incoming value is NULL
        let _ = conn.execute(
            "UPDATE kanban_cards SET title = ?1, assigned_agent_id = ?2, github_issue_url = ?3, description = COALESCE(?4, description), updated_at = datetime('now') WHERE id = ?5",
            libsql_rusqlite::params![body.title, body.assignee_agent_id, body.github_issue_url, body.description, existing_id],
        );
        drop(conn);

        // Transition to dispatchable state if not already — fires OnCardTransition hook
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
        let _ =
            crate::kanban::transition_status(&state.db, &state.engine, &existing_id, &ready_state);

        let conn = match state.db.lock() {
            Ok(c) => c,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
        };
        return match conn.query_row(
            &format!("{CARD_SELECT} WHERE kc.id = ?1"),
            [&existing_id],
            |row| card_row_to_json(row),
        ) {
            Ok(card) => {
                crate::server::ws::emit_event(
                    &state.broadcast_tx,
                    "kanban_card_updated",
                    card.clone(),
                );
                (
                    StatusCode::OK,
                    Json(json!({"card": card, "deduplicated": true})),
                )
            }
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            ),
        };
    }

    // Pipeline-driven: new cards with assignee start in dispatchable state
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
    let result = conn.execute(
        "INSERT INTO kanban_cards (id, repo_id, title, status, priority, assigned_agent_id, github_issue_url, github_issue_number, description, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, 'medium', ?5, ?6, ?7, ?8, datetime('now'), datetime('now'))",
        libsql_rusqlite::params![
            id,
            body.github_repo,
            body.title,
            ready_state,
            body.assignee_agent_id,
            body.github_issue_url,
            body.github_issue_number,
            body.description,
        ],
    );

    if let Err(e) = result {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        );
    }

    match conn.query_row(&format!("{CARD_SELECT} WHERE kc.id = ?1"), [&id], |row| {
        card_row_to_json(row)
    }) {
        Ok(card) => {
            crate::server::ws::emit_event(&state.broadcast_tx, "kanban_card_created", card.clone());
            (StatusCode::CREATED, Json(json!({"card": card})))
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        ),
    }
}

// ── Helpers ────────────────────────────────────────────────────

pub(super) fn card_row_to_json(
    row: &libsql_rusqlite::Row,
) -> libsql_rusqlite::Result<serde_json::Value> {
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
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    let mut stmt = match conn.prepare(
        "SELECT id, card_id, from_status, to_status, source, result, created_at \
         FROM kanban_audit_logs WHERE card_id = ?1 ORDER BY created_at DESC LIMIT 50",
    ) {
        Ok(s) => s,
        Err(_) => {
            // Table may not exist yet
            return (StatusCode::OK, Json(json!({"logs": []})));
        }
    };

    let logs: Vec<serde_json::Value> = stmt
        .query_map([&id], |row| {
            Ok(json!({
                "id": row.get::<_, i64>(0)?,
                "card_id": row.get::<_, String>(1)?,
                "from_status": row.get::<_, Option<String>>(2)?,
                "to_status": row.get::<_, Option<String>>(3)?,
                "source": row.get::<_, Option<String>>(4)?,
                "result": row.get::<_, Option<String>>(5)?,
                "created_at": row.get::<_, Option<String>>(6)?,
            }))
        })
        .ok()
        .map(|iter| iter.filter_map(|r| r.ok()).collect())
        .unwrap_or_default();

    (StatusCode::OK, Json(json!({"logs": logs})))
}

/// GET /api/kanban-cards/:id/comments
/// Fetch GitHub comments for the linked issue via `gh` CLI.
pub async fn card_github_comments(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let (repo_id, issue_number) = {
        let conn = match state.db.lock() {
            Ok(c) => c,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
        };
        match conn.query_row(
            "SELECT repo_id, github_issue_number FROM kanban_cards WHERE id = ?1",
            [&id],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                ))
            },
        ) {
            Ok(r) => r,
            Err(_) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "card not found"})),
                );
            }
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

    // Fetch comments AND body via the GitHub adapter in a blocking task
    let card_id = id.clone();
    let db = state.db.clone();
    let result =
        tokio::task::spawn_blocking(move || crate::github::fetch_issue_comments(&repo, number))
            .await;

    match result {
        Ok(Ok(issue)) => {
            let comments = serde_json::to_value(issue.comments).unwrap_or_else(|_| json!([]));
            let body = issue.body.unwrap_or_default();

            // On-demand sync: update card description from latest issue body
            // Only UPDATE when the value actually changed to avoid polluting updated_at
            if let Ok(conn) = db.lock() {
                let _ = conn.execute(
                    "UPDATE kanban_cards SET description = ?1, updated_at = datetime('now') \
                     WHERE id = ?2 AND (description IS NOT ?1 OR description IS NULL)",
                    libsql_rusqlite::params![body, card_id],
                );
            }

            (
                StatusCode::OK,
                Json(json!({"comments": comments, "body": body})),
            )
        }
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))),
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
    let valid = ["resume", "rework", "dismiss", "requeue"];
    if !valid.contains(&body.decision.as_str()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("decision must be one of: {}", valid.join(", "))})),
        );
    }

    // Verify card exists and currently requires manual intervention.
    let card_info: Option<(String, Option<String>, Option<String>, String, String)> = {
        let conn = match state.db.lock() {
            Ok(c) => c,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
        };
        conn.query_row(
            "SELECT status, review_status, blocked_reason, COALESCE(assigned_agent_id, ''), title FROM kanban_cards WHERE id = ?1",
            [&body.card_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )
        .ok()
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
        let pending_dispatch_ids: Vec<String> = state
            .db
            .lock()
            .ok()
            .and_then(|conn| {
                let mut stmt = conn
                    .prepare(
                        "SELECT id FROM task_dispatches
                         WHERE kanban_card_id = ?1 AND dispatch_type = 'pm-decision' AND status = 'pending'",
                    )
                    .ok()?;
                Some(
                    stmt.query_map([&body.card_id], |row| row.get(0))
                        .ok()?
                        .filter_map(|row| row.ok())
                        .collect(),
                )
            })
            .unwrap_or_default();
        for dispatch_id in pending_dispatch_ids {
            crate::dispatch::mark_dispatch_completed_pg_first(
                &state.db,
                state.pg_pool.as_ref(),
                &dispatch_id,
                &completion_result,
            )
            .ok();
        }
    }
    // Clear manual-intervention markers before applying the selected decision.
    if let Ok(conn) = state.db.lock() {
        conn.execute(
            "UPDATE kanban_cards SET blocked_reason = NULL WHERE id = ?1",
            [&body.card_id],
        )
        .ok();
        if legacy_manual_state || review_status.as_deref() == Some("dilemma_pending") {
            crate::engine::transition::execute_intent_on_conn(
                &conn,
                &crate::engine::transition::TransitionIntent::SetReviewStatus {
                    card_id: body.card_id.clone(),
                    review_status: None,
                },
            )
            .ok();
            crate::engine::transition::execute_intent_on_conn(
                &conn,
                &crate::engine::transition::TransitionIntent::SyncReviewState {
                    card_id: body.card_id.clone(),
                    state: "idle".to_string(),
                },
            )
            .ok();
        }
    }

    let message = match body.decision.as_str() {
        "resume" => {
            // Guard: resume requires a live dispatch + working session.
            // Without one the card would be stranded in in_progress with nothing driving it.
            let has_live = {
                if let Ok(conn) = state.db.lock() {
                    let count: i64 = conn
                        .query_row(
                            "SELECT COUNT(*) FROM task_dispatches td \
                             JOIN sessions s ON s.active_dispatch_id = td.id AND s.status IN ('working', 'idle') \
                             WHERE td.kanban_card_id = ?1 AND td.status IN ('pending', 'dispatched')",
                            [&body.card_id],
                            |r| r.get(0),
                        )
                        .unwrap_or(0);
                    count > 0
                } else {
                    false
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
            if let Err(e) = crate::kanban::transition_status_with_opts(
                &state.db,
                &state.engine,
                &body.card_id,
                &resume_target,
                "pm-decision",
                crate::engine::transition::ForceIntent::OperatorOverride,
            ) {
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
            match crate::dispatch::create_dispatch(
                &state.db,
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
                    let pending_dispatch_ids: Vec<String> = state
                        .db
                        .lock()
                        .ok()
                        .and_then(|conn| {
                            let mut stmt = conn
                                .prepare(
                                    "SELECT id FROM task_dispatches
                                     WHERE kanban_card_id = ?1 AND dispatch_type = 'pm-decision' AND status = 'pending'",
                                )
                                .ok()?;
                            Some(
                                stmt.query_map([&body.card_id], |row| row.get(0))
                                    .ok()?
                                    .filter_map(|row| row.ok())
                                    .collect(),
                            )
                        })
                        .unwrap_or_default();
                    for dispatch_id in pending_dispatch_ids {
                        crate::dispatch::mark_dispatch_completed_pg_first(
                            &state.db,
                            state.pg_pool.as_ref(),
                            &dispatch_id,
                            &completion_result,
                        )
                        .ok();
                    }
                    // Pipeline-driven: rework target from current state's review_rework gate
                    let rework_status: String = state
                        .db
                        .lock()
                        .ok()
                        .and_then(|c| {
                            c.query_row(
                                "SELECT status FROM kanban_cards WHERE id = ?1",
                                [&body.card_id],
                                |r| r.get(0),
                            )
                            .ok()
                        })
                        .unwrap_or_default();
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
                    if let Err(e) = crate::kanban::transition_status_with_opts(
                        &state.db,
                        &state.engine,
                        &body.card_id,
                        &rework_target,
                        "pm-decision",
                        crate::engine::transition::ForceIntent::OperatorOverride,
                    ) {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({"error": format!("rework transition failed: {e}")})),
                        );
                    }
                    if let Ok(conn) = state.db.lock() {
                        // #155: Use intent for review_status mutation
                        crate::engine::transition::execute_intent_on_conn(
                            &conn,
                            &crate::engine::transition::TransitionIntent::SetReviewStatus {
                                card_id: body.card_id.clone(),
                                review_status: Some("rework_pending".to_string()),
                            },
                        )
                        .ok();
                        // #117/#158: sync canonical review state via unified entrypoint
                        crate::engine::ops::review_state_sync_on_conn(
                            &conn,
                            &serde_json::json!({"card_id": body.card_id, "state": "rework_pending", "last_decision": "pm_rework"}).to_string(),
                        );
                    }
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
            if let Err(e) = crate::kanban::transition_status_with_opts(
                &state.db,
                &state.engine,
                &body.card_id,
                terminal,
                "pm-decision",
                crate::engine::transition::ForceIntent::OperatorOverride,
            ) {
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
            if let Err(e) = crate::kanban::transition_status_with_opts(
                &state.db,
                &state.engine,
                &body.card_id,
                requeue_target,
                "pm-decision",
                crate::engine::transition::ForceIntent::OperatorOverride,
            ) {
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
    if let Ok(conn) = state.db.lock() {
        if let Ok(card) = conn.query_row(
            &format!("{CARD_SELECT} WHERE kc.id = ?1"),
            [&body.card_id],
            |row| card_row_to_json(row),
        ) {
            crate::server::ws::emit_event(&state.broadcast_tx, "kanban_card_updated", card);
        }
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

fn find_active_review_dispatch_id(
    conn: &libsql_rusqlite::Connection,
    card_id: &str,
) -> Option<String> {
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

fn trimmed_header_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

pub(super) fn require_explicit_bearer_token(
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

fn resolve_agent_id_from_channel_id_on_conn(
    conn: &libsql_rusqlite::Connection,
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

pub(super) fn resolve_requesting_agent_id_on_conn(
    conn: &libsql_rusqlite::Connection,
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

pub(super) async fn resolve_requesting_agent_id_with_pg(
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

    let reason = body.reason.as_deref().unwrap_or("manual rereview");
    let (current_status, assigned_agent_id, card_title, gh_url, caller_source) = 'lookup: {
        if let Some(pool) = state.pg_pool.as_ref() {
            match sqlx::query_as::<_, (String, Option<String>, String, Option<String>)>(
                "SELECT status, assigned_agent_id, title, github_issue_url
                 FROM kanban_cards WHERE id = $1",
            )
            .bind(&id)
            .fetch_optional(pool)
            .await
            {
                Ok(Some((status, agent, title, url))) => {
                    let caller = if let Ok(conn) = state.db.lock() {
                        resolve_requesting_agent_id_on_conn(&conn, &headers)
                            .unwrap_or_else(|| "api".to_string())
                    } else {
                        "api".to_string()
                    };
                    break 'lookup (status, agent, title, url, caller);
                }
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
                        "[rereview] postgres lookup failed, falling back to sqlite"
                    );
                }
            }
        }

        let conn = match state.db.lock() {
            Ok(c) => c,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
        };
        match conn.query_row(
            "SELECT status, assigned_agent_id, title, github_issue_url
             FROM kanban_cards WHERE id = ?1",
            [&id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    resolve_requesting_agent_id_on_conn(&conn, &headers)
                        .unwrap_or_else(|| "api".to_string()),
                ))
            },
        ) {
            Ok(values) => values,
            Err(_) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": format!("card not found: {id}")})),
                );
            }
        }
    };

    let assigned_agent_id = match assigned_agent_id.filter(|value| !value.is_empty()) {
        Some(value) => value,
        None => {
            return (
                StatusCode::CONFLICT,
                Json(json!({"error": "card has no assigned agent"})),
            );
        }
    };

    {
        let conn = match state.db.lock() {
            Ok(c) => c,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
        };

        let stale_ids: Vec<String> = conn
            .prepare(
                "SELECT id FROM task_dispatches
                 WHERE kanban_card_id = ?1
                   AND dispatch_type IN ('review', 'review-decision')
                   AND status IN ('pending', 'dispatched')",
            )
            .and_then(|mut stmt| {
                stmt.query_map([&id], |row| row.get::<_, String>(0))
                    .map(|rows| rows.filter_map(|row| row.ok()).collect())
            })
            .unwrap_or_default();

        for stale_id in &stale_ids {
            if let Err(e) = crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_conn(
                &conn,
                stale_id,
                Some("superseded_by_rereview"),
            ) {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
        }

        // ── Stale cleanup: reset review-related fields so OnReviewEnter starts clean ──
        conn.execute(
            "UPDATE kanban_cards
             SET review_status = NULL,
                 suggestion_pending_at = NULL,
                 review_entered_at = NULL,
                 awaiting_dod_at = NULL,
                 updated_at = datetime('now')
             WHERE id = ?1",
            [&id],
        )
        .ok();

        let sync_payload = json!({
            "card_id": id,
            "state": "idle",
        })
        .to_string();
        let sync_result = crate::engine::ops::review_state_sync_on_conn(&conn, &sync_payload);
        if sync_result.contains("\"error\"") {
            tracing::warn!("[kanban] rereview review_state_sync cleanup failed: {sync_result}");
        }

        // #272/#420: Explicitly clear repeated-finding escalation markers so a
        // new re-review cycle starts with clean state. The generic sync uses COALESCE (preserves old
        // value when NULL is passed), so we do a targeted UPDATE here instead of
        // widening the idle-sync semantics which would affect timeout / gate-failure
        // paths that also sync to "idle".
        if let Err(e) = conn.execute(
            "UPDATE card_review_state
             SET approach_change_round = NULL,
                 session_reset_round = NULL
             WHERE card_id = ?1",
            [&id],
        ) {
            tracing::warn!("[kanban] rereview repeated-finding reset failed: {e}");
        }
    }

    let transitioned_into_review = current_status != "review";

    if transitioned_into_review {
        if let Err(e) = crate::kanban::transition_status_with_opts(
            &state.db,
            &state.engine,
            &id,
            "review",
            &format!("{caller_source}:rereview({reason})"),
            crate::engine::transition::ForceIntent::OperatorOverride,
        ) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    } else {
        crate::kanban::fire_enter_hooks(&state.db, &state.engine, &id, "review");
    }

    let mut review_dispatch_id = state
        .db
        .lock()
        .ok()
        .and_then(|conn| find_active_review_dispatch_id(&conn, &id));

    if review_dispatch_id.is_none() && !transitioned_into_review {
        let _ = state
            .engine
            .fire_hook_by_name_blocking("OnReviewEnter", json!({ "card_id": id }));
        crate::kanban::drain_hook_side_effects(&state.db, &state.engine);
        review_dispatch_id = state
            .db
            .lock()
            .ok()
            .and_then(|conn| find_active_review_dispatch_id(&conn, &id));
    }

    if review_dispatch_id.is_none() {
        match crate::dispatch::create_dispatch(
            &state.db,
            &state.engine,
            &id,
            &assigned_agent_id,
            "review",
            &card_title,
            &json!({ "rereview": true, "reason": reason }),
        ) {
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

    crate::kanban::correct_tn_to_fn_on_reopen(&state.db, state.pg_pool.as_ref(), &id);

    // Reset completed_at + advance any active auto-queue entries linked to
    // this card. SQLite work is best-effort because PG-only cards have no
    // SQLite row; the canonical card view comes from PG below.
    if let Ok(conn) = state.db.lock() {
        let _ = conn.execute(
            "UPDATE kanban_cards
             SET completed_at = NULL, updated_at = datetime('now')
             WHERE id = ?1",
            [&id],
        );

        let entry_ids: Vec<String> = conn
            .prepare(
                "SELECT id FROM auto_queue_entries
                 WHERE kanban_card_id = ?1
                   AND status IN ('pending', 'dispatched', 'done')
                   AND run_id IN (
                       SELECT id FROM auto_queue_runs WHERE status IN ('active', 'paused')
                   )",
            )
            .ok()
            .and_then(|mut stmt| {
                stmt.query_map([&id], |row| row.get::<_, String>(0))
                    .ok()
                    .map(|rows| rows.filter_map(|row| row.ok()).collect())
            })
            .unwrap_or_default();
        for entry_id in entry_ids {
            if let Err(error) = move_auto_queue_entry_to_dispatched_on_conn(
                &conn,
                &entry_id,
                "rereview_dispatch",
                &crate::db::auto_queue::EntryStatusUpdateOptions {
                    dispatch_id: Some(review_dispatch_id.clone()),
                    slot_index: None,
                },
            ) {
                tracing::warn!(
                    card_id = %id,
                    %error,
                    "[rereview] sqlite auto-queue entry update failed (entry may live only in PG)"
                );
            }
        }
    }

    let card = if let Some(pool) = state.pg_pool.as_ref() {
        match load_card_json_pg(pool, &id).await {
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
        }
    } else {
        let conn = match state.db.lock() {
            Ok(c) => c,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
        };
        match conn.query_row(&format!("{CARD_SELECT} WHERE kc.id = ?1"), [&id], |row| {
            card_row_to_json(row)
        }) {
            Ok(card) => card,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
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

/// POST /api/re-review
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
    let mut results = Vec::new();

    for issue_number in &body.issues {
        let mut card_id: Option<String> = None;
        if let Some(pool) = state.pg_pool.as_ref() {
            match sqlx::query_scalar::<_, String>(
                "SELECT id FROM kanban_cards WHERE github_issue_number = $1",
            )
            .bind(*issue_number)
            .fetch_optional(pool)
            .await
            {
                Ok(value) => card_id = value,
                Err(error) => {
                    tracing::warn!(
                        %issue_number,
                        %error,
                        "[batch_rereview] postgres lookup failed, falling back to sqlite"
                    );
                }
            }
        }
        if card_id.is_none() {
            card_id = state.db.lock().ok().and_then(|conn| {
                conn.query_row(
                    "SELECT id FROM kanban_cards WHERE github_issue_number = ?1",
                    [issue_number],
                    |row| row.get(0),
                )
                .ok()
            });
        }

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

    let caller_source = {
        let conn = match state.db.lock() {
            Ok(c) => c,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
        };
        resolve_requesting_agent_id_on_conn(&conn, &headers).unwrap_or_else(|| "api".to_string())
    };

    // ── Pre-check: card must be in done state ──
    let current_status: String = if let Some(pool) = state.pg_pool.as_ref() {
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
        }
    } else {
        let conn = match state.db.lock() {
            Ok(c) => c,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
        };
        match conn.query_row(
            "SELECT status FROM kanban_cards WHERE id = ?1",
            [&id],
            |row| row.get(0),
        ) {
            Ok(status) => status,
            Err(_) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": format!("card not found: {id}")})),
                );
            }
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

    if let Some(pool) = state.pg_pool.as_ref() {
        if let Err(error) = mark_api_reopen_skip_preflight_on_pg(pool, &id).await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    json!({"error": format!("failed to stage API reopen preflight skip: {error}")}),
                ),
            );
        }

        let transition_result = if reset_full {
            crate::kanban::transition_status_with_opts_and_allowed_cleanup_pg(
                Some(&state.db),
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
            crate::kanban::transition_status_with_opts_pg(
                Some(&state.db),
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
                crate::kanban::correct_tn_to_fn_on_reopen(&state.db, state.pg_pool.as_ref(), &id);

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

    {
        let conn = match state.db.lock() {
            Ok(c) => c,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
        };
        if let Err(e) = mark_api_reopen_skip_preflight_on_conn(&conn, &id) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("failed to stage API reopen preflight skip: {e}")})),
            );
        }
    }

    // ── Transition terminal → work state (force=true bypasses terminal guard) ──
    match {
        let result = crate::kanban::transition_status_with_opts(
            &state.db,
            &state.engine,
            &id,
            &reopen_target,
            &format!("{caller_source}:reopen({reason})"),
            crate::engine::transition::ForceIntent::OperatorOverride,
        );
        result.map(|result| (result.from, result.to))
    } {
        Ok((from_status, to_status)) => {
            crate::kanban::correct_tn_to_fn_on_reopen(&state.db, state.pg_pool.as_ref(), &id);

            let (gh_url, card, cleanup_counts): (
                Option<String>,
                Result<serde_json::Value, String>,
                (usize, usize),
            ) = {
                // ── Post-transition cleanup: clear completed_at and optional recovery fields ──
                let conn = match state.db.lock() {
                    Ok(c) => c,
                    Err(e) => {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({"error": format!("{e}")})),
                        );
                    }
                };

                let cleanup_counts = if reset_full {
                    match cleanup_force_transition_revert_on_conn(&conn, &id, &reopen_target) {
                        Ok(counts) => {
                            crate::server::routes::dispatches::clear_all_threads(&conn, &id);
                            if let Err(e) = clear_reopen_preflight_cache_on_conn(&conn, &id) {
                                return (
                                    StatusCode::INTERNAL_SERVER_ERROR,
                                    Json(
                                        json!({"error": format!("failed to clear reopen cache: {e}")}),
                                    ),
                                );
                            }
                            counts
                        }
                        Err(e) => {
                            return (
                                StatusCode::INTERNAL_SERVER_ERROR,
                                Json(json!({"error": format!("{e}")})),
                            );
                        }
                    }
                } else {
                    if let Err(e) = consume_api_reopen_preflight_skip_on_conn(&conn, &id) {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(
                                json!({"error": format!("failed to persist API reopen preflight skip: {e}")}),
                            ),
                        );
                    }
                    (0, 0)
                };

                // Always clear completed_at on reopen
                conn.execute(
                    "UPDATE kanban_cards SET completed_at = NULL, updated_at = datetime('now') WHERE id = ?1",
                    [&id],
                )
                .ok();

                // #155: Optional review_status via intent
                if let Some(ref rs) = body.review_status {
                    crate::engine::transition::execute_intent_on_conn(
                        &conn,
                        &crate::engine::transition::TransitionIntent::SetReviewStatus {
                            card_id: id.clone(),
                            review_status: Some(rs.clone()),
                        },
                    )
                    .ok();
                }

                // Reopen reactivates completed auto-queue entries so the card can be
                // redispatched after stale live reservations are cleaned up.
                let entry_ids: Vec<String> = conn
                    .prepare(
                        "SELECT id FROM auto_queue_entries
                         WHERE kanban_card_id = ?1 AND status = 'done'",
                    )
                    .ok()
                    .and_then(|mut stmt| {
                        stmt.query_map([&id], |row| row.get::<_, String>(0))
                            .ok()
                            .map(|rows| rows.filter_map(|row| row.ok()).collect())
                    })
                    .unwrap_or_default();
                for entry_id in entry_ids {
                    if let Err(error) = move_auto_queue_entry_to_dispatched_on_conn(
                        &conn,
                        &entry_id,
                        "api_reopen",
                        &crate::db::auto_queue::EntryStatusUpdateOptions::default(),
                    ) {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({"error": format!("{error}")})),
                        );
                    }
                }

                // Re-open GitHub issue if linked
                let gh_url: Option<String> = conn
                    .query_row(
                        "SELECT github_issue_url FROM kanban_cards WHERE id = ?1",
                        [&id],
                        |row| row.get(0),
                    )
                    .ok()
                    .flatten();

                let card = conn
                    .query_row(&format!("{CARD_SELECT} WHERE kc.id = ?1"), [&id], |row| {
                        card_row_to_json(row)
                    })
                    .map_err(|e| format!("{e}"));
                (gh_url, card, cleanup_counts)
            };

            if let Some(url) = gh_url.as_deref() {
                if let Err(e) = crate::github::reopen_issue_by_url(url).await {
                    tracing::warn!("[kanban] Failed to reopen GitHub issue {url}: {e}");
                    return (
                        StatusCode::BAD_GATEWAY,
                        Json(json!({
                            "error": format!("github issue reopen failed before reopen response: {e}"),
                            "reopened": false,
                            "github_issue_url": url,
                        })),
                    );
                }
            }

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
                Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))),
            }
        }
        Err(e) => {
            if let Ok(conn) = state.db.lock() {
                let _ = clear_api_reopen_skip_preflight_on_conn(&conn, &id);
            }
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            )
        }
    }
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

    let caller_source = {
        let conn = match state.db.lock() {
            Ok(c) => c,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
        };
        resolve_requesting_agent_id_on_conn(&conn, &headers).unwrap_or_else(|| "api".to_string())
    };
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
            let card_ids: Vec<String> = if let Some(pool) = state.pg_pool.as_ref() {
                match sqlx::query_scalar::<_, String>(
                    "SELECT id
                     FROM kanban_cards
                     WHERE github_issue_number = $1
                     ORDER BY id ASC",
                )
                .bind(issue_number)
                .fetch_all(pool)
                .await
                {
                    Ok(ids) => ids,
                    Err(error) => {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({"error": format!("{error}")})),
                        );
                    }
                }
            } else {
                let conn = match state.db.lock() {
                    Ok(c) => c,
                    Err(e) => {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({"error": format!("{e}")})),
                        );
                    }
                };
                let mut stmt = match conn.prepare(
                    "SELECT id FROM kanban_cards WHERE github_issue_number = ?1 ORDER BY id ASC",
                ) {
                    Ok(stmt) => stmt,
                    Err(e) => {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({"error": format!("{e}")})),
                        );
                    }
                };
                stmt.query_map([issue_number], |row| row.get(0))
                    .ok()
                    .map(|rows| rows.filter_map(|row| row.ok()).collect())
                    .unwrap_or_default()
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
        let transition_result = if let Some(pool) = state.pg_pool.as_ref() {
            if force_transition_needs_cleanup(&body.status, body.cancel_dispatches) {
                crate::kanban::transition_status_with_opts_and_allowed_cleanup_pg(
                    Some(&state.db),
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
            } else {
                crate::kanban::transition_status_with_opts_pg(
                    Some(&state.db),
                    pool,
                    &state.engine,
                    &card_id,
                    &body.status,
                    &batch_transition_source,
                    crate::engine::transition::ForceIntent::OperatorOverride,
                )
                .await
                .map(|result| (result, (0, 0)))
            }
        } else {
            crate::kanban::transition_status_with_opts(
                &state.db,
                &state.engine,
                &card_id,
                &body.status,
                &batch_transition_source,
                crate::engine::transition::ForceIntent::OperatorOverride,
            )
            .map(|result| {
                let counts = if force_transition_needs_cleanup(&body.status, body.cancel_dispatches)
                {
                    let conn = state
                        .db
                        .lock()
                        .map_err(|error| anyhow::anyhow!("{error}"))?;
                    cleanup_force_transition_revert_on_conn(&conn, &card_id, &body.status)
                } else {
                    Ok((0, 0))
                }?;
                Ok::<_, anyhow::Error>((result, counts))
            })
            .and_then(|result| result)
        };

        match transition_result {
            Ok(result) => {
                results.push(json!({
                    "card_id": card_id,
                    "issue_number": issue_number,
                    "ok": true,
                    "from": result.0.from,
                    "to": result.0.to,
                    "cancelled_dispatches": result.1.0,
                    "skipped_auto_queue_entries": result.1.1,
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

fn cleanup_force_transition_terminal_on_conn(
    conn: &libsql_rusqlite::Connection,
    card_id: &str,
    target_status: &str,
) -> anyhow::Result<usize> {
    let reason = format!("force-transition to {target_status}");
    let cancelled_dispatches =
        crate::dispatch::cancel_active_dispatches_for_card_on_conn(conn, card_id, Some(&reason))?;

    if cancelled_dispatches > 0 {
        crate::engine::transition::execute_intent_on_conn(
            conn,
            &crate::engine::transition::TransitionIntent::SetLatestDispatchId {
                card_id: card_id.to_string(),
                dispatch_id: None,
            },
        )?;
    }

    Ok(cancelled_dispatches)
}

fn count_live_auto_queue_entries_for_card_on_conn(
    conn: &libsql_rusqlite::Connection,
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

fn clear_force_transition_terminalized_links_on_conn(
    conn: &libsql_rusqlite::Connection,
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

fn cleanup_force_transition_revert_on_conn(
    conn: &libsql_rusqlite::Connection,
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
    crate::engine::ops::skip_live_auto_queue_entries_for_card_on_conn(conn, card_id)?;
    clear_force_transition_terminalized_links_on_conn(conn, card_id)?;
    crate::kanban::cleanup_force_transition_revert_fields_on_conn(conn, card_id)?;

    Ok((cancelled_dispatches, skipped_auto_queue_entries))
}

fn move_auto_queue_entry_to_dispatched_on_conn(
    conn: &libsql_rusqlite::Connection,
    entry_id: &str,
    trigger_source: &str,
    options: &crate::db::auto_queue::EntryStatusUpdateOptions,
) -> Result<(), crate::db::auto_queue::EntryStatusUpdateError> {
    match crate::db::auto_queue::update_entry_status_on_conn(
        conn,
        entry_id,
        crate::db::auto_queue::ENTRY_STATUS_DISPATCHED,
        trigger_source,
        options,
    ) {
        Err(crate::db::auto_queue::EntryStatusUpdateError::InvalidTransition {
            from_status,
            to_status,
            ..
        }) if from_status == crate::db::auto_queue::ENTRY_STATUS_DONE
            && to_status == crate::db::auto_queue::ENTRY_STATUS_DISPATCHED =>
        {
            crate::db::auto_queue::reactivate_done_entry_on_conn(
                conn,
                entry_id,
                trigger_source,
                options,
            )
            .map(|_| ())
        }
        other => other.map(|_| ()),
    }
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

fn load_card_metadata_map_on_conn(
    conn: &libsql_rusqlite::Connection,
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

fn save_card_metadata_map_on_conn(
    conn: &libsql_rusqlite::Connection,
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
            libsql_rusqlite::params![serde_json::to_string(metadata)?, card_id],
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

fn mark_api_reopen_skip_preflight_on_conn(
    conn: &libsql_rusqlite::Connection,
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

fn clear_api_reopen_skip_preflight_on_conn(
    conn: &libsql_rusqlite::Connection,
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

fn consume_api_reopen_preflight_skip_on_conn(
    conn: &libsql_rusqlite::Connection,
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

fn clear_reopen_preflight_cache_on_conn(
    conn: &libsql_rusqlite::Connection,
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
    let caller_source = {
        let conn = match state.db.lock() {
            Ok(c) => c,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
        };
        resolve_requesting_agent_id_on_conn(&conn, &headers).unwrap_or_else(|| "api".to_string())
    };
    let terminal_cleanup = if let Some(pool) = state.pg_pool.as_ref() {
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
        }
    } else {
        let conn = match state.db.lock() {
            Ok(c) => c,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
        };
        crate::pipeline::ensure_loaded();
        let (card_repo_id, card_agent_id): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT repo_id, assigned_agent_id FROM kanban_cards WHERE id = ?1",
                [&id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap_or_default();
        let effective = crate::pipeline::resolve_for_card(
            &conn,
            card_repo_id.as_deref(),
            card_agent_id.as_deref(),
        );
        effective.is_terminal(&target_status) && body.cancel_dispatches.unwrap_or(true)
    };

    let transition_result = if let Some(pool) = state.pg_pool.as_ref() {
        if needs_cleanup {
            crate::kanban::transition_status_with_opts_and_allowed_cleanup_pg(
                Some(&state.db),
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
            crate::kanban::transition_status_with_opts_and_allowed_cleanup_pg(
                Some(&state.db),
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
            crate::kanban::transition_status_with_opts_pg(
                Some(&state.db),
                pool,
                &state.engine,
                &id,
                &target_status,
                &caller_source,
                crate::engine::transition::ForceIntent::OperatorOverride,
            )
            .await
        }
    } else {
        if needs_cleanup {
            crate::kanban::transition_status_with_opts_and_on_conn(
                &state.db,
                &state.engine,
                &id,
                &target_status,
                &caller_source,
                crate::engine::transition::ForceIntent::OperatorOverride,
                // allowed: forced rewind must scrub stale review/dispatch state atomically.
                crate::kanban::AllowedOnConnMutation::ForceTransitionRevertCleanup,
                |conn| {
                    cleanup_counts =
                        cleanup_force_transition_revert_on_conn(conn, &id, &target_status)?;
                    Ok(())
                },
            )
        } else if terminal_cleanup {
            crate::kanban::transition_status_with_opts_and_on_conn(
                &state.db,
                &state.engine,
                &id,
                &target_status,
                &caller_source,
                crate::engine::transition::ForceIntent::OperatorOverride,
                // allowed: terminal force path must cancel stale dispatches before commit.
                crate::kanban::AllowedOnConnMutation::ForceTransitionTerminalCleanup,
                |conn| {
                    cleanup_counts.0 =
                        cleanup_force_transition_terminal_on_conn(conn, &id, &target_status)?;
                    Ok(())
                },
            )
        } else {
            crate::kanban::transition_status_with_opts(
                &state.db,
                &state.engine,
                &id,
                &target_status,
                &caller_source,
                crate::engine::transition::ForceIntent::OperatorOverride,
            )
        }
    };

    match transition_result {
        Ok(result) => {
            let (cancelled_dispatches, skipped_auto_queue_entries) = cleanup_counts;
            crate::kanban::drain_hook_side_effects(&state.db, &state.engine);

            let card = if let Some(pool) = state.pg_pool.as_ref() {
                load_card_json_pg(pool, &id)
                    .await
                    .map_err(|error| format!("{error}"))
                    .and_then(|card| {
                        card.ok_or_else(|| "card not found after force-transition".to_string())
                    })
            } else {
                let conn = state.db.lock().unwrap();
                let card =
                    conn.query_row(&format!("{CARD_SELECT} WHERE kc.id = ?1"), [&id], |row| {
                        card_row_to_json(row)
                    });
                drop(conn);
                card.map_err(|error| format!("{error}"))
            };
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
