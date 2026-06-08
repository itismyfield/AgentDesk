//! Axum route handlers for dispatch thread linking and lookup.
//!
//! The Postgres/Discord-API thread-reuse helpers were relocated to
//! `crate::services::dispatches::discord_delivery` (#3037) so the service layer
//! no longer reaches back into the route layer. These handlers consume those
//! relocated helpers.

use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
};
use serde_json::json;
use sqlx::Row as SqlxRow;

use crate::db::agents::{resolve_agent_counter_model_channel_pg, resolve_agent_primary_channel_pg};
use crate::server::routes::AppState;
use crate::services::dispatches::LinkDispatchThreadBody;
use crate::services::dispatches::discord_delivery::{
    get_mapped_thread_for_channel_pg, get_thread_for_channel_pg,
    set_thread_for_channel_map_only_pg, set_thread_for_channel_pg,
};

use super::parse_channel_id;

// ── Route handlers ────────────────────────────────────────────
/// POST /api/internal/link-dispatch-thread
/// Links a dispatch's kanban card to a Discord thread (sets active_thread_id).
/// Called by dcserver router.rs when it creates a thread as fallback.
pub async fn link_dispatch_thread(
    State(state): State<AppState>,
    Json(body): Json<LinkDispatchThreadBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "postgres pool unavailable"})),
        );
    };

    let dispatch_row = match sqlx::query(
        "SELECT kanban_card_id, dispatch_type, to_agent_id
         FROM task_dispatches
         WHERE id = $1",
    )
    .bind(&body.dispatch_id)
    .fetch_optional(pool)
    .await
    {
        Ok(row) => row,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{error}")})),
            );
        }
    };

    match dispatch_row {
        Some(row) => {
            let cid: String = row.try_get("kanban_card_id").unwrap_or_default();
            let dispatch_type: Option<String> = row.try_get("dispatch_type").ok().flatten();
            let to_agent_id: String = row.try_get("to_agent_id").unwrap_or_default();

            if let Err(error) = sqlx::query(
                "UPDATE task_dispatches
                 SET thread_id = $1,
                     updated_at = NOW()
                 WHERE id = $2",
            )
            .bind(&body.thread_id)
            .bind(&body.dispatch_id)
            .execute(pool)
            .await
            {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{error}")})),
                );
            }

            let save_result = if let Some(ref ch_id) = body.channel_id {
                if let Ok(ch_num) = ch_id.parse::<u64>() {
                    let counter_channel =
                        resolve_agent_counter_model_channel_pg(pool, &to_agent_id)
                            .await
                            .ok()
                            .flatten()
                            .and_then(|value| parse_channel_id(&value));
                    let is_counter_model_thread =
                        super::use_counter_model_channel(dispatch_type.as_deref())
                            && counter_channel == Some(ch_num);
                    if is_counter_model_thread {
                        set_thread_for_channel_map_only_pg(pool, &cid, ch_num, &body.thread_id)
                            .await
                    } else {
                        set_thread_for_channel_pg(pool, &cid, ch_num, &body.thread_id).await
                    }
                } else {
                    sqlx::query(
                        "UPDATE kanban_cards
                         SET active_thread_id = $1,
                             updated_at = NOW()
                         WHERE id = $2",
                    )
                    .bind(&body.thread_id)
                    .bind(&cid)
                    .execute(pool)
                    .await
                    .map(|_| ())
                    .map_err(|error| format!("{error}"))
                }
            } else {
                sqlx::query(
                    "UPDATE kanban_cards
                     SET active_thread_id = $1,
                         updated_at = NOW()
                     WHERE id = $2",
                )
                .bind(&body.thread_id)
                .bind(&cid)
                .execute(pool)
                .await
                .map(|_| ())
                .map_err(|error| format!("{error}"))
            };

            match save_result {
                Ok(()) => (StatusCode::OK, Json(json!({"ok": true, "card_id": cid}))),
                Err(error) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": error})),
                ),
            }
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "dispatch not found"})),
        ),
    }
}

