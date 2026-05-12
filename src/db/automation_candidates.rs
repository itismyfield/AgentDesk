use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, Row, Transaction};

use crate::services::automation_candidate_contract::PIPELINE_STAGE_ID;

const MAX_ITERATIONS: i32 = 10;

#[derive(Debug, Clone)]
pub struct IterationRecord {
    pub id: String,
    pub card_id: String,
    pub iteration: i32,
    pub branch: String,
    pub commit_hash: Option<String>,
    pub metric_before: Option<f64>,
    pub metric_after: Option<f64>,
    pub is_simplification: bool,
    pub status: String,
    pub description: Option<String>,
    pub allowed_write_paths_used: Vec<String>,
    pub run_seconds: Option<i32>,
    pub crash_trace: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct InsertIterationParams {
    pub card_id: String,
    pub iteration: i32,
    pub branch: String,
    pub commit_hash: Option<String>,
    pub metric_before: Option<f64>,
    pub metric_after: Option<f64>,
    pub is_simplification: bool,
    pub status: String,
    pub description: Option<String>,
    pub allowed_write_paths_used: Vec<String>,
    pub run_seconds: Option<i32>,
    pub crash_trace: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MaterializeCandidateCardParams {
    pub title: String,
    pub repo_id: Option<String>,
    pub priority: Option<String>,
    pub assigned_agent_id: Option<String>,
    pub description: Option<String>,
    pub metadata_json: String,
    pub dedupe_key: Option<String>,
    pub start_ready: bool,
}

#[derive(Debug, Clone)]
pub struct MaterializedCandidateCard {
    pub card_id: String,
    pub created: bool,
    pub status: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IterationOutcomeAction {
    KeepContinue,
    KeepFinalGate,
    DiscardRequeue,
    DiscardFinalGate,
}

#[derive(Debug, Clone)]
pub struct PersistedIterationOutcome {
    pub record: IterationRecord,
    pub child_card_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricDirection {
    LowerIsBetter,
    HigherIsBetter,
}

impl MetricDirection {
    pub fn parse(value: Option<&str>) -> Self {
        match value
            .unwrap_or("lower_is_better")
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "higher" | "higher_is_better" | "maximize" | "max" => Self::HigherIsBetter,
            _ => Self::LowerIsBetter,
        }
    }
}

/// Deterministically compute keep/discard from metrics.
/// Rules (in order):
///  1. `crashed` / `timeout` → status is already set by caller, returned as-is.
///  2. `is_simplification=true` → keep (simplification always wins).
///  3. metric improvement in the configured direction → keep.
///  4. Missing/equal/regressed metrics → discard.
pub fn compute_verdict(
    metric_before: Option<f64>,
    metric_after: Option<f64>,
    is_simplification: bool,
    caller_status: &str,
    metric_direction: MetricDirection,
) -> &'static str {
    if matches!(caller_status, "crashed" | "timeout") {
        return "discard";
    }
    if is_simplification {
        return "keep";
    }
    match (metric_before, metric_after) {
        (Some(before), Some(after)) => match metric_direction {
            MetricDirection::LowerIsBetter if after < before => "keep",
            MetricDirection::HigherIsBetter if after > before => "keep",
            _ => "discard",
        },
        _ => "discard",
    }
}

pub fn is_final_iteration(iteration: i32) -> bool {
    iteration >= MAX_ITERATIONS
}

fn ensure_one_card_row_affected(
    rows_affected: u64,
    action: &str,
    card_id: &str,
) -> Result<(), String> {
    if rows_affected == 1 {
        return Ok(());
    }
    Err(format!(
        "{action} affected {rows_affected} rows for automation candidate card {card_id}; expected 1"
    ))
}

#[cfg(test)]
mod verdict_tests;

pub async fn insert_iteration_pg(
    pool: &PgPool,
    params: InsertIterationParams,
) -> Result<IterationRecord, String> {
    let row = sqlx::query(
        r#"
        INSERT INTO automation_candidate_iterations (
            card_id, iteration, branch, commit_hash,
            metric_before, metric_after, is_simplification,
            status, description, allowed_write_paths_used,
            run_seconds, crash_trace
        ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
        RETURNING id::text AS id, card_id, iteration, branch, commit_hash,
                  metric_before, metric_after, is_simplification,
                  status, description, allowed_write_paths_used,
                  run_seconds, crash_trace, created_at
        "#,
    )
    .bind(&params.card_id)
    .bind(params.iteration)
    .bind(&params.branch)
    .bind(params.commit_hash.as_deref())
    .bind(params.metric_before)
    .bind(params.metric_after)
    .bind(params.is_simplification)
    .bind(&params.status)
    .bind(params.description.as_deref())
    .bind(&params.allowed_write_paths_used)
    .bind(params.run_seconds)
    .bind(params.crash_trace.as_deref())
    .fetch_one(pool)
    .await
    .map_err(|error| format!("insert iteration: {error}"))?;

    row_to_record(&row)
}

pub async fn persist_iteration_outcome_pg(
    pool: &PgPool,
    params: InsertIterationParams,
    action: IterationOutcomeAction,
) -> Result<PersistedIterationOutcome, String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin iteration transaction: {error}"))?;

    validate_active_candidate_iteration_in_tx(&mut tx, &params.card_id, params.iteration).await?;

    let row = sqlx::query(
        r#"
        INSERT INTO automation_candidate_iterations (
            card_id, iteration, branch, commit_hash,
            metric_before, metric_after, is_simplification,
            status, description, allowed_write_paths_used,
            run_seconds, crash_trace
        ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
        RETURNING id::text AS id, card_id, iteration, branch, commit_hash,
                  metric_before, metric_after, is_simplification,
                  status, description, allowed_write_paths_used,
                  run_seconds, crash_trace, created_at
        "#,
    )
    .bind(&params.card_id)
    .bind(params.iteration)
    .bind(&params.branch)
    .bind(params.commit_hash.as_deref())
    .bind(params.metric_before)
    .bind(params.metric_after)
    .bind(params.is_simplification)
    .bind(&params.status)
    .bind(params.description.as_deref())
    .bind(&params.allowed_write_paths_used)
    .bind(params.run_seconds)
    .bind(params.crash_trace.as_deref())
    .fetch_one(&mut *tx)
    .await
    .map_err(|error| format!("insert iteration: {error}"))?;

    let record = row_to_record(&row)?;
    let child_card_id = match action {
        IterationOutcomeAction::KeepContinue => {
            update_card_program_current_iteration_in_tx(&mut tx, &params.card_id, params.iteration)
                .await?;
            None
        }
        IterationOutcomeAction::KeepFinalGate | IterationOutcomeAction::DiscardFinalGate => {
            update_card_program_current_iteration_in_tx(&mut tx, &params.card_id, params.iteration)
                .await?;
            transition_card_status_in_tx(&mut tx, &params.card_id, "review").await?;
            None
        }
        IterationOutcomeAction::DiscardRequeue => {
            update_card_program_current_iteration_in_tx(&mut tx, &params.card_id, params.iteration)
                .await?;
            let (parent_title, parent_metadata) =
                load_card_header_in_tx(&mut tx, &params.card_id).await?;
            transition_card_status_in_tx(&mut tx, &params.card_id, "review").await?;
            Some(
                create_child_candidate_card_in_tx(
                    &mut tx,
                    &params.card_id,
                    &parent_title,
                    params.iteration + 1,
                    &parent_metadata,
                )
                .await?,
            )
        }
    };

    tx.commit()
        .await
        .map_err(|error| format!("commit iteration transaction: {error}"))?;

    Ok(PersistedIterationOutcome {
        record,
        child_card_id,
    })
}

async fn validate_active_candidate_iteration_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    card_id: &str,
    iteration: i32,
) -> Result<serde_json::Value, String> {
    let Some((status, program)) = load_card_program_for_update_in_tx(tx, card_id).await? else {
        return ensure_one_card_row_affected(0, "lock automation candidate card", card_id)
            .map(|_| serde_json::Value::Null);
    };
    if !is_active_iteration_status(&status) {
        return Err(format!(
            "automation candidate card {card_id} is not active for iteration writes: {status}"
        ));
    }
    let expected = program_current_iteration(&program).saturating_add(1);
    if iteration != expected {
        return Err(format!(
            "automation candidate card {card_id} iteration out of sequence: expected {expected}, got {iteration}"
        ));
    }
    let max = program_iteration_budget(&program);
    if iteration > max {
        return Err(format!(
            "automation candidate card {card_id} iteration budget exceeded: max {max}, got {iteration}"
        ));
    }
    Ok(program)
}

pub async fn list_iterations_for_card_pg(
    pool: &PgPool,
    card_id: &str,
) -> Result<Vec<IterationRecord>, String> {
    let rows = sqlx::query(
        r#"
        SELECT id::text AS id, card_id, iteration, branch, commit_hash,
               metric_before, metric_after, is_simplification,
               status, description, allowed_write_paths_used,
               run_seconds, crash_trace, created_at
        FROM automation_candidate_iterations
        WHERE card_id = $1
        ORDER BY iteration ASC
        "#,
    )
    .bind(card_id)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("list iterations: {error}"))?;

    rows.iter().map(|row| row_to_record(row)).collect()
}

/// Load `title` and `metadata` JSON for a card — used by the discard path to seed child cards.
pub async fn load_card_header_pg(
    pool: &PgPool,
    card_id: &str,
) -> Result<Option<(String, serde_json::Value)>, String> {
    let row = sqlx::query(
        r#"
        SELECT title, metadata::text AS metadata
        FROM kanban_cards
        WHERE id = $1
          AND pipeline_stage_id = $2
        "#,
    )
    .bind(card_id)
    .bind(PIPELINE_STAGE_ID)
    .fetch_optional(pool)
    .await
    .map_err(|e| format!("load card header: {e}"))?;

    let Some(row) = row else { return Ok(None) };
    let title: String = row.try_get("title").unwrap_or_else(|_| card_id.to_string());
    let meta_raw: Option<String> = row.try_get("metadata").unwrap_or(None);
    let meta = meta_raw
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or(serde_json::Value::Object(Default::default()));
    Ok(Some((title, meta)))
}

async fn load_card_header_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    card_id: &str,
) -> Result<(String, serde_json::Value), String> {
    let row = sqlx::query(
        r#"
        SELECT title, metadata::text AS metadata
        FROM kanban_cards
        WHERE id = $1
          AND pipeline_stage_id = $2
        "#,
    )
    .bind(card_id)
    .bind(PIPELINE_STAGE_ID)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|error| format!("load card header: {error}"))?;

    let Some(row) = row else {
        return Err(format!("automation candidate card not found: {card_id}"));
    };
    let title: String = row.try_get("title").unwrap_or_else(|_| card_id.to_string());
    let meta_raw: Option<String> = row.try_get("metadata").unwrap_or(None);
    let meta = meta_raw
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or(serde_json::Value::Object(Default::default()));
    Ok((title, meta))
}

