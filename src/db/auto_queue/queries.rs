use sqlx::{PgPool, Row as SqlxRow};

#[derive(Debug, Clone, Default)]
pub struct GenerateCardFilter {
    pub repo: Option<String>,
    pub agent_id: Option<String>,
    pub issue_numbers: Option<Vec<i64>>,
}

#[derive(Debug, Clone, Default)]
pub struct StatusFilter {
    pub repo: Option<String>,
    pub agent_id: Option<String>,
}

fn normalized_status_filter_value(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

#[derive(Debug, Clone)]
pub struct BacklogCardRecord {
    pub card_id: String,
    pub repo_id: Option<String>,
    pub assigned_agent_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct GenerateCandidateRecord {
    pub card_id: String,
    pub agent_id: String,
    pub priority: String,
    pub description: Option<String>,
    pub metadata: Option<String>,
    pub github_issue_number: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct AutoQueueRunRecord {
    pub id: String,
    pub repo: Option<String>,
    pub agent_id: Option<String>,
    pub review_mode: String,
    pub status: String,
    pub timeout_minutes: i64,
    pub ai_model: Option<String>,
    pub ai_rationale: Option<String>,
    pub created_at: i64,
    pub completed_at: Option<i64>,
    pub max_concurrent_threads: i64,
    pub thread_group_count: i64,
}

#[derive(Debug, Clone)]
pub struct StatusEntryRecord {
    pub id: String,
    pub agent_id: String,
    pub card_id: String,
    pub dispatch_id: Option<String>,
    pub priority_rank: i64,
    pub reason: Option<String>,
    pub status: String,
    pub retry_count: i64,
    pub created_at: i64,
    pub dispatched_at: Option<i64>,
    pub completed_at: Option<i64>,
    pub card_title: Option<String>,
    pub github_issue_number: Option<i64>,
    pub github_repo: Option<String>,
    pub thread_group: i64,
    pub slot_index: Option<i64>,
    pub batch_phase: i64,
    pub channel_thread_map: Option<String>,
    pub active_thread_id: Option<String>,
    pub card_status: Option<String>,
    pub review_round: i64,
}

#[derive(Debug, Clone)]
pub struct AutoQueueRunHistoryRecord {
    pub id: String,
    pub repo: Option<String>,
    pub agent_id: Option<String>,
    pub status: String,
    pub created_at: i64,
    pub completed_at: Option<i64>,
    pub entry_count: i64,
    pub done_count: i64,
    pub skipped_count: i64,
    pub pending_count: i64,
    pub dispatched_count: i64,
}

pub async fn find_latest_run_id_pg(
    pool: &PgPool,
    filter: &StatusFilter,
) -> Result<Option<String>, sqlx::Error> {
    let repo = normalized_status_filter_value(filter.repo.as_deref());
    let agent_id = normalized_status_filter_value(filter.agent_id.as_deref());

    sqlx::query_scalar::<_, String>(
        "SELECT id
         FROM auto_queue_runs r
         WHERE (
             $1::TEXT IS NULL
             OR r.repo = $1
             OR EXISTS (
                 SELECT 1
                 FROM auto_queue_entries e
                 JOIN kanban_cards kc ON kc.id = e.kanban_card_id
                 WHERE e.run_id = r.id
                   AND kc.repo_id = $1
             )
         )
           AND (
             $2::TEXT IS NULL
             OR r.agent_id = $2
             OR EXISTS (
                 SELECT 1
                 FROM auto_queue_entries e
                 WHERE e.run_id = r.id
                   AND e.agent_id = $2
             )
         )
         ORDER BY created_at DESC
         LIMIT 1",
    )
    .bind(repo)
    .bind(agent_id)
    .fetch_optional(pool)
    .await
}

pub async fn get_run_pg(
    pool: &PgPool,
    run_id: &str,
) -> Result<Option<AutoQueueRunRecord>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT id,
                repo,
                agent_id,
                COALESCE(review_mode, 'enabled') AS review_mode,
                status,
                timeout_minutes::BIGINT AS timeout_minutes,
                ai_model,
                ai_rationale,
                EXTRACT(EPOCH FROM created_at)::BIGINT * 1000 AS created_at,
                CASE WHEN completed_at IS NOT NULL
                    THEN EXTRACT(EPOCH FROM completed_at)::BIGINT * 1000
                END AS completed_at,
                COALESCE(max_concurrent_threads, 1)::BIGINT AS max_concurrent_threads,
                COALESCE(thread_group_count, 1)::BIGINT AS thread_group_count
         FROM auto_queue_runs
         WHERE id = $1",
    )
    .bind(run_id)
    .fetch_optional(pool)
    .await?;

    row.map(|row| auto_queue_run_record_from_pg_row(&row))
        .transpose()
}

pub async fn get_status_entry_pg(
    pool: &PgPool,
    entry_id: &str,
) -> Result<Option<StatusEntryRecord>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT e.id,
                e.agent_id,
                COALESCE(e.kanban_card_id, '') AS kanban_card_id,
                e.dispatch_id,
                e.priority_rank::BIGINT AS priority_rank,
                e.reason,
                e.status,
                COALESCE(e.retry_count, 0)::BIGINT AS retry_count,
                EXTRACT(EPOCH FROM e.created_at)::BIGINT * 1000 AS created_at,
                CASE WHEN e.dispatched_at IS NOT NULL
                    THEN EXTRACT(EPOCH FROM e.dispatched_at)::BIGINT * 1000
                END AS dispatched_at,
                CASE WHEN e.completed_at IS NOT NULL
                    THEN EXTRACT(EPOCH FROM e.completed_at)::BIGINT * 1000
                END AS completed_at,
                kc.title,
                kc.github_issue_number::BIGINT AS github_issue_number,
                kc.github_issue_url AS github_repo,
                COALESCE(e.thread_group, 0)::BIGINT AS thread_group,
                e.slot_index::BIGINT AS slot_index,
                COALESCE(e.batch_phase, 0)::BIGINT AS batch_phase,
                kc.channel_thread_map::text AS channel_thread_map,
                kc.active_thread_id,
                kc.status AS card_status,
                GREATEST(COALESCE(crs.review_round, 0), COALESCE(kc.review_round, 0))::BIGINT AS review_round
         FROM auto_queue_entries e
         LEFT JOIN kanban_cards kc ON e.kanban_card_id = kc.id
         LEFT JOIN card_review_state crs ON e.kanban_card_id = crs.card_id
         WHERE e.id = $1",
    )
    .bind(entry_id)
    .fetch_optional(pool)
    .await?;

    row.map(|row| status_entry_record_from_pg_row(&row))
        .transpose()
}

