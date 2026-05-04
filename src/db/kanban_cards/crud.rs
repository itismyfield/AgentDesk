use sqlx::{PgPool, Row as SqlxRow};

use super::{
    CardPipelineContext, GithubIssueRef, IssueCardUpsert, IssueCardUpsertResult, UpdateCardFields,
    normalize_optional_description, normalize_optional_text,
};

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