async fn update_card_program_current_iteration_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    card_id: &str,
    current_iteration: i32,
) -> Result<(), String> {
    let result = sqlx::query(
        r#"
        UPDATE kanban_cards
           SET metadata = jsonb_set(
                   COALESCE(metadata, '{}'::jsonb),
                   '{program,current_iteration}',
                   to_jsonb($1::int),
                   true
               ),
               updated_at = NOW()
         WHERE id = $2
           AND pipeline_stage_id = $3
        "#,
    )
    .bind(current_iteration)
    .bind(card_id)
    .bind(PIPELINE_STAGE_ID)
    .execute(&mut **tx)
    .await
    .map_err(|error| {
        format!("update card {card_id} program current_iteration to {current_iteration}: {error}")
    })?;
    ensure_one_card_row_affected(
        result.rows_affected(),
        "update card program current_iteration",
        card_id,
    )?;
    Ok(())
}

async fn transition_card_status_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    card_id: &str,
    new_status: &str,
) -> Result<(), String> {
    let result = sqlx::query(
        r#"
        UPDATE kanban_cards
           SET status = $1, updated_at = NOW()
         WHERE id = $2
           AND pipeline_stage_id = $3
        "#,
    )
    .bind(new_status)
    .bind(card_id)
    .bind(PIPELINE_STAGE_ID)
    .execute(&mut **tx)
    .await
    .map_err(|error| format!("transition card {card_id} to {new_status}: {error}"))?;
    ensure_one_card_row_affected(result.rows_affected(), "transition card status", card_id)?;
    Ok(())
}