pub async fn list_status_entries_pg(
    pool: &PgPool,
    run_id: &str,
    filter: &StatusFilter,
) -> Result<Vec<StatusEntryRecord>, sqlx::Error> {
    let agent_id = normalized_status_filter_value(filter.agent_id.as_deref());
    let repo = normalized_status_filter_value(filter.repo.as_deref());

    let rows = sqlx::query(
        "SELECT e.id,
                e.agent_id,
                COALESCE(e.kanban_card_id, '') AS kanban_card_id,
                e.dispatch_id,
                e.priority_rank::BIGINT AS priority_rank,
                e.reason,
                e.status,
                COALESCE(e.retry_count, 0)::BIGINT AS retry_count,
                EXTRACT(EPOCH FROM e.created_at)::BIGINT * 1000 AS created_at,
                CASE WHEN e.dispatched_at IS NOT NULL
                    THEN EXTRACT(EPOCH FROM e.dispatched_at)::BIGINT * 1000
                END AS dispatched_at,
                CASE WHEN e.completed_at IS NOT NULL
                    THEN EXTRACT(EPOCH FROM e.completed_at)::BIGINT * 1000
                END AS completed_at,
                kc.title,
                kc.github_issue_number::BIGINT AS github_issue_number,
                kc.github_issue_url AS github_repo,
                COALESCE(e.thread_group, 0)::BIGINT AS thread_group,
                e.slot_index::BIGINT AS slot_index,
                COALESCE(e.batch_phase, 0)::BIGINT AS batch_phase,
                kc.channel_thread_map::text AS channel_thread_map,
                kc.active_thread_id,
                kc.status AS card_status,
                GREATEST(COALESCE(crs.review_round, 0), COALESCE(kc.review_round, 0))::BIGINT AS review_round
         FROM auto_queue_entries e
         LEFT JOIN kanban_cards kc ON e.kanban_card_id = kc.id
         LEFT JOIN card_review_state crs ON e.kanban_card_id = crs.card_id
         WHERE e.run_id = $1
           AND ($2::TEXT IS NULL OR e.agent_id = $2)
           AND ($3::TEXT IS NULL OR kc.repo_id = $3)
         ORDER BY e.priority_rank ASC",
    )
    .bind(run_id)
    .bind(agent_id)
    .bind(repo)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| status_entry_record_from_pg_row(&row))
        .collect()
}