/// GET /api/internal/card-thread?dispatch_id=xxx
/// Returns the active_thread_id for a dispatch's card (if any).
pub async fn get_card_thread(
    State(state): State<AppState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let dispatch_id = match params.get("dispatch_id") {
        Some(id) => id,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "dispatch_id required"})),
            );
        }
    };

    let Some(pool) = state.pg_pool_ref() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "postgres pool unavailable"})),
        );
    };

    let result = match sqlx::query(
        "SELECT kc.id AS card_id,
                kc.title AS card_title,
                kc.github_issue_url AS github_issue_url,
                kc.github_issue_number::BIGINT AS github_issue_number,
                kc.description AS issue_body,
                kc.deferred_dod_json::text AS deferred_dod_json,
                kc.active_thread_id AS legacy_thread_id,
                td.dispatch_type AS dispatch_type,
                td.title AS dispatch_title,
                td.to_agent_id AS dispatch_agent_id,
                td.context AS dispatch_context
         FROM task_dispatches td
         JOIN kanban_cards kc ON kc.id = td.kanban_card_id
         WHERE td.id = $1",
    )
    .bind(dispatch_id)
    .fetch_optional(pool)
    .await
    {
        Ok(row) => row,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{error}")})),
            );
        }
    };

    match result {
        Some(row) => {
            let card_id: String = row.try_get("card_id").unwrap_or_default();
            let card_title: String = row.try_get("card_title").unwrap_or_default();
            let github_issue_url: Option<String> = row.try_get("github_issue_url").ok().flatten();
            let github_issue_number: Option<i64> =
                row.try_get("github_issue_number").ok().flatten();
            let issue_body: Option<String> = row.try_get("issue_body").ok().flatten();
            let deferred_dod_json: Option<String> = row.try_get("deferred_dod_json").ok().flatten();
            let dispatch_type: Option<String> = row.try_get("dispatch_type").ok().flatten();
            let dispatch_title: Option<String> = row.try_get("dispatch_title").ok().flatten();
            let to_agent_id: String = row.try_get("dispatch_agent_id").unwrap_or_default();
            let dispatch_context: Option<String> = row.try_get("dispatch_context").ok().flatten();

            let primary_channel = resolve_agent_primary_channel_pg(pool, &to_agent_id)
                .await
                .ok()
                .flatten();
            let counter_model_channel = resolve_agent_counter_model_channel_pg(pool, &to_agent_id)
                .await
                .ok()
                .flatten();
            // Determine target channel for this dispatch type
            let use_alt = super::use_counter_model_channel(dispatch_type.as_deref());
            let target_channel = if use_alt {
                counter_model_channel.as_deref()
            } else {
                primary_channel.as_deref()
            };
            // Look up channel-specific thread
            let thread_id = if let Some(ch_num) = target_channel.and_then(parse_channel_id) {
                let lookup_result = if use_alt {
                    get_mapped_thread_for_channel_pg(pool, &card_id, ch_num).await
                } else {
                    get_thread_for_channel_pg(pool, &card_id, ch_num).await
                };
                match lookup_result {
                    Ok(thread_id) => thread_id,
                    Err(error) => {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({"error": error})),
                        );
                    }
                }
            } else {
                None
            };
            let deferred_dod = deferred_dod_json
                .as_deref()
                .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok());

            (
                StatusCode::OK,
                Json(json!({
                    "card_id": card_id,
                    "card_title": card_title,
                    "github_issue_url": github_issue_url,
                    "github_issue_number": github_issue_number,
                    "issue_body": issue_body,
                    "deferred_dod": deferred_dod,
                    "active_thread_id": thread_id,
                    "dispatch_type": dispatch_type,
                    "dispatch_title": dispatch_title,
                    "discord_channel_id": primary_channel,
                    "discord_channel_alt": counter_model_channel,
                    "discord_channel_target": target_channel,
                    "dispatch_context": dispatch_context,
                })),
            )
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "dispatch not found"})),
        ),
    }
}

/// GET /api/internal/pending-dispatch-for-thread?thread_id=xxx
///
/// #222: Look up the latest pending/dispatched dispatch whose kanban card is
/// linked to the given thread channel. Used by turn_bridge as fallback when
/// parse_dispatch_id(user_text) fails in unified/reused threads, including
/// review and review-decision flows.
pub async fn get_pending_dispatch_for_thread(
    State(state): State<AppState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let thread_id = match params.get("thread_id") {
        Some(id) => id,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "thread_id required"})),
            );
        }
    };

    let Some(pool) = state.pg_pool_ref() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "postgres pool unavailable"})),
        );
    };

    let dispatch_id = match sqlx::query_scalar::<_, String>(
        "SELECT td.id
         FROM task_dispatches td
         JOIN kanban_cards kc ON kc.id = td.kanban_card_id
         WHERE td.status IN ('pending', 'dispatched')
           AND (
             td.thread_id = $1
             OR kc.active_thread_id = $1
             OR EXISTS(
               SELECT 1
               FROM jsonb_each_text(COALESCE(kc.channel_thread_map, '{}'::jsonb)) AS entry(key, value)
               WHERE entry.value = $1
             )
           )
         ORDER BY td.created_at DESC
         LIMIT 1",
    )
    .bind(thread_id)
    .fetch_optional(pool)
    .await
    {
        Ok(dispatch_id) => dispatch_id,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{error}")})),
            );
        }
    };

    match dispatch_id {
        Some(id) => (StatusCode::OK, Json(json!({"dispatch_id": id}))),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no pending dispatch for thread"})),
        ),
    }
}