async fn create_child_candidate_card_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    parent_card_id: &str,
    parent_title: &str,
    iteration: i32,
    parent_metadata: &serde_json::Value,
) -> Result<String, String> {
    let child_id = uuid::Uuid::new_v4().to_string();
    let child_title = format!("{parent_title} [iter {iteration}]");

    let mut child_meta = parent_metadata.clone();
    if let Some(program) = child_meta.get_mut("program") {
        if let Some(obj) = program.as_object_mut() {
            obj.insert(
                "current_iteration".to_string(),
                serde_json::Value::Number((iteration - 1).into()),
            );
        }
    }
    child_meta["parent_iteration"] = serde_json::Value::Number((iteration - 1).into());
    child_meta["iteration"] = serde_json::Value::Number(iteration.into());

    let child_meta_json = serde_json::to_string(&child_meta)
        .map_err(|error| format!("serialize child metadata: {error}"))?;

    sqlx::query(
        r#"
        INSERT INTO kanban_cards (
            id, title, status, priority,
            pipeline_stage_id, parent_card_id,
            metadata, created_at, updated_at
        ) VALUES (
            $1, $2, 'ready', 'medium',
            $3, $4,
            CAST($5 AS jsonb), NOW(), NOW()
        )
        "#,
    )
    .bind(&child_id)
    .bind(&child_title)
    .bind(PIPELINE_STAGE_ID)
    .bind(parent_card_id)
    .bind(&child_meta_json)
    .execute(&mut **tx)
    .await
    .map_err(|error| format!("create child card: {error}"))?;

    Ok(child_id)
}