pub async fn list_run_history_pg(
    pool: &PgPool,
    filter: &StatusFilter,
    limit: usize,
) -> Result<Vec<AutoQueueRunHistoryRecord>, sqlx::Error> {
    let repo = normalized_status_filter_value(filter.repo.as_deref());
    let agent_id = normalized_status_filter_value(filter.agent_id.as_deref());
    let limit = limit.clamp(1, 20) as i64;

    let rows = sqlx::query(
        "SELECT r.id,
                r.repo,
                r.agent_id,
                r.status,
                EXTRACT(EPOCH FROM r.created_at)::BIGINT * 1000 AS created_at,
                CASE WHEN r.completed_at IS NOT NULL
                    THEN EXTRACT(EPOCH FROM r.completed_at)::BIGINT * 1000
                END AS completed_at,
                COUNT(e.id)::BIGINT AS entry_count,
                COALESCE(SUM(CASE WHEN e.status = 'done' THEN 1 ELSE 0 END), 0)::BIGINT AS done_count,
                COALESCE(SUM(CASE WHEN e.status = 'skipped' THEN 1 ELSE 0 END), 0)::BIGINT AS skipped_count,
                COALESCE(SUM(CASE WHEN e.status = 'pending' THEN 1 ELSE 0 END), 0)::BIGINT AS pending_count,
                COALESCE(SUM(CASE WHEN e.status = 'dispatched' THEN 1 ELSE 0 END), 0)::BIGINT AS dispatched_count
         FROM auto_queue_runs r
         LEFT JOIN auto_queue_entries e ON e.run_id = r.id
         LEFT JOIN kanban_cards kc ON kc.id = e.kanban_card_id
         WHERE ($1::TEXT IS NULL OR COALESCE(kc.repo_id, r.repo, '') = $1)
           AND ($2::TEXT IS NULL OR COALESCE(e.agent_id, r.agent_id, '') = $2)
         GROUP BY r.id, r.repo, r.agent_id, r.status, r.created_at, r.completed_at
         ORDER BY r.created_at DESC
         LIMIT $3",
    )
    .bind(repo)
    .bind(agent_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| auto_queue_run_history_record_from_pg_row(&row))
        .collect()
}

pub async fn list_backlog_cards_pg(
    pool: &PgPool,
    filter: &GenerateCardFilter,
) -> Result<Vec<BacklogCardRecord>, sqlx::Error> {
    let repo = filter.repo.as_deref().filter(|value| !value.is_empty());
    let agent_id = filter.agent_id.as_deref().filter(|value| !value.is_empty());
    let issue_numbers = filter
        .issue_numbers
        .as_ref()
        .filter(|nums| !nums.is_empty())
        .cloned();

    let rows = sqlx::query(
        "SELECT kc.id,
                kc.repo_id,
                kc.assigned_agent_id
         FROM kanban_cards kc
         WHERE kc.status = 'backlog'
           AND ($1::TEXT IS NULL OR kc.repo_id = $1)
           AND ($2::TEXT IS NULL OR kc.assigned_agent_id = $2)
           AND ($3::BIGINT[] IS NULL OR kc.github_issue_number::BIGINT = ANY($3::BIGINT[]))",
    )
    .bind(repo)
    .bind(agent_id)
    .bind(issue_numbers)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            Ok(BacklogCardRecord {
                card_id: row.try_get("id")?,
                repo_id: row.try_get("repo_id")?,
                assigned_agent_id: row.try_get("assigned_agent_id")?,
            })
        })
        .collect()
}

