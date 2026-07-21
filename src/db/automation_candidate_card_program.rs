use sqlx::{PgPool, Postgres, Row, Transaction};

use crate::db::automation_candidates::{MAX_ITERATIONS, ensure_one_card_row_affected};
use crate::services::automation_candidate_contract::PIPELINE_STAGE_ID;

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

pub(super) fn is_active_iteration_status(status: &str) -> bool {
    matches!(status, "ready" | "requested" | "in_progress")
}

pub(super) fn program_current_iteration(program: &serde_json::Value) -> i32 {
    program
        .get("current_iteration")
        .and_then(|v| v.as_i64())
        .filter(|value| *value >= 0)
        .and_then(|value| i32::try_from(value).ok())
        .unwrap_or(0)
}

pub(super) fn program_iteration_budget(program: &serde_json::Value) -> i32 {
    program
        .get("iteration_budget")
        .and_then(|v| v.as_i64())
        .unwrap_or(MAX_ITERATIONS as i64)
        .clamp(1, MAX_ITERATIONS as i64) as i32
}

/// Transition the card to a new status.
#[allow(dead_code)] // staged-rollout automation-candidate repo helper; not on every target. See #3034
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
#[allow(dead_code)] // staged-rollout automation-candidate repo helper; not on every target. See #3034
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
#[allow(dead_code)] // staged-rollout automation-candidate repo helper; not on every target. See #3034
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
#[allow(dead_code)] // staged-rollout automation-candidate repo helper; not on every target. See #3034
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
#[allow(dead_code)] // staged-rollout automation-candidate repo helper; not on every target. See #3034
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