pub async fn materialize_candidate_card_pg(
    pool: &PgPool,
    params: MaterializeCandidateCardParams,
) -> Result<MaterializedCandidateCard, String> {
    let create_status = if params.start_ready {
        "ready"
    } else {
        "backlog"
    };
    let dedupe_key = params
        .dedupe_key
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin materialize candidate transaction: {error}"))?;

    if let Some(dedupe_key) = dedupe_key.as_deref() {
        lock_candidate_dedupe_key_in_tx(&mut tx, dedupe_key).await?;

        let existing_id: Option<String> = sqlx::query_scalar(
            r#"
            SELECT id
            FROM kanban_cards
            WHERE pipeline_stage_id = $1
              AND metadata->'automation_candidate'->>'dedupe_key' = $2
            ORDER BY
                CASE status
                    WHEN 'ready' THEN 0
                    WHEN 'requested' THEN 1
                    WHEN 'in_progress' THEN 2
                    WHEN 'backlog' THEN 3
                    ELSE 4
                END ASC,
                COALESCE((metadata->'program'->>'current_iteration')::int, -1) DESC,
                updated_at DESC,
                id DESC
            LIMIT 1
            "#,
        )
        .bind(PIPELINE_STAGE_ID)
        .bind(dedupe_key)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|error| format!("find automation candidate by dedupe_key: {error}"))?;

        if let Some(card_id) = existing_id {
            let status = if params.start_ready {
                "ready".to_string()
            } else {
                sqlx::query_scalar::<_, String>(
                    "SELECT status FROM kanban_cards WHERE id = $1 LIMIT 1",
                )
                .bind(&card_id)
                .fetch_one(&mut *tx)
                .await
                .map_err(|error| format!("load existing candidate status: {error}"))?
            };

            let result = sqlx::query(
                r#"
                UPDATE kanban_cards
                   SET title = $1,
                       repo_id = COALESCE($2, repo_id),
                       priority = COALESCE($3, priority),
                       assigned_agent_id = COALESCE($4, assigned_agent_id),
                       description = COALESCE($5, description),
                       metadata = CASE
                           WHEN metadata->'program' ? 'current_iteration'
                           THEN jsonb_set(
                               CAST($6 AS jsonb),
                               '{program,current_iteration}',
                               metadata->'program'->'current_iteration',
                               true
                           )
                           ELSE CAST($6 AS jsonb)
                       END,
                       status = CASE WHEN $7 THEN 'ready' ELSE status END,
                       pipeline_stage_id = $8,
                       updated_at = NOW()
                 WHERE id = $9
                "#,
            )
            .bind(&params.title)
            .bind(params.repo_id.as_deref())
            .bind(params.priority.as_deref())
            .bind(params.assigned_agent_id.as_deref())
            .bind(params.description.as_deref())
            .bind(&params.metadata_json)
            .bind(params.start_ready)
            .bind(PIPELINE_STAGE_ID)
            .bind(&card_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| format!("update automation candidate card: {error}"))?;
            ensure_one_card_row_affected(
                result.rows_affected(),
                "update automation candidate card",
                &card_id,
            )?;

            tx.commit()
                .await
                .map_err(|error| format!("commit update automation candidate card: {error}"))?;

            return Ok(MaterializedCandidateCard {
                card_id,
                created: false,
                status,
            });
        }
    }

    let card_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO kanban_cards (
            id, repo_id, title, status, priority,
            assigned_agent_id, description, metadata,
            pipeline_stage_id, created_at, updated_at
        ) VALUES (
            $1, $2, $3, $4, COALESCE($5, 'medium'),
            $6, $7, CAST($8 AS jsonb),
            $9, NOW(), NOW()
        )
        "#,
    )
    .bind(&card_id)
    .bind(params.repo_id.as_deref())
    .bind(&params.title)
    .bind(create_status)
    .bind(params.priority.as_deref())
    .bind(params.assigned_agent_id.as_deref())
    .bind(params.description.as_deref())
    .bind(&params.metadata_json)
    .bind(PIPELINE_STAGE_ID)
    .execute(&mut *tx)
    .await
    .map_err(|error| format!("insert automation candidate card: {error}"))?;

    tx.commit()
        .await
        .map_err(|error| format!("commit insert automation candidate card: {error}"))?;

    Ok(MaterializedCandidateCard {
        card_id,
        created: true,
        status: create_status.to_string(),
    })
}

