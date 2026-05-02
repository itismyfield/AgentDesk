use serde_json::json;
use sqlx::{PgPool, QueryBuilder, Row as SqlxRow};

#[derive(Debug, Clone, Default)]
pub struct ListCardsFilter {
    pub status: Option<String>,
    pub repo_id: Option<String>,
    pub assigned_agent_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct KanbanCardRecord {
    pub id: String,
    pub repo_id: Option<String>,
    pub title: String,
    pub status: String,
    pub priority: String,
    pub assigned_agent_id: Option<String>,
    pub github_issue_url: Option<String>,
    pub github_issue_number: Option<i64>,
    pub latest_dispatch_id: Option<String>,
    pub review_round: i64,
    pub metadata_raw: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub description: Option<String>,
    pub blocked_reason: Option<String>,
    pub review_notes: Option<String>,
    pub review_status: Option<String>,
    pub started_at: Option<String>,
    pub requested_at: Option<String>,
    pub completed_at: Option<String>,
    pub pipeline_stage_id: Option<String>,
    pub owner_agent_id: Option<String>,
    pub requester_agent_id: Option<String>,
    pub parent_card_id: Option<String>,
    pub sort_order: i64,
    pub depth: i64,
    pub latest_dispatch_status: Option<String>,
    pub latest_dispatch_title: Option<String>,
    pub latest_dispatch_type: Option<String>,
    pub latest_dispatch_result_summary: Option<String>,
    pub latest_dispatch_chain_depth: Option<i64>,
}

const CARD_SELECT_SQL_PG: &str = "SELECT kc.id, kc.repo_id, kc.title, kc.status, kc.priority, kc.assigned_agent_id, \
    kc.github_issue_url, kc.github_issue_number::bigint AS github_issue_number, kc.latest_dispatch_id, kc.review_round::bigint AS review_round, kc.metadata::text AS metadata, \
    kc.created_at::text AS created_at, kc.updated_at::text AS updated_at, \
    td.status AS d_status, td.dispatch_type AS d_type, td.title AS d_title, td.chain_depth::bigint AS d_depth, \
    td.result AS d_result, td.context AS d_context, \
    kc.description, kc.blocked_reason, kc.review_notes, kc.review_status, \
    kc.started_at::text AS started_at, kc.requested_at::text AS requested_at, kc.completed_at::text AS completed_at, kc.pipeline_stage_id, \
    kc.owner_agent_id, kc.requester_agent_id, kc.parent_card_id, kc.sort_order::bigint AS sort_order, kc.depth::bigint AS depth \
    FROM kanban_cards kc LEFT JOIN task_dispatches td ON td.id = kc.latest_dispatch_id";

pub async fn list_registered_repo_ids_pg(pool: &PgPool) -> Result<Vec<String>, String> {
    sqlx::query_scalar::<_, String>("SELECT id FROM github_repos")
        .fetch_all(pool)
        .await
        .map_err(|error| format!("list registered repos from postgres: {error}"))
}

#[derive(Debug, Clone, Default)]
pub struct IssueCardUpsert {
    pub repo_id: String,
    pub issue_number: i64,
    pub issue_url: Option<String>,
    pub title: String,
    pub description: Option<String>,
    pub priority: Option<String>,
    pub assigned_agent_id: Option<String>,
    pub metadata_json: Option<String>,
    pub status_on_create: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueCardUpsertResult {
    pub card_id: String,
    pub created: bool,
}

fn normalize_optional_text(value: Option<String>) -> Option<String> {
    value
        .map(|raw| raw.trim().to_string())
        .filter(|raw| !raw.is_empty())
}

fn normalize_optional_description(value: Option<String>) -> Option<String> {
    value
        .map(|raw| raw.trim_end().to_string())
        .filter(|raw| !raw.trim().is_empty())
}

pub async fn upsert_card_from_issue_pg(
    pool: &PgPool,
    params: IssueCardUpsert,
) -> Result<IssueCardUpsertResult, String> {
    let repo_id = params.repo_id.trim().to_string();
    if repo_id.is_empty() {
        return Err("upsert issue card: repo_id is required".to_string());
    }

    let title = params.title.trim().to_string();
    if title.is_empty() {
        return Err("upsert issue card: title is required".to_string());
    }

    let issue_url = normalize_optional_text(params.issue_url);
    let description = normalize_optional_description(params.description);
    let priority = normalize_optional_text(params.priority);
    let assigned_agent_id = normalize_optional_text(params.assigned_agent_id);
    let metadata_json = normalize_optional_text(params.metadata_json);
    let status_on_create =
        normalize_optional_text(params.status_on_create).unwrap_or_else(|| "backlog".to_string());

    let inserted_id = sqlx::query_scalar::<_, String>(
        "INSERT INTO kanban_cards (
            id,
            repo_id,
            title,
            status,
            priority,
            assigned_agent_id,
            github_issue_url,
            github_issue_number,
            description,
            metadata,
            created_at,
            updated_at
         ) VALUES (
            $1,
            $2,
            $3,
            $4,
            COALESCE($5, 'medium'),
            $6,
            $7,
            $8,
            $9,
            CAST($10 AS jsonb),
            NOW(),
            NOW()
         )
         ON CONFLICT (repo_id, github_issue_number)
         WHERE repo_id IS NOT NULL AND github_issue_number IS NOT NULL
         DO NOTHING
         RETURNING id",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(&repo_id)
    .bind(&title)
    .bind(&status_on_create)
    .bind(priority.as_deref())
    .bind(assigned_agent_id.as_deref())
    .bind(issue_url.as_deref())
    .bind(params.issue_number)
    .bind(description.as_deref())
    .bind(metadata_json.as_deref())
    .fetch_optional(pool)
    .await
    .map_err(|error| {
        format!(
            "insert postgres issue card {repo_id}#{}: {error}",
            params.issue_number
        )
    })?;

    if let Some(card_id) = inserted_id {
        return Ok(IssueCardUpsertResult {
            card_id,
            created: true,
        });
    }

    let updated_id = sqlx::query_scalar::<_, String>(
        "UPDATE kanban_cards
         SET title = $1,
             priority = COALESCE($2, kanban_cards.priority),
             assigned_agent_id = COALESCE($3, kanban_cards.assigned_agent_id),
             github_issue_url = COALESCE($4, kanban_cards.github_issue_url),
             description = COALESCE($5, kanban_cards.description),
             metadata = COALESCE(CAST($6 AS jsonb), kanban_cards.metadata),
             updated_at = NOW()
         WHERE repo_id = $7
           AND github_issue_number = $8
         RETURNING id",
    )
    .bind(&title)
    .bind(priority.as_deref())
    .bind(assigned_agent_id.as_deref())
    .bind(issue_url.as_deref())
    .bind(description.as_deref())
    .bind(metadata_json.as_deref())
    .bind(&repo_id)
    .bind(params.issue_number)
    .fetch_one(pool)
    .await
    .map_err(|error| {
        format!(
            "update postgres issue card {repo_id}#{}: {error}",
            params.issue_number
        )
    })?;

    Ok(IssueCardUpsertResult {
        card_id: updated_id,
        created: false,
    })
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

fn kanban_card_row_to_record_pg(row: &sqlx::postgres::PgRow) -> Result<KanbanCardRecord, String> {
    let latest_dispatch_status = row
        .try_get::<Option<String>, _>("d_status")
        .map_err(|error| format!("decode d_status: {error}"))?;
    let latest_dispatch_type = row
        .try_get::<Option<String>, _>("d_type")
        .map_err(|error| format!("decode d_type: {error}"))?;
    let latest_dispatch_result_raw = row
        .try_get::<Option<String>, _>("d_result")
        .map_err(|error| format!("decode d_result: {error}"))?;
    let latest_dispatch_context_raw = row
        .try_get::<Option<String>, _>("d_context")
        .map_err(|error| format!("decode d_context: {error}"))?;

    Ok(KanbanCardRecord {
        id: row
            .try_get::<String, _>("id")
            .map_err(|error| format!("decode id: {error}"))?,
        repo_id: row
            .try_get::<Option<String>, _>("repo_id")
            .map_err(|error| format!("decode repo_id: {error}"))?,
        title: row
            .try_get::<String, _>("title")
            .map_err(|error| format!("decode title: {error}"))?,
        status: row
            .try_get::<String, _>("status")
            .map_err(|error| format!("decode status: {error}"))?,
        priority: row
            .try_get::<Option<String>, _>("priority")
            .map_err(|error| format!("decode priority: {error}"))?
            .unwrap_or_else(|| "medium".to_string()),
        assigned_agent_id: row
            .try_get::<Option<String>, _>("assigned_agent_id")
            .map_err(|error| format!("decode assigned_agent_id: {error}"))?,
        github_issue_url: row
            .try_get::<Option<String>, _>("github_issue_url")
            .map_err(|error| format!("decode github_issue_url: {error}"))?,
        github_issue_number: row
            .try_get::<Option<i64>, _>("github_issue_number")
            .map_err(|error| format!("decode github_issue_number: {error}"))?,
        latest_dispatch_id: row
            .try_get::<Option<String>, _>("latest_dispatch_id")
            .map_err(|error| format!("decode latest_dispatch_id: {error}"))?,
        review_round: row
            .try_get::<Option<i64>, _>("review_round")
            .map_err(|error| format!("decode review_round: {error}"))?
            .unwrap_or(0),
        metadata_raw: row
            .try_get::<Option<String>, _>("metadata")
            .map_err(|error| format!("decode metadata: {error}"))?,
        created_at: row
            .try_get::<Option<String>, _>("created_at")
            .map_err(|error| format!("decode created_at: {error}"))?,
        updated_at: row
            .try_get::<Option<String>, _>("updated_at")
            .map_err(|error| format!("decode updated_at: {error}"))?,
        latest_dispatch_status: latest_dispatch_status.clone(),
        latest_dispatch_type: latest_dispatch_type.clone(),
        latest_dispatch_title: row
            .try_get::<Option<String>, _>("d_title")
            .map_err(|error| format!("decode d_title: {error}"))?,
        latest_dispatch_chain_depth: row
            .try_get::<Option<i64>, _>("d_depth")
            .map_err(|error| format!("decode d_depth: {error}"))?,
        latest_dispatch_result_summary: crate::dispatch::summarize_dispatch_from_text(
            latest_dispatch_type.as_deref(),
            latest_dispatch_status.as_deref(),
            latest_dispatch_result_raw.as_deref(),
            latest_dispatch_context_raw.as_deref(),
        ),
        description: row
            .try_get::<Option<String>, _>("description")
            .map_err(|error| format!("decode description: {error}"))?,
        blocked_reason: row
            .try_get::<Option<String>, _>("blocked_reason")
            .map_err(|error| format!("decode blocked_reason: {error}"))?,
        review_notes: row
            .try_get::<Option<String>, _>("review_notes")
            .map_err(|error| format!("decode review_notes: {error}"))?,
        review_status: row
            .try_get::<Option<String>, _>("review_status")
            .map_err(|error| format!("decode review_status: {error}"))?,
        started_at: row
            .try_get::<Option<String>, _>("started_at")
            .map_err(|error| format!("decode started_at: {error}"))?,
        requested_at: row
            .try_get::<Option<String>, _>("requested_at")
            .map_err(|error| format!("decode requested_at: {error}"))?,
        completed_at: row
            .try_get::<Option<String>, _>("completed_at")
            .map_err(|error| format!("decode completed_at: {error}"))?,
        pipeline_stage_id: row
            .try_get::<Option<String>, _>("pipeline_stage_id")
            .map_err(|error| format!("decode pipeline_stage_id: {error}"))?,
        owner_agent_id: row
            .try_get::<Option<String>, _>("owner_agent_id")
            .map_err(|error| format!("decode owner_agent_id: {error}"))?,
        requester_agent_id: row
            .try_get::<Option<String>, _>("requester_agent_id")
            .map_err(|error| format!("decode requester_agent_id: {error}"))?,
        parent_card_id: row
            .try_get::<Option<String>, _>("parent_card_id")
            .map_err(|error| format!("decode parent_card_id: {error}"))?,
        sort_order: row
            .try_get::<Option<i64>, _>("sort_order")
            .map_err(|error| format!("decode sort_order: {error}"))?
            .unwrap_or(0),
        depth: row
            .try_get::<Option<i64>, _>("depth")
            .map_err(|error| format!("decode depth: {error}"))?
            .unwrap_or(0),
    })
}

#[derive(Debug, Clone)]
pub struct ActiveTurnTarget {
    pub session_key: String,
    pub provider: Option<String>,
    pub thread_channel_id: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct UpdateCardFields {
    pub title: Option<String>,
    pub priority: Option<String>,
    pub assigned_agent_id: Option<String>,
    pub repo_id: Option<String>,
    pub github_issue_url: Option<String>,
    pub description: Option<String>,
    pub metadata_json: Option<String>,
    pub review_status: Option<String>,
    pub review_notes: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RetryDispatchSpec {
    pub agent_id: String,
    pub dispatch_type: String,
    pub title: String,
}

#[derive(Debug, Clone)]
pub struct DodStateRecord {
    pub deferred_dod_json: Option<String>,
    pub status: String,
    pub review_status: Option<String>,
}

#[derive(Debug, Clone)]
pub struct GithubIssueRef {
    pub repo_id: Option<String>,
    pub issue_number: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct PmDecisionCardInfo {
    pub status: String,
    pub review_status: Option<String>,
    pub blocked_reason: Option<String>,
    pub agent_id: String,
    pub title: String,
}

#[derive(Debug, Clone)]
pub struct RereviewCardInfo {
    pub status: String,
    pub assigned_agent_id: Option<String>,
    pub title: String,
    pub github_issue_url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CardPipelineContext {
    pub repo_id: Option<String>,
    pub assigned_agent_id: Option<String>,
}

pub async fn load_active_turn_targets_for_card_pg(
    pool: &PgPool,
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

pub async fn clear_session_for_turn_target_pg(
    pool: &PgPool,
    session_key: &str,
) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE sessions
         SET status = 'disconnected',
             active_dispatch_id = NULL,
             claude_session_id = NULL
         WHERE session_key = $1",
    )
    .bind(session_key)
    .execute(pool)
    .await
    .map_err(|error| anyhow::anyhow!("clear session for {session_key}: {error}"))?;
    Ok(())
}

pub async fn insert_card_pg(
    pool: &PgPool,
    id: &str,
    repo_id: Option<&str>,
    title: &str,
    status: &str,
    priority: &str,
    github_issue_url: Option<&str>,
) -> Result<(), String> {
    sqlx::query(
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
    .bind(id)
    .bind(repo_id)
    .bind(title)
    .bind(status)
    .bind(priority)
    .bind(github_issue_url)
    .execute(pool)
    .await
    .map_err(|error| format!("{error}"))?;
    Ok(())
}

pub async fn card_status_pg(pool: &PgPool, card_id: &str) -> Result<Option<String>, String> {
    sqlx::query_scalar::<_, String>("SELECT status FROM kanban_cards WHERE id = $1 LIMIT 1")
        .bind(card_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("{error}"))
}

pub async fn update_card_fields_pg(
    pool: &PgPool,
    card_id: &str,
    fields: &UpdateCardFields,
) -> Result<bool, String> {
    let result = sqlx::query(
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
    .bind(fields.title.as_deref())
    .bind(fields.priority.as_deref())
    .bind(fields.assigned_agent_id.as_deref())
    .bind(fields.repo_id.as_deref())
    .bind(fields.github_issue_url.as_deref())
    .bind(fields.description.as_deref())
    .bind(fields.metadata_json.as_deref())
    .bind(fields.review_status.as_deref())
    .bind(fields.review_notes.as_deref())
    .bind(card_id)
    .execute(pool)
    .await
    .map_err(|error| format!("{error}"))?;
    Ok(result.rows_affected() > 0)
}

pub async fn assign_card_agent_pg(
    pool: &PgPool,
    card_id: &str,
    agent_id: &str,
) -> Result<bool, String> {
    let result = sqlx::query(
        "UPDATE kanban_cards
         SET assigned_agent_id = $1,
             updated_at = NOW()
         WHERE id = $2",
    )
    .bind(agent_id)
    .bind(card_id)
    .execute(pool)
    .await
    .map_err(|error| format!("{error}"))?;
    Ok(result.rows_affected() > 0)
}

pub async fn delete_card_pg(pool: &PgPool, card_id: &str) -> Result<bool, String> {
    let result = sqlx::query("DELETE FROM kanban_cards WHERE id = $1")
        .bind(card_id)
        .execute(pool)
        .await
        .map_err(|error| format!("{error}"))?;
    Ok(result.rows_affected() > 0)
}

pub async fn latest_dispatch_id_for_card_pg(
    pool: &PgPool,
    card_id: &str,
) -> Result<Option<String>, String> {
    sqlx::query_scalar::<_, Option<String>>(
        "SELECT latest_dispatch_id FROM kanban_cards WHERE id = $1",
    )
    .bind(card_id)
    .fetch_optional(pool)
    .await
    .map(|value| value.flatten())
    .map_err(|error| format!("{error}"))
}

pub async fn assigned_agent_id_for_card_pg(
    pool: &PgPool,
    card_id: &str,
) -> Result<Option<String>, String> {
    sqlx::query_scalar::<_, Option<String>>(
        "SELECT assigned_agent_id FROM kanban_cards WHERE id = $1",
    )
    .bind(card_id)
    .fetch_optional(pool)
    .await
    .map(|value| value.flatten())
    .map_err(|error| format!("{error}"))
}

pub async fn load_retry_dispatch_spec_pg(
    pool: &PgPool,
    card_id: &str,
) -> Result<Option<RetryDispatchSpec>, String> {
    let Some((card_agent_id, card_title, latest_dispatch_id)) =
        sqlx::query_as::<_, (Option<String>, String, Option<String>)>(
            "SELECT assigned_agent_id, title, latest_dispatch_id
             FROM kanban_cards
             WHERE id = $1",
        )
        .bind(card_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("{error}"))?
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
        .await
        .map_err(|error| format!("{error}"))?
    } else {
        None
    };
    let latest_dispatch = match latest_dispatch {
        Some(row) => Some(row),
        None => sqlx::query_as::<_, (Option<String>, Option<String>, Option<String>)>(
            "SELECT to_agent_id, dispatch_type, title
                 FROM task_dispatches
                 WHERE kanban_card_id = $1
                 ORDER BY created_at DESC, id DESC
                 LIMIT 1",
        )
        .bind(card_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("{error}"))?,
    };

    let (dispatch_agent_id, dispatch_type, dispatch_title) =
        latest_dispatch.unwrap_or((None, None, None));
    Ok(Some(RetryDispatchSpec {
        agent_id: dispatch_agent_id.or(card_agent_id).unwrap_or_default(),
        dispatch_type: dispatch_type
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "implementation".to_string()),
        title: dispatch_title
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(card_title),
    }))
}

pub async fn load_dod_state_pg(
    pool: &PgPool,
    card_id: &str,
) -> Result<Option<DodStateRecord>, String> {
    sqlx::query_as::<_, (Option<String>, String, Option<String>)>(
        "SELECT deferred_dod_json, status, review_status
         FROM kanban_cards
         WHERE id = $1",
    )
    .bind(card_id)
    .fetch_optional(pool)
    .await
    .map(|row| {
        row.map(
            |(deferred_dod_json, status, review_status)| DodStateRecord {
                deferred_dod_json,
                status,
                review_status,
            },
        )
    })
    .map_err(|error| format!("load postgres DoD state: {error}"))
}

pub async fn update_deferred_dod_pg(
    pool: &PgPool,
    card_id: &str,
    dod_json: &str,
) -> Result<(), String> {
    sqlx::query(
        "UPDATE kanban_cards
         SET deferred_dod_json = $1, updated_at = NOW()
         WHERE id = $2",
    )
    .bind(dod_json)
    .bind(card_id)
    .execute(pool)
    .await
    .map_err(|error| format!("update postgres DoD state: {error}"))?;
    Ok(())
}

pub async fn update_review_clock_after_dod_pg(pool: &PgPool, card_id: &str) -> Result<(), String> {
    sqlx::query(
        "UPDATE kanban_cards
         SET review_entered_at = NOW(), awaiting_dod_at = NULL
         WHERE id = $1",
    )
    .bind(card_id)
    .execute(pool)
    .await
    .map_err(|error| format!("update postgres review clock: {error}"))?;
    Ok(())
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

pub async fn card_github_issue_ref_pg(
    pool: &PgPool,
    card_id: &str,
) -> Result<Option<GithubIssueRef>, String> {
    let Some(row) = sqlx::query(
        "SELECT repo_id, github_issue_number::BIGINT AS github_issue_number
         FROM kanban_cards
         WHERE id = $1",
    )
    .bind(card_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("query postgres card github issue: {error}"))?
    else {
        return Ok(None);
    };

    Ok(Some(GithubIssueRef {
        repo_id: row
            .try_get::<Option<String>, _>("repo_id")
            .map_err(|error| format!("decode postgres card repo_id: {error}"))?,
        issue_number: row
            .try_get::<Option<i64>, _>("github_issue_number")
            .map_err(|error| format!("decode postgres card github_issue_number: {error}"))?,
    }))
}

pub async fn update_card_description_if_changed_pg(
    pool: &PgPool,
    card_id: &str,
    body: &str,
) -> Result<(), String> {
    sqlx::query(
        "UPDATE kanban_cards
         SET description = $1,
             updated_at = NOW()
         WHERE id = $2
           AND (description IS DISTINCT FROM $1 OR description IS NULL)",
    )
    .bind(body)
    .bind(card_id)
    .execute(pool)
    .await
    .map_err(|error| format!("update postgres card description: {error}"))?;
    Ok(())
}

pub async fn load_pm_decision_card_info_pg(
    pool: &PgPool,
    card_id: &str,
) -> Result<Option<PmDecisionCardInfo>, String> {
    sqlx::query_as::<_, (String, Option<String>, Option<String>, String, String)>(
        "SELECT COALESCE(status, ''), review_status, blocked_reason, COALESCE(assigned_agent_id, ''), title
         FROM kanban_cards
         WHERE id = $1",
    )
    .bind(card_id)
    .fetch_optional(pool)
    .await
    .map(|row| {
        row.map(
            |(status, review_status, blocked_reason, agent_id, title)| PmDecisionCardInfo {
                status,
                review_status,
                blocked_reason,
                agent_id,
                title,
            },
        )
    })
    .map_err(|error| format!("load card for pm decision: {error}"))
}

pub async fn pending_pm_decision_dispatch_ids_pg(
    pool: &PgPool,
    card_id: &str,
) -> Result<Vec<String>, String> {
    sqlx::query_scalar::<_, String>(
        "SELECT id FROM task_dispatches
         WHERE kanban_card_id = $1
           AND dispatch_type = 'pm-decision'
           AND status = 'pending'",
    )
    .bind(card_id)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("load pending pm-decision dispatches: {error}"))
}

pub async fn clear_manual_intervention_marker_pg(
    pool: &PgPool,
    card_id: &str,
) -> Result<(), String> {
    sqlx::query("UPDATE kanban_cards SET blocked_reason = NULL, updated_at = NOW() WHERE id = $1")
        .bind(card_id)
        .execute(pool)
        .await
        .map_err(|error| format!("clear manual intervention marker: {error}"))?;
    Ok(())
}

pub async fn has_live_dispatch_session_pg(pool: &PgPool, card_id: &str) -> Result<bool, String> {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT
         FROM task_dispatches td
         JOIN sessions s ON s.active_dispatch_id = td.id
            AND s.status IN ('turn_active', 'awaiting_bg', 'awaiting_user', 'working', 'idle')
         WHERE td.kanban_card_id = $1
           AND td.status IN ('pending', 'dispatched')",
    )
    .bind(card_id)
    .fetch_one(pool)
    .await
    .map(|count| count > 0)
    .map_err(|error| format!("check live dispatch/session: {error}"))
}

pub async fn load_rereview_card_info_pg(
    pool: &PgPool,
    card_id: &str,
) -> Result<Option<RereviewCardInfo>, String> {
    sqlx::query_as::<_, (String, Option<String>, String, Option<String>)>(
        "SELECT status, assigned_agent_id, title, github_issue_url
         FROM kanban_cards WHERE id = $1",
    )
    .bind(card_id)
    .fetch_optional(pool)
    .await
    .map(|row| {
        row.map(
            |(status, assigned_agent_id, title, github_issue_url)| RereviewCardInfo {
                status,
                assigned_agent_id,
                title,
                github_issue_url,
            },
        )
    })
    .map_err(|error| format!("postgres lookup failed: {error}"))
}

pub async fn stale_review_dispatch_ids_pg(
    pool: &PgPool,
    card_id: &str,
) -> Result<Vec<String>, String> {
    sqlx::query_scalar::<_, String>(
        "SELECT id FROM task_dispatches
         WHERE kanban_card_id = $1
           AND dispatch_type IN ('review', 'review-decision')
           AND status IN ('pending', 'dispatched')",
    )
    .bind(card_id)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("postgres stale dispatch lookup failed: {error}"))
}

pub async fn cleanup_rereview_card_pg(pool: &PgPool, card_id: &str) -> Result<(), String> {
    sqlx::query(
        "UPDATE kanban_cards
         SET review_status = NULL,
             suggestion_pending_at = NULL,
             review_entered_at = NULL,
             awaiting_dod_at = NULL,
             updated_at = NOW()
         WHERE id = $1",
    )
    .bind(card_id)
    .execute(pool)
    .await
    .map_err(|error| format!("postgres rereview cleanup failed: {error}"))?;
    Ok(())
}

pub async fn reset_repeated_finding_rounds_pg(pool: &PgPool, card_id: &str) -> Result<(), String> {
    sqlx::query(
        "UPDATE card_review_state
         SET approach_change_round = NULL,
             session_reset_round = NULL,
             updated_at = NOW()
         WHERE card_id = $1",
    )
    .bind(card_id)
    .execute(pool)
    .await
    .map_err(|error| format!("postgres rereview repeated-finding reset failed: {error}"))?;
    Ok(())
}

pub async fn reset_completed_at_pg(pool: &PgPool, card_id: &str) -> Result<(), String> {
    sqlx::query(
        "UPDATE kanban_cards
         SET completed_at = NULL, updated_at = NOW()
         WHERE id = $1",
    )
    .bind(card_id)
    .execute(pool)
    .await
    .map_err(|error| format!("postgres completed_at reset failed: {error}"))?;
    Ok(())
}

pub async fn active_auto_queue_entry_ids_for_rereview_pg(
    pool: &PgPool,
    card_id: &str,
) -> Result<Vec<String>, String> {
    sqlx::query_scalar::<_, String>(
        "SELECT id FROM auto_queue_entries
         WHERE kanban_card_id = $1
           AND status IN ('pending', 'dispatched', 'done')
           AND run_id IN (
               SELECT id FROM auto_queue_runs WHERE status IN ('active', 'paused')
           )",
    )
    .bind(card_id)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("postgres auto-queue entry lookup failed: {error}"))
}

pub async fn card_id_by_issue_number_pg(
    pool: &PgPool,
    issue_number: i64,
) -> Result<Option<String>, String> {
    sqlx::query_scalar::<_, String>("SELECT id FROM kanban_cards WHERE github_issue_number = $1")
        .bind(issue_number)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("postgres lookup failed: {error}"))
}

pub async fn card_ids_by_issue_number_pg(
    pool: &PgPool,
    issue_number: i64,
) -> Result<Vec<String>, String> {
    sqlx::query_scalar::<_, String>(
        "SELECT id
         FROM kanban_cards
         WHERE github_issue_number = $1
         ORDER BY id ASC",
    )
    .bind(issue_number)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("{error}"))
}

pub async fn load_card_pipeline_context_pg(
    pool: &PgPool,
    card_id: &str,
) -> Result<Option<CardPipelineContext>, String> {
    let Some(row) =
        sqlx::query("SELECT repo_id, assigned_agent_id FROM kanban_cards WHERE id = $1")
            .bind(card_id)
            .fetch_optional(pool)
            .await
            .map_err(|error| format!("{error}"))?
    else {
        return Ok(None);
    };
    Ok(Some(CardPipelineContext {
        repo_id: row.try_get("repo_id").unwrap_or_default(),
        assigned_agent_id: row.try_get("assigned_agent_id").unwrap_or_default(),
    }))
}

pub async fn github_issue_url_for_card_pg(
    pool: &PgPool,
    card_id: &str,
) -> Result<Option<String>, String> {
    sqlx::query_scalar::<_, Option<String>>(
        "SELECT github_issue_url FROM kanban_cards WHERE id = $1",
    )
    .bind(card_id)
    .fetch_optional(pool)
    .await
    .map(|value| value.flatten())
    .map_err(|error| format!("{error}"))
}

pub async fn update_card_review_status_pg(
    pool: &PgPool,
    card_id: &str,
    review_status: &str,
) -> Result<(), String> {
    sqlx::query(
        "UPDATE kanban_cards
         SET review_status = $1,
             updated_at = NOW()
         WHERE id = $2",
    )
    .bind(review_status)
    .bind(card_id)
    .execute(pool)
    .await
    .map_err(|error| format!("{error}"))?;
    Ok(())
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub fn card_row_to_json(row: &sqlite_test::Row) -> sqlite_test::Result<serde_json::Value> {
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
        "github_repo": repo_id,
        "assignee_agent_id": assigned_agent_id,
        "metadata_json": metadata_raw,
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
        "latest_dispatch_status": latest_dispatch_status.clone(),
        "latest_dispatch_title": row.get::<_, Option<String>>(15).unwrap_or(None),
        "latest_dispatch_type": latest_dispatch_type.clone(),
        "latest_dispatch_result_summary": latest_dispatch_result_summary,
        "latest_dispatch_chain_depth": row.get::<_, Option<i64>>(16).unwrap_or(None),
        "child_count": 0,
    }))
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

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub fn find_active_review_dispatch_id_on_conn(
    conn: &sqlite_test::Connection,
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

pub async fn find_active_review_dispatch_id_pg(pool: &PgPool, card_id: &str) -> Option<String> {
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

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub fn resolve_agent_id_from_channel_id_on_conn(
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

pub async fn resolve_agent_id_from_channel_id_with_pg(
    pool: &PgPool,
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
pub fn resolve_existing_agent_id_on_conn(
    conn: &sqlite_test::Connection,
    agent_id: &str,
) -> Option<String> {
    conn.query_row(
        "SELECT id FROM agents WHERE id = ?1 LIMIT 1",
        [agent_id],
        |row| row.get(0),
    )
    .ok()
}

pub async fn resolve_existing_agent_id_with_pg(pool: &PgPool, agent_id: &str) -> Option<String> {
    sqlx::query("SELECT id FROM agents WHERE id = $1 LIMIT 1")
        .bind(agent_id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .and_then(|row| row.try_get::<String, _>("id").ok())
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub fn count_live_auto_queue_entries_for_card_on_conn(
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
pub fn clear_force_transition_terminalized_links_on_conn(
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
pub fn skip_live_auto_queue_entries_for_card_legacy(
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
pub fn cleanup_force_transition_revert_on_conn(
    conn: &sqlite_test::Connection,
    card_id: &str,
    target_status: &str,
) -> anyhow::Result<(usize, usize)> {
    let reason = format!("force-transition to {target_status}");
    let skipped_auto_queue_entries = count_live_auto_queue_entries_for_card_on_conn(conn, card_id)?;
    let cancelled_dispatches =
        crate::dispatch::cancel_active_dispatches_for_card_on_conn(conn, card_id, Some(&reason))?;
    skip_live_auto_queue_entries_for_card_legacy(conn, card_id)?;
    clear_force_transition_terminalized_links_on_conn(conn, card_id)?;
    crate::kanban::cleanup_force_transition_revert_fields_on_conn(conn, card_id)?;

    Ok((cancelled_dispatches, skipped_auto_queue_entries))
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub fn move_auto_queue_entry_to_dispatched_on_conn(
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

pub async fn move_auto_queue_entry_to_dispatched_on_pg(
    pool: &PgPool,
    entry_id: &str,
    trigger_source: &str,
    options: &crate::db::auto_queue::EntryStatusUpdateOptions,
) -> Result<(), String> {
    crate::db::auto_queue::reactivate_done_entry_on_pg(pool, entry_id, trigger_source, options)
        .await
        .map(|_| ())
}

pub async fn reactivate_done_auto_queue_entries_pg(
    pool: &PgPool,
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
pub fn load_card_metadata_map_on_conn(
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

pub async fn load_card_metadata_map_pg(
    pool: &PgPool,
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
pub fn save_card_metadata_map_on_conn(
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

pub async fn save_card_metadata_map_pg(
    pool: &PgPool,
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
pub fn mark_api_reopen_skip_preflight_on_conn(
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

pub async fn mark_api_reopen_skip_preflight_on_pg(
    pool: &PgPool,
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
pub fn clear_api_reopen_skip_preflight_on_conn(
    conn: &sqlite_test::Connection,
    card_id: &str,
) -> anyhow::Result<()> {
    let mut metadata = load_card_metadata_map_on_conn(conn, card_id)?;
    metadata.remove("skip_preflight_once");
    save_card_metadata_map_on_conn(conn, card_id, &metadata)
}

pub async fn clear_api_reopen_skip_preflight_on_pg(
    pool: &PgPool,
    card_id: &str,
) -> anyhow::Result<()> {
    let mut metadata = load_card_metadata_map_pg(pool, card_id).await?;
    metadata.remove("skip_preflight_once");
    save_card_metadata_map_pg(pool, card_id, &metadata).await
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub fn consume_api_reopen_preflight_skip_on_conn(
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

pub async fn consume_api_reopen_preflight_skip_on_pg(
    pool: &PgPool,
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
pub fn clear_reopen_preflight_cache_on_conn(
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

pub async fn clear_reopen_preflight_cache_on_pg(
    pool: &PgPool,
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

pub async fn active_dispatch_ids_for_card_pg(
    pool: &PgPool,
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

pub async fn cancelled_dispatch_ids_among_pg(
    pool: &PgPool,
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

pub async fn clear_all_threads_pg(pool: &PgPool, card_id: &str) -> anyhow::Result<()> {
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
