use serde_json::json;
use sqlx::{PgPool, QueryBuilder, Row as SqlxRow};

use super::{CARD_SELECT_SQL_PG, KanbanCardRecord, ListCardsFilter, kanban_card_row_to_record_pg};
use crate::utils::github_links::{
    normalize_optional_github_issue_url, normalize_optional_github_repo_id,
};

pub async fn list_registered_repo_ids_pg(pool: &PgPool) -> Result<Vec<String>, String> {
    sqlx::query_scalar::<_, String>("SELECT id FROM github_repos")
        .fetch_all(pool)
        .await
        .map_err(|error| format!("list registered repos from postgres: {error}"))
}

pub async fn list_cards_pg(
    pool: &PgPool,
    filter: &ListCardsFilter,
    registered_repo_ids: &[String],
) -> Result<Vec<KanbanCardRecord>, String> {
    let mut query = QueryBuilder::new(format!("{CARD_SELECT_SQL_PG} WHERE 1=1"));

    if let Some(status) = filter.status.as_deref() {
        query.push(" AND kc.status = ");
        query.push_bind(status.to_string());
    }

    if let Some(repo_id) = filter.repo_id.as_deref() {
        query.push(" AND kc.repo_id = ");
        query.push_bind(repo_id.to_string());
    } else if !registered_repo_ids.is_empty() {
        query.push(" AND kc.repo_id IN (");
        let mut separated = query.separated(", ");
        for repo_id in registered_repo_ids {
            separated.push_bind(repo_id.to_string());
        }
        separated.push_unseparated(")");
    }

    if let Some(agent_id) = filter.assigned_agent_id.as_deref() {
        query.push(" AND kc.assigned_agent_id = ");
        query.push_bind(agent_id.to_string());
    }

    query.push(" ORDER BY kc.created_at DESC");

    let rows = query
        .build()
        .fetch_all(pool)
        .await
        .map_err(|error| format!("list cards from postgres: {error}"))?;

    rows.into_iter()
        .map(|row| kanban_card_row_to_record_pg(&row))
        .collect()
}

pub async fn load_card_review_state_json_pg(
    pool: &PgPool,
    card_id: &str,
) -> Result<Option<serde_json::Value>, String> {
    let Some(row) = sqlx::query(
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
    .bind(card_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("{error}"))?
    else {
        return Ok(None);
    };

    Ok(Some(json!({
        "card_id": row.try_get::<String, _>("card_id").unwrap_or_else(|_| card_id.to_string()),
        "review_round": row.try_get::<i64, _>("review_round").unwrap_or(0),
        "state": row.try_get::<String, _>("state").unwrap_or_else(|_| "idle".to_string()),
        "pending_dispatch_id": row.try_get::<Option<String>, _>("pending_dispatch_id").ok().flatten(),
        "last_verdict": row.try_get::<Option<String>, _>("last_verdict").ok().flatten(),
        "last_decision": row.try_get::<Option<String>, _>("last_decision").ok().flatten(),
        "decided_by": row.try_get::<Option<String>, _>("decided_by").ok().flatten(),
        "decided_at": row.try_get::<Option<String>, _>("decided_at").ok().flatten(),
        "review_entered_at": row.try_get::<Option<String>, _>("review_entered_at").ok().flatten(),
        "updated_at": row.try_get::<Option<String>, _>("updated_at").ok().flatten(),
    })))
}

pub async fn list_card_reviews_json_pg(
    pool: &PgPool,
    card_id: &str,
) -> Result<Vec<serde_json::Value>, String> {
    let rows = sqlx::query(
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
    .bind(card_id)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("query: {error}"))?;

    Ok(rows
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
        .collect())
}

pub async fn stalled_card_ids_pg(pool: &PgPool) -> Result<Vec<String>, String> {
    sqlx::query_scalar::<_, String>(
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
    .map_err(|error| format!("query postgres stalled cards: {error}"))
}

pub async fn list_card_audit_logs_json_pg(
    pool: &PgPool,
    card_id: &str,
) -> Result<Vec<serde_json::Value>, String> {
    let rows = sqlx::query(
        "SELECT id::BIGINT AS id, card_id, from_status, to_status, source, result, created_at::text AS created_at
         FROM kanban_audit_logs
         WHERE card_id = $1
         ORDER BY created_at DESC
         LIMIT 50",
    )
    .bind(card_id)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("query postgres card audit log: {error}"))?;

    Ok(rows
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
        .collect())
}

pub async fn load_card_json_pg(
    pool: &PgPool,
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

    let repo_id_raw: Option<String> = row
        .try_get("repo_id")
        .map_err(|error| format!("decode repo_id for {card_id}: {error}"))?;
    let repo_id = normalize_optional_github_repo_id(repo_id_raw);
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
    let github_issue_number: Option<i64> = row
        .try_get("github_issue_number")
        .map_err(|error| format!("decode github_issue_number for {card_id}: {error}"))?;
    let github_issue_url = normalize_optional_github_issue_url(
        row.try_get::<Option<String>, _>("github_issue_url")
            .map_err(|error| format!("decode github_issue_url for {card_id}: {error}"))?,
        repo_id.as_deref(),
        github_issue_number,
    );

    Ok(Some(json!({
        "id": row.try_get::<String, _>("id").map_err(|error| format!("decode id for {card_id}: {error}"))?,
        "repo_id": repo_id,
        "title": row.try_get::<String, _>("title").map_err(|error| format!("decode title for {card_id}: {error}"))?,
        "status": row.try_get::<String, _>("status").map_err(|error| format!("decode status for {card_id}: {error}"))?,
        "priority": row.try_get::<String, _>("priority").map_err(|error| format!("decode priority for {card_id}: {error}"))?,
        "assigned_agent_id": assigned_agent_id,
        "github_issue_url": github_issue_url,
        "github_issue_number": github_issue_number,
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