async fn lock_candidate_dedupe_key_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    dedupe_key: &str,
) -> Result<(), String> {
    sqlx::query("SELECT pg_advisory_xact_lock(2064, hashtext($1))")
        .bind(dedupe_key)
        .execute(&mut **tx)
        .await
        .map_err(|error| format!("lock automation candidate dedupe_key: {error}"))?;
    Ok(())
}

pub async fn iteration_count_for_card_pg(pool: &PgPool, card_id: &str) -> Result<i64, String> {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM automation_candidate_iterations WHERE card_id = $1",
    )
    .bind(card_id)
    .fetch_one(pool)
    .await
    .map_err(|error| format!("count iterations: {error}"))
}

fn row_to_record(row: &sqlx::postgres::PgRow) -> Result<IterationRecord, String> {
    Ok(IterationRecord {
        id: row
            .try_get("id")
            .map_err(|error| format!("decode id: {error}"))?,
        card_id: row
            .try_get("card_id")
            .map_err(|error| format!("decode card_id: {error}"))?,
        iteration: row
            .try_get("iteration")
            .map_err(|error| format!("decode iteration: {error}"))?,
        branch: row
            .try_get("branch")
            .map_err(|error| format!("decode branch: {error}"))?,
        commit_hash: row
            .try_get("commit_hash")
            .map_err(|error| format!("decode commit_hash: {error}"))?,
        metric_before: row
            .try_get("metric_before")
            .map_err(|error| format!("decode metric_before: {error}"))?,
        metric_after: row
            .try_get("metric_after")
            .map_err(|error| format!("decode metric_after: {error}"))?,
        is_simplification: row
            .try_get("is_simplification")
            .map_err(|error| format!("decode is_simplification: {error}"))?,
        status: row
            .try_get("status")
            .map_err(|error| format!("decode status: {error}"))?,
        description: row
            .try_get("description")
            .map_err(|error| format!("decode description: {error}"))?,
        allowed_write_paths_used: row
            .try_get::<Vec<String>, _>("allowed_write_paths_used")
            .unwrap_or_default(),
        run_seconds: row
            .try_get("run_seconds")
            .map_err(|error| format!("decode run_seconds: {error}"))?,
        crash_trace: row
            .try_get("crash_trace")
            .map_err(|error| format!("decode crash_trace: {error}"))?,
        created_at: row
            .try_get("created_at")
            .map_err(|error| format!("decode created_at: {error}"))?,
    })
}

