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