pub async fn list_generate_candidates_pg(
    pool: &PgPool,
    filter: &GenerateCardFilter,
    enqueueable_states: &[String],
) -> Result<Vec<GenerateCandidateRecord>, sqlx::Error> {
    let repo = filter.repo.as_deref().filter(|value| !value.is_empty());
    let agent_id = filter.agent_id.as_deref().filter(|value| !value.is_empty());
    let issue_numbers = filter
        .issue_numbers
        .as_ref()
        .filter(|nums| !nums.is_empty())
        .cloned();

    let rows = sqlx::query(
        "SELECT kc.id,
                kc.assigned_agent_id,
                kc.priority,
                kc.description,
                kc.metadata::TEXT AS metadata,
                kc.github_issue_number::BIGINT AS github_issue_number
         FROM kanban_cards kc
         WHERE kc.status = ANY($1::TEXT[])
           AND ($2::TEXT IS NULL OR kc.repo_id = $2)
           AND ($3::TEXT IS NULL OR kc.assigned_agent_id = $3)
           AND ($4::BIGINT[] IS NULL OR kc.github_issue_number::BIGINT = ANY($4::BIGINT[]))
         ORDER BY
           CASE kc.priority
             WHEN 'urgent' THEN 0
             WHEN 'high' THEN 1
             WHEN 'medium' THEN 2
             WHEN 'low' THEN 3
             ELSE 4
           END,
           kc.created_at ASC",
    )
    .bind(enqueueable_states)
    .bind(repo)
    .bind(agent_id)
    .bind(issue_numbers)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            Ok(GenerateCandidateRecord {
                card_id: row.try_get("id")?,
                agent_id: row
                    .try_get::<Option<String>, _>("assigned_agent_id")?
                    .unwrap_or_default(),
                priority: row
                    .try_get::<Option<String>, _>("priority")?
                    .unwrap_or_else(|| "medium".to_string()),
                description: row.try_get("description")?,
                metadata: row.try_get("metadata")?,
                github_issue_number: row.try_get("github_issue_number")?,
            })
        })
        .collect()
}

pub async fn count_cards_by_status_pg(
    pool: &PgPool,
    repo: Option<&str>,
    agent_id: Option<&str>,
    status: &str,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT
         FROM kanban_cards
         WHERE status = $1
           AND ($2::TEXT IS NULL OR repo_id = $2)
           AND ($3::TEXT IS NULL OR assigned_agent_id = $3)",
    )
    .bind(status)
    .bind(repo.filter(|value| !value.is_empty()))
    .bind(agent_id.filter(|value| !value.is_empty()))
    .fetch_one(pool)
    .await
}

fn auto_queue_run_record_from_pg_row(
    row: &sqlx::postgres::PgRow,
) -> Result<AutoQueueRunRecord, sqlx::Error> {
    Ok(AutoQueueRunRecord {
        id: row.try_get("id")?,
        repo: row.try_get("repo")?,
        agent_id: row.try_get("agent_id")?,
        review_mode: row.try_get("review_mode")?,
        status: row.try_get("status")?,
        timeout_minutes: row.try_get("timeout_minutes")?,
        ai_model: row.try_get("ai_model")?,
        ai_rationale: row.try_get("ai_rationale")?,
        created_at: row.try_get("created_at")?,
        completed_at: row.try_get("completed_at")?,
        max_concurrent_threads: row.try_get("max_concurrent_threads")?,
        thread_group_count: row.try_get("thread_group_count")?,
    })
}

fn status_entry_record_from_pg_row(
    row: &sqlx::postgres::PgRow,
) -> Result<StatusEntryRecord, sqlx::Error> {
    Ok(StatusEntryRecord {
        id: row.try_get("id")?,
        agent_id: row.try_get("agent_id")?,
        card_id: row.try_get("kanban_card_id")?,
        dispatch_id: row.try_get("dispatch_id")?,
        priority_rank: row.try_get("priority_rank")?,
        reason: row.try_get("reason")?,
        status: row.try_get("status")?,
        retry_count: row.try_get("retry_count")?,
        created_at: row.try_get("created_at")?,
        dispatched_at: row.try_get("dispatched_at")?,
        completed_at: row.try_get("completed_at")?,
        card_title: row.try_get("title")?,
        github_issue_number: row.try_get("github_issue_number")?,
        github_repo: row.try_get("github_repo")?,
        thread_group: row.try_get("thread_group")?,
        slot_index: row.try_get("slot_index")?,
        batch_phase: row.try_get("batch_phase")?,
        channel_thread_map: row.try_get("channel_thread_map")?,
        active_thread_id: row.try_get("active_thread_id")?,
        card_status: row.try_get("card_status")?,
        review_round: row.try_get("review_round")?,
    })
}

fn auto_queue_run_history_record_from_pg_row(
    row: &sqlx::postgres::PgRow,
) -> Result<AutoQueueRunHistoryRecord, sqlx::Error> {
    Ok(AutoQueueRunHistoryRecord {
        id: row.try_get("id")?,
        repo: row.try_get("repo")?,
        agent_id: row.try_get("agent_id")?,
        status: row.try_get("status")?,
        created_at: row.try_get("created_at")?,
        completed_at: row.try_get("completed_at")?,
        entry_count: row.try_get("entry_count")?,
        done_count: row.try_get("done_count")?,
        skipped_count: row.try_get("skipped_count")?,
        pending_count: row.try_get("pending_count")?,
        dispatched_count: row.try_get("dispatched_count")?,
    })
}