/// Load the card's metadata JSON and extract the program contract.
pub async fn load_card_program_pg(
    pool: &PgPool,
    card_id: &str,
) -> Result<Option<serde_json::Value>, String> {
    let metadata_raw: Option<String> = sqlx::query_scalar(
        "SELECT metadata::text FROM kanban_cards WHERE id = $1 AND pipeline_stage_id = $2",
    )
    .bind(card_id)
    .bind(PIPELINE_STAGE_ID)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load card metadata: {error}"))?
    .flatten();

    let program = metadata_raw
        .as_deref()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .and_then(|meta| meta.get("program").cloned());

    Ok(program)
}

/// Load the card's status and program contract only when the candidate is still executable.
pub async fn load_active_card_program_pg(
    pool: &PgPool,
    card_id: &str,
) -> Result<Option<(String, serde_json::Value)>, String> {
    let row = sqlx::query(
        r#"
        SELECT status, metadata::text AS metadata
        FROM kanban_cards
        WHERE id = $1
          AND pipeline_stage_id = $2
        "#,
    )
    .bind(card_id)
    .bind(PIPELINE_STAGE_ID)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load active card metadata: {error}"))?;

    let Some(row) = row else { return Ok(None) };
    let status: String = row
        .try_get("status")
        .map_err(|error| format!("decode card status: {error}"))?;
    let metadata_raw: Option<String> = row.try_get("metadata").unwrap_or(None);
    let program = metadata_raw
        .as_deref()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .and_then(|meta| meta.get("program").cloned());

    let Some(program) = program else {
        return Ok(None);
    };
    Ok(Some((status, program)))
}

pub(crate) async fn load_card_program_for_update_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    card_id: &str,
) -> Result<Option<(String, serde_json::Value)>, String> {
    let row = sqlx::query(
        r#"
        SELECT status, metadata::text AS metadata
        FROM kanban_cards
        WHERE id = $1
          AND pipeline_stage_id = $2
        FOR UPDATE
        "#,
    )
    .bind(card_id)
    .bind(PIPELINE_STAGE_ID)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|error| format!("lock active card metadata: {error}"))?;

    let Some(row) = row else { return Ok(None) };
    let status: String = row
        .try_get("status")
        .map_err(|error| format!("decode locked card status: {error}"))?;
    let metadata_raw: Option<String> = row.try_get("metadata").unwrap_or(None);
    let program = metadata_raw
        .as_deref()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .and_then(|meta| meta.get("program").cloned());

    let Some(program) = program else {
        return Ok(None);
    };
    Ok(Some((status, program)))
}

fn is_active_iteration_status(status: &str) -> bool {
    matches!(status, "ready" | "requested" | "in_progress")
}

fn program_current_iteration(program: &serde_json::Value) -> i32 {
    program
        .get("current_iteration")
        .and_then(|v| v.as_i64())
        .filter(|value| *value >= 0)
        .and_then(|value| i32::try_from(value).ok())
        .unwrap_or(0)
}

fn program_iteration_budget(program: &serde_json::Value) -> i32 {
    program
        .get("iteration_budget")
        .and_then(|v| v.as_i64())
        .unwrap_or(MAX_ITERATIONS as i64)
        .clamp(1, MAX_ITERATIONS as i64) as i32
}

/// Transition the card to a new status.
pub async fn transition_card_status_pg(
    pool: &PgPool,
    card_id: &str,
    new_status: &str,
) -> Result<(), String> {
    let result = sqlx::query(
        r#"
        UPDATE kanban_cards
           SET status = $1, updated_at = NOW()
         WHERE id = $2
           AND pipeline_stage_id = $3
        "#,
    )
    .bind(new_status)
    .bind(card_id)
    .bind(PIPELINE_STAGE_ID)
    .execute(pool)
    .await
    .map_err(|error| format!("transition card {card_id} to {new_status}: {error}"))?;
    ensure_one_card_row_affected(result.rows_affected(), "transition card status", card_id)?;
    Ok(())
}

/// Persist the last completed automation iteration on the card's program metadata.
pub async fn update_card_program_current_iteration_pg(
    pool: &PgPool,
    card_id: &str,
    current_iteration: i32,
) -> Result<(), String> {
    let result = sqlx::query(
        r#"
        UPDATE kanban_cards
           SET metadata = jsonb_set(
                   COALESCE(metadata, '{}'::jsonb),
                   '{program,current_iteration}',
                   to_jsonb($1::int),
                   true
               ),
               updated_at = NOW()
         WHERE id = $2
           AND pipeline_stage_id = $3
        "#,
    )
    .bind(current_iteration)
    .bind(card_id)
    .bind(PIPELINE_STAGE_ID)
    .execute(pool)
    .await
    .map_err(|error| {
        format!("update card {card_id} program current_iteration to {current_iteration}: {error}")
    })?;
    ensure_one_card_row_affected(
        result.rows_affected(),
        "update card program current_iteration",
        card_id,
    )?;
    Ok(())
}

/// Create a child card for the next iteration of an automation candidate.
pub async fn create_child_candidate_card_pg(
    pool: &PgPool,
    parent_card_id: &str,
    parent_title: &str,
    iteration: i32,
    parent_metadata: &serde_json::Value,
) -> Result<String, String> {
    let child_id = uuid::Uuid::new_v4().to_string();
    let child_title = format!("{parent_title} [iter {iteration}]");

    let mut child_meta = parent_metadata.clone();
    if let Some(program) = child_meta.get_mut("program") {
        if let Some(obj) = program.as_object_mut() {
            obj.insert(
                "current_iteration".to_string(),
                serde_json::Value::Number((iteration - 1).into()),
            );
        }
    }
    child_meta["parent_iteration"] = serde_json::Value::Number((iteration - 1).into());
    child_meta["iteration"] = serde_json::Value::Number(iteration.into());

    let child_meta_json = serde_json::to_string(&child_meta)
        .map_err(|error| format!("serialize child metadata: {error}"))?;

    sqlx::query(
        r#"
        INSERT INTO kanban_cards (
            id, title, status, priority,
            pipeline_stage_id, parent_card_id,
            metadata, created_at, updated_at
        ) VALUES (
            $1, $2, 'ready', 'medium',
            $3, $4,
            CAST($5 AS jsonb), NOW(), NOW()
        )
        "#,
    )
    .bind(&child_id)
    .bind(&child_title)
    .bind(PIPELINE_STAGE_ID)
    .bind(parent_card_id)
    .bind(&child_meta_json)
    .execute(pool)
    .await
    .map_err(|error| format!("create child card: {error}"))?;

    Ok(child_id)
}

/// Read `metadata->'program'->>'repo_dir'` for the card.
///
/// Returns `None` if the card doesn't exist or the field isn't set.
pub async fn load_card_repo_dir_pg(pool: &PgPool, card_id: &str) -> Result<Option<String>, String> {
    let value: Option<String> = sqlx::query_scalar(
        r#"
        SELECT metadata->'program'->>'repo_dir'
        FROM kanban_cards
        WHERE id = $1
          AND pipeline_stage_id = $2
        "#,
    )
    .bind(card_id)
    .bind(PIPELINE_STAGE_ID)
    .fetch_optional(pool)
    .await
    .map_err(|e| format!("load card repo_dir: {e}"))?
    .flatten();
    Ok(value)
}

/// Read `metadata->'program'->>'final_gate'` for the card.
///
/// Returns `None` if the card doesn't exist or the field isn't set.
/// Expected values: `"manual_review"` (default) | `"auto_apply_after_green"`.
pub async fn load_card_final_gate_pg(
    pool: &PgPool,
    card_id: &str,
) -> Result<Option<String>, String> {
    let value: Option<String> = sqlx::query_scalar(
        r#"
        SELECT metadata->'program'->>'final_gate'
        FROM kanban_cards
        WHERE id = $1
          AND pipeline_stage_id = $2
        "#,
    )
    .bind(card_id)
    .bind(PIPELINE_STAGE_ID)
    .fetch_optional(pool)
    .await
    .map_err(|e| format!("load card final_gate: {e}"))?
    .flatten();
    Ok(value)
}

/// Approve a card for final application (manual_review gate).
pub async fn approve_candidate_card_pg(pool: &PgPool, card_id: &str) -> Result<(), String> {
    let result = sqlx::query(
        r#"UPDATE kanban_cards
           SET review_status = 'approved',
               updated_at    = NOW()
           WHERE id = $1
             AND pipeline_stage_id = $2"#,
    )
    .bind(card_id)
    .bind(PIPELINE_STAGE_ID)
    .execute(pool)
    .await
    .map_err(|error| format!("approve candidate card: {error}"))?;
    ensure_one_card_row_affected(result.rows_affected(), "approve candidate card", card_id)?;
    Ok(())
}
