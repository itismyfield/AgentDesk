use std::collections::HashMap;

mod stage_validation;
use stage_validation::{validate_pipeline_stages, validate_supported_stage_changes};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Row, Transaction};

use crate::utils::api::clamp_api_limit;

/// Accepted `on_failure` values for a `pipeline_stages` row.
pub const STAGE_ON_FAILURE_VALUES: &[&str] =
    &["escalate", "retry-with-backoff", "fallback-stage", "fail"];

/// #1082 -- accepted `backoff` policy values.
pub const STAGE_BACKOFF_VALUES: &[&str] = &["exponential", "linear", "none"];

/// Upsert on `(repo_id, stage_name)` so a kept stage keeps the id cards point at.
/// Column order MUST match the `.bind(...)` chain in `replace_stages`.
const INSERT_STAGE_SQL: &str = "INSERT INTO pipeline_stages (
    repo_id, stage_name, stage_order, trigger_after, entry_skill,
    timeout_minutes, on_failure, skip_condition, provider, agent_override_id,
    on_failure_target, max_retries, parallel_with, backoff
 ) VALUES (
    $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14
 )
 ON CONFLICT (repo_id, stage_name) DO UPDATE SET
    stage_order = EXCLUDED.stage_order,
    trigger_after = EXCLUDED.trigger_after,
    entry_skill = EXCLUDED.entry_skill,
    timeout_minutes = EXCLUDED.timeout_minutes,
    on_failure = EXCLUDED.on_failure,
    skip_condition = EXCLUDED.skip_condition,
    provider = EXCLUDED.provider,
    agent_override_id = EXCLUDED.agent_override_id,
    on_failure_target = EXCLUDED.on_failure_target,
    max_retries = EXCLUDED.max_retries,
    parallel_with = EXCLUDED.parallel_with,
    backoff = EXCLUDED.backoff";

/// `list_pipeline_stages_pg` projection. `backoff` added by #3868 so it
/// round-trips back through the list/GET path. Column order MUST match
/// `pg_stage_row_to_json`.
const SELECT_STAGES_SQL: &str =
    "SELECT id, repo_id, stage_name, stage_order, trigger_after, entry_skill,
        timeout_minutes, on_failure, skip_condition, provider,
        agent_override_id, on_failure_target, max_retries, parallel_with, backoff
 FROM pipeline_stages
 WHERE ($1::text IS NULL OR repo_id = $1)
   AND ($2::text IS NULL OR agent_override_id = $2)
 ORDER BY stage_order ASC";

/// Saves take a repo's stage lock exclusively and card moves take it shared, so
/// a move never reads a stage order that a save is changing.
const STAGE_SAVE_LOCK_SQL: &str =
    "SELECT pg_advisory_xact_lock(hashtext('pipeline_stages:' || $1))";
const STAGE_MOVE_LOCK_SQL: &str =
    "SELECT pg_advisory_xact_lock_shared(hashtext('pipeline_stages:' || $1))";

/// Worker capability path a build advertises once its card moves take the stage lock.
pub(crate) const STAGE_LOCK_CAPABILITY: [&str; 2] = ["pipeline", "stage_lock_v1"];

#[derive(Debug)]
pub enum PipelineRouteError {
    BadRequest { stage: String, error: String },
    NotFound(String),
    Conflict(String),
    Unavailable(String),
    Database(String),
}

/// Metadata fields no executor reads keep their stored value when a save omits them.
#[derive(Debug, Deserialize)]
pub struct PipelineStageInput {
    pub stage_name: String,
    pub stage_order: Option<i64>,
    pub trigger_after: Option<String>,
    pub entry_skill: Option<String>,
    pub provider: Option<String>,
    pub agent_override_id: Option<String>,
    pub timeout_minutes: Option<i64>,
    pub on_failure: Option<String>,
    pub on_failure_target: Option<String>,
    pub max_retries: Option<i64>,
    /// #1082 backoff policy. One of STAGE_BACKOFF_VALUES. Persisted as
    /// declarative stage metadata so it round-trips through GET; no executor
    /// reads these per-stage columns.
    pub backoff: Option<String>,
    pub skip_condition: Option<String>,
    pub parallel_with: Option<String>,
}

/// A repo's stored stage row, read under the save lock.
#[derive(Clone, Default, sqlx::FromRow)]
struct StoredStage {
    id: i64,
    stage_name: Option<String>,
    stage_order: Option<i64>,
    provider: Option<String>,
    skip_condition: Option<String>,
    agent_override_id: Option<String>,
    entry_skill: Option<String>,
    timeout_minutes: Option<i64>,
    on_failure: Option<String>,
    on_failure_target: Option<String>,
    max_retries: Option<i64>,
    parallel_with: Option<String>,
    backoff: Option<String>,
}

pub struct CardPipelineState {
    pub repo_id: Option<String>,
    pub stages: Vec<Value>,
    pub history: Vec<Value>,
    pub current_stage: Value,
}

pub struct PipelineRouteService<'a> {
    pool: &'a PgPool,
}

impl<'a> PipelineRouteService<'a> {
    pub fn new(pool: &'a PgPool) -> Self {
        Self { pool }
    }

    pub async fn list_stages(
        &self,
        repo: Option<&str>,
        agent_id: Option<&str>,
    ) -> Result<Vec<Value>, PipelineRouteError> {
        list_pipeline_stages_pg(self.pool, repo, agent_id).await
    }

    pub async fn replace_stages(
        &self,
        repo: &str,
        stages: &[PipelineStageInput],
    ) -> Result<Vec<Value>, PipelineRouteError> {
        validate_pipeline_stages(stages)?;

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|error| PipelineRouteError::Database(format!("begin tx: {error}")))?;

        let stored = lock_repo_stages(&mut tx, repo).await?;
        validate_supported_stage_changes(stages, &stored)?;
        let orders: HashMap<&str, i64> = stages
            .iter()
            .enumerate()
            .map(|(idx, stage)| {
                let order = stage.stage_order.unwrap_or(idx as i64 + 1);
                (stage.stage_name.as_str(), order)
            })
            .collect();
        ensure_cards_keep_their_path(&mut tx, &stored, &orders).await?;

        let removed: Vec<i64> = stored
            .iter()
            .filter(|row| {
                !row.stage_name
                    .as_deref()
                    .is_some_and(|name| orders.contains_key(name))
            })
            .map(|row| row.id)
            .collect();
        sqlx::query("DELETE FROM pipeline_stages WHERE id = ANY($1)")
            .bind(&removed)
            .execute(&mut *tx)
            .await
            .map_err(|error| PipelineRouteError::Database(format!("delete: {error}")))?;

        for stage in stages {
            let kept = stored
                .iter()
                .find(|row| row.stage_name.as_deref() == Some(stage.stage_name.as_str()))
                .cloned()
                .unwrap_or_default();
            let backoff = match stage.backoff.as_deref() {
                Some(value) => normalize_optional(Some(value)).map(str::to_string),
                None => kept.backoff,
            };

            sqlx::query(INSERT_STAGE_SQL)
                .bind(repo)
                .bind(&stage.stage_name)
                .bind(orders[stage.stage_name.as_str()])
                .bind(stage.trigger_after.as_deref())
                .bind(stage.entry_skill.clone().or(kept.entry_skill))
                .bind(stage.timeout_minutes.or(kept.timeout_minutes).unwrap_or(60))
                .bind(
                    stage
                        .on_failure
                        .clone()
                        .or(kept.on_failure)
                        .unwrap_or_else(|| "fail".to_string()),
                )
                .bind(stage.skip_condition.as_deref())
                .bind(stage.provider.as_deref())
                .bind(stage.agent_override_id.as_deref())
                .bind(stage.on_failure_target.clone().or(kept.on_failure_target))
                .bind(stage.max_retries.or(kept.max_retries).unwrap_or(0))
                .bind(stage.parallel_with.clone().or(kept.parallel_with))
                .bind(backoff)
                .execute(&mut *tx)
                .await
                .map_err(|error| {
                    PipelineRouteError::Database(format!(
                        "insert stage '{}': {error}",
                        stage.stage_name
                    ))
                })?;
        }

        tx.commit()
            .await
            .map_err(|error| PipelineRouteError::Database(format!("commit: {error}")))?;

        self.list_stages(Some(repo), None).await
    }

    pub async fn delete_stages(&self, repo: &str) -> Result<u64, PipelineRouteError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|error| PipelineRouteError::Database(format!("begin tx: {error}")))?;
        let stored = lock_repo_stages(&mut tx, repo).await?;
        ensure_cards_keep_their_path(&mut tx, &stored, &HashMap::new()).await?;
        let result = sqlx::query("DELETE FROM pipeline_stages WHERE repo_id = $1")
            .bind(repo)
            .execute(&mut *tx)
            .await
            .map_err(database_error)?;
        tx.commit()
            .await
            .map_err(|error| PipelineRouteError::Database(format!("commit: {error}")))?;
        Ok(result.rows_affected())
    }

    pub async fn card_pipeline(
        &self,
        card_id: &str,
    ) -> Result<CardPipelineState, PipelineRouteError> {
        let repo_id = sqlx::query_scalar::<_, Option<String>>(
            "SELECT repo_id FROM kanban_cards WHERE id = $1",
        )
        .bind(card_id)
        .fetch_optional(self.pool)
        .await
        .map_err(database_error)?
        .ok_or_else(|| PipelineRouteError::NotFound("card not found".to_string()))?;

        let stages = if let Some(repo_id) = repo_id.as_deref() {
            self.list_stages(Some(repo_id), None).await?
        } else {
            Vec::new()
        };
        let history = self.card_pipeline_history(card_id).await?;
        let current_stage = find_current_stage(&stages, &history);

        Ok(CardPipelineState {
            repo_id,
            stages,
            history,
            current_stage,
        })
    }

    pub async fn card_history(&self, card_id: &str) -> Result<Vec<Value>, PipelineRouteError> {
        let rows = sqlx::query(
            "SELECT id, dispatch_type, status, from_agent_id, to_agent_id, title, result,
                    created_at::text AS created_at, updated_at::text AS updated_at
             FROM task_dispatches
             WHERE kanban_card_id = $1
             ORDER BY created_at ASC",
        )
        .bind(card_id)
        .fetch_all(self.pool)
        .await
        .map_err(|error| PipelineRouteError::Database(format!("prepare: {error}")))?;
        rows.into_iter()
            .map(|row| {
                Ok(dispatch_history_json(
                    row.try_get::<String, _>("id")?,
                    row.try_get::<Option<String>, _>("dispatch_type")?,
                    row.try_get::<Option<String>, _>("status")?,
                    row.try_get::<Option<String>, _>("from_agent_id")?,
                    row.try_get::<Option<String>, _>("to_agent_id")?,
                    row.try_get::<Option<String>, _>("title")?,
                    row.try_get::<Option<String>, _>("result")?,
                    row.try_get::<Option<String>, _>("created_at")?,
                    row.try_get::<Option<String>, _>("updated_at")?,
                ))
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()
            .map_err(|error| PipelineRouteError::Database(format!("decode history row: {error}")))
    }

    pub async fn card_transcripts(
        &self,
        card_id: &str,
        limit: usize,
    ) -> Result<Vec<Value>, PipelineRouteError> {
        self.ensure_card_exists(card_id).await?;
        list_card_transcripts_pg(self.pool, card_id, limit)
            .await
            .map_err(|error| PipelineRouteError::Database(format!("transcripts: {error}")))
    }

    pub async fn effective_pipeline(
        &self,
        repo: Option<&str>,
        agent_id: Option<&str>,
    ) -> Result<Value, PipelineRouteError> {
        if crate::pipeline::try_get().is_none() {
            return Err(PipelineRouteError::NotFound(
                "default pipeline not loaded".to_string(),
            ));
        }

        let effective = crate::pipeline::resolve_for_card_pg(self.pool, repo, agent_id).await;
        let repo_has_override = self.repo_has_override(repo).await?;
        let agent_has_override = self.agent_has_override(agent_id).await?;

        Ok(json!({
            "pipeline": effective.to_json(),
            "layers": {
                "default": true,
                "repo": repo_has_override,
                "agent": agent_has_override,
            },
        }))
    }

    pub async fn pipeline_graph(
        &self,
        repo: Option<&str>,
        agent_id: Option<&str>,
    ) -> Result<Value, PipelineRouteError> {
        if crate::pipeline::try_get().is_none() {
            return Err(PipelineRouteError::NotFound(
                "default pipeline not loaded".to_string(),
            ));
        }

        let effective = crate::pipeline::resolve_for_card_pg(self.pool, repo, agent_id).await;
        Ok(effective.to_graph())
    }

    async fn ensure_card_exists(&self, card_id: &str) -> Result<(), PipelineRouteError> {
        let count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*)::BIGINT AS count FROM kanban_cards WHERE id = $1",
        )
        .bind(card_id)
        .fetch_one(self.pool)
        .await
        .map_err(|error| PipelineRouteError::Database(format!("query: {error}")))?;
        if count > 0 {
            Ok(())
        } else {
            Err(PipelineRouteError::NotFound("card not found".to_string()))
        }
    }

    async fn repo_has_override(&self, repo: Option<&str>) -> Result<bool, PipelineRouteError> {
        let Some(repo_id) = repo else {
            return Ok(false);
        };
        let value = sqlx::query_scalar::<_, bool>(
            "SELECT pipeline_config IS NOT NULL AND TRIM(pipeline_config::text) != ''
             FROM github_repos
             WHERE id = $1",
        )
        .bind(repo_id)
        .fetch_optional(self.pool)
        .await
        .map_err(database_error)?;
        Ok(value.unwrap_or(false))
    }

    async fn agent_has_override(&self, agent_id: Option<&str>) -> Result<bool, PipelineRouteError> {
        let Some(agent_id) = agent_id else {
            return Ok(false);
        };
        let value = sqlx::query_scalar::<_, bool>(
            "SELECT pipeline_config IS NOT NULL AND TRIM(pipeline_config::text) != ''
             FROM agents
             WHERE id = $1",
        )
        .bind(agent_id)
        .fetch_optional(self.pool)
        .await
        .map_err(database_error)?;
        Ok(value.unwrap_or(false))
    }

    async fn card_pipeline_history(&self, card_id: &str) -> Result<Vec<Value>, PipelineRouteError> {
        let rows = sqlx::query(
            "SELECT id, kanban_card_id, from_agent_id, to_agent_id, dispatch_type,
                    status, title, context, result, created_at::text AS created_at, updated_at::text AS updated_at
             FROM task_dispatches
             WHERE kanban_card_id = $1
             ORDER BY created_at ASC",
        )
        .bind(card_id)
        .fetch_all(self.pool)
        .await
        .map_err(|error| PipelineRouteError::Database(format!("history query: {error}")))?;
        rows.into_iter()
            .map(|row| {
                Ok(dispatch_pipeline_history_json(
                    row.try_get::<String, _>("id")?,
                    row.try_get::<Option<String>, _>("kanban_card_id")?,
                    row.try_get::<Option<String>, _>("from_agent_id")?,
                    row.try_get::<Option<String>, _>("to_agent_id")?,
                    row.try_get::<Option<String>, _>("dispatch_type")?,
                    row.try_get::<Option<String>, _>("status")?,
                    row.try_get::<Option<String>, _>("title")?,
                    row.try_get::<Option<String>, _>("context")?,
                    row.try_get::<Option<String>, _>("result")?,
                    row.try_get::<Option<String>, _>("created_at")?,
                    row.try_get::<Option<String>, _>("updated_at")?,
                ))
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()
            .map_err(|error| PipelineRouteError::Database(format!("decode history row: {error}")))
    }
}

/// Validate a stage's `on_failure` string. Returns `Err(value)` with the
/// offending value when unknown, `Ok(())` otherwise (including None/empty).
pub fn validate_on_failure(value: Option<&str>) -> Result<(), String> {
    match value {
        None => Ok(()),
        Some(v) if v.is_empty() => Ok(()),
        Some(v) if STAGE_ON_FAILURE_VALUES.iter().any(|a| *a == v) => Ok(()),
        Some(v) => Err(format!(
            "on_failure='{}' is invalid; expected one of {:?}",
            v, STAGE_ON_FAILURE_VALUES
        )),
    }
}

/// Map `None` or an empty/whitespace-only string to `None` so it persists as
/// SQL NULL (rather than an empty string). Used for the optional `backoff`
/// column (#3868) so an omitted/blank policy round-trips back as `null`.
fn normalize_optional(value: Option<&str>) -> Option<&str> {
    value.filter(|v| !v.trim().is_empty())
}

/// Validate a stage's `backoff` string.
pub fn validate_backoff(value: Option<&str>) -> Result<(), String> {
    match value {
        None => Ok(()),
        Some(v) if v.is_empty() => Ok(()),
        Some(v) if STAGE_BACKOFF_VALUES.iter().any(|a| *a == v) => Ok(()),
        Some(v) => Err(format!(
            "backoff='{}' is invalid; expected one of {:?}",
            v, STAGE_BACKOFF_VALUES
        )),
    }
}

/// Takes the repo's save lock, then reads its stages in a fresh snapshot.
async fn lock_repo_stages(
    tx: &mut Transaction<'_, Postgres>,
    repo: &str,
) -> Result<Vec<StoredStage>, PipelineRouteError> {
    sqlx::query(STAGE_SAVE_LOCK_SQL)
        .bind(repo)
        .execute(&mut **tx)
        .await
        .map_err(|error| PipelineRouteError::Database(format!("lock stages: {error}")))?;
    ensure_every_node_locks_stage_moves(tx).await?;
    sqlx::query_as::<_, StoredStage>(
        "SELECT id, stage_name, stage_order, provider, skip_condition, agent_override_id,
                entry_skill, timeout_minutes, on_failure,
                on_failure_target, max_retries, parallel_with, backoff
           FROM pipeline_stages
          WHERE repo_id = $1",
    )
    .bind(repo)
    .fetch_all(&mut **tx)
    .await
    .map_err(|error| PipelineRouteError::Database(format!("load stages: {error}")))
}

/// Older builds move cards without the stage lock, so saves wait until no online node runs one.
async fn ensure_every_node_locks_stage_moves(
    tx: &mut Transaction<'_, Postgres>,
) -> Result<(), PipelineRouteError> {
    let ready = sqlx::query_scalar::<_, bool>(
        "SELECT NOT EXISTS (
             SELECT 1 FROM worker_nodes
              WHERE status = 'online'
                AND COALESCE(capabilities #>> $1::text[], 'false') <> 'true'
         )",
    )
    .bind(&STAGE_LOCK_CAPABILITY[..])
    .fetch_one(&mut **tx)
    .await
    .map_err(|error| PipelineRouteError::Database(format!("check node builds: {error}")))?;
    if ready {
        Ok(())
    } else {
        Err(PipelineRouteError::Unavailable(
            "stage edits wait until every online node runs a build that locks card stage moves"
                .to_string(),
        ))
    }
}

/// A save may not remove a stage an open card is in, or move a kept stage to its
/// other side: either would skip or repeat work for that card.
async fn ensure_cards_keep_their_path(
    tx: &mut Transaction<'_, Postgres>,
    stored: &[StoredStage],
    orders: &HashMap<&str, i64>,
) -> Result<(), PipelineRouteError> {
    let ids: Vec<String> = stored.iter().map(|row| row.id.to_string()).collect();
    let occupied = sqlx::query_as::<_, (String, i64)>(
        "SELECT pipeline_stage_id, COUNT(*)
           FROM kanban_cards
          WHERE pipeline_stage_id = ANY($1)
            AND COALESCE(status, '') NOT IN ('done', 'cancelled')
          GROUP BY pipeline_stage_id",
    )
    .bind(&ids)
    .fetch_all(&mut **tx)
    .await
    .map_err(|error| PipelineRouteError::Database(format!("load stage cards: {error}")))?;

    for (stage_id, cards) in occupied {
        let Some(stage) = stored.iter().find(|row| row.id.to_string() == stage_id) else {
            continue;
        };
        let name = stage.stage_name.as_deref().unwrap_or_default();
        let Some(&order) = orders.get(name) else {
            return Err(PipelineRouteError::Conflict(format!(
                "stage '{name}' has {cards} open card(s) in it; finish or move them before removing the stage"
            )));
        };
        for other in stored {
            let Some(other_name) = other.stage_name.as_deref() else {
                continue;
            };
            let Some(&other_order) = orders.get(other_name) else {
                continue;
            };
            if other.stage_order.cmp(&stage.stage_order) != other_order.cmp(&order) {
                return Err(PipelineRouteError::Conflict(format!(
                    "stage '{name}' has {cards} open card(s) in it; moving '{other_name}' to its other side would skip or repeat work for them"
                )));
            }
        }
    }
    Ok(())
}

/// Where a card's stage move starts from.
#[derive(Clone, Copy)]
pub enum StageStep<'a> {
    /// Put the card in the first stage this trigger starts.
    Enter(&'a str),
    /// Move the card past its current stage, or enter at this trigger when it has none.
    Advance(&'a str),
}

#[derive(Serialize, sqlx::FromRow)]
struct MovedStage {
    id: i64,
    stage_name: Option<String>,
    agent_override_id: Option<String>,
    provider: Option<String>,
    skip_condition: Option<String>,
}

/// Moves a card's `pipeline_stage_id` under the repo's shared stage lock. Returns
/// `{status, stage}` with status entered, advanced, completed, missing or unchanged.
pub async fn move_card_stage(
    pool: &PgPool,
    card_id: &str,
    step: StageStep<'_>,
) -> Result<Value, String> {
    let without_move = |status: &str| json!({ "status": status, "stage": null });
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin stage move for {card_id}: {error}"))?;
    let repo_id =
        sqlx::query_scalar::<_, Option<String>>("SELECT repo_id FROM kanban_cards WHERE id = $1")
            .bind(card_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|error| format!("load card {card_id}: {error}"))?
            .flatten();
    let Some(repo_id) = repo_id else {
        return Ok(without_move("unchanged"));
    };
    sqlx::query(STAGE_MOVE_LOCK_SQL)
        .bind(&repo_id)
        .execute(&mut *tx)
        .await
        .map_err(|error| format!("lock stages of {repo_id}: {error}"))?;
    let current = sqlx::query_scalar::<_, Option<String>>(
        "SELECT pipeline_stage_id FROM kanban_cards WHERE id = $1 FOR UPDATE",
    )
    .bind(card_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(|error| format!("lock card {card_id}: {error}"))?;

    let (status, stage) = match (step, current) {
        (StageStep::Advance(_), Some(current)) => {
            let Some(order) = sqlx::query_scalar::<_, Option<i64>>(
                "SELECT stage_order FROM pipeline_stages WHERE id::text = $1",
            )
            .bind(&current)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|error| format!("load stage {current}: {error}"))?
            else {
                return Ok(without_move("missing"));
            };
            let next = sqlx::query_as::<_, MovedStage>(
                "SELECT id, stage_name, agent_override_id, provider, skip_condition
                   FROM pipeline_stages
                  WHERE repo_id = $1 AND stage_order > $2
                  ORDER BY stage_order ASC
                  LIMIT 1",
            )
            .bind(&repo_id)
            .bind(order)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|error| format!("load stage after {current}: {error}"))?;
            match next {
                Some(stage) => ("advanced", Some(stage)),
                None => ("completed", None),
            }
        }
        (StageStep::Enter(trigger_after), _) | (StageStep::Advance(trigger_after), None) => {
            let first = sqlx::query_as::<_, MovedStage>(
                "SELECT id, stage_name, agent_override_id, provider, skip_condition
                   FROM pipeline_stages
                  WHERE repo_id = $1 AND trigger_after = $2
                  ORDER BY stage_order ASC
                  LIMIT 1",
            )
            .bind(&repo_id)
            .bind(trigger_after)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|error| format!("load first {trigger_after} stage: {error}"))?;
            match first {
                Some(stage) => ("entered", Some(stage)),
                None => return Ok(without_move("unchanged")),
            }
        }
    };

    sqlx::query("UPDATE kanban_cards SET pipeline_stage_id = $2, updated_at = NOW() WHERE id = $1")
        .bind(card_id)
        .bind(stage.as_ref().map(|stage| stage.id.to_string()))
        .execute(&mut *tx)
        .await
        .map_err(|error| format!("move card {card_id}: {error}"))?;
    tx.commit()
        .await
        .map_err(|error| format!("commit stage move for {card_id}: {error}"))?;
    Ok(json!({ "status": status, "stage": stage }))
}

#[allow(clippy::too_many_arguments)]
fn stage_json(
    id: i64,
    repo_id: Option<String>,
    stage_name: Option<String>,
    stage_order: i64,
    trigger_after: Option<String>,
    entry_skill: Option<String>,
    timeout_minutes: i64,
    on_failure: Option<String>,
    skip_condition: Option<String>,
    provider: Option<String>,
    agent_override_id: Option<String>,
    on_failure_target: Option<String>,
    max_retries: Option<i64>,
    parallel_with: Option<String>,
    backoff: Option<String>,
) -> Value {
    json!({
        "id": id,
        "repo_id": repo_id,
        "repo": repo_id,
        "stage_name": stage_name,
        "stage_order": stage_order,
        "trigger_after": trigger_after,
        "entry_skill": entry_skill,
        "timeout_minutes": timeout_minutes,
        "on_failure": on_failure,
        "skip_condition": skip_condition,
        "provider": provider,
        "agent_override_id": agent_override_id,
        "on_failure_target": on_failure_target,
        "max_retries": max_retries,
        "parallel_with": parallel_with,
        "backoff": backoff,
    })
}

fn pg_stage_row_to_json(row: &sqlx::postgres::PgRow) -> Result<Value, sqlx::Error> {
    let stage_order = row.try_get::<i64, _>("stage_order")?;
    let timeout_minutes = row.try_get::<i64, _>("timeout_minutes")?;
    let max_retries = row.try_get::<Option<i64>, _>("max_retries")?;

    Ok(stage_json(
        row.try_get::<i64, _>("id")?,
        row.try_get::<Option<String>, _>("repo_id")?,
        row.try_get::<Option<String>, _>("stage_name")?,
        stage_order,
        row.try_get::<Option<String>, _>("trigger_after")?,
        row.try_get::<Option<String>, _>("entry_skill")?,
        timeout_minutes,
        row.try_get::<Option<String>, _>("on_failure")?,
        row.try_get::<Option<String>, _>("skip_condition")?,
        row.try_get::<Option<String>, _>("provider")?,
        row.try_get::<Option<String>, _>("agent_override_id")?,
        row.try_get::<Option<String>, _>("on_failure_target")?,
        max_retries,
        row.try_get::<Option<String>, _>("parallel_with")?,
        row.try_get::<Option<String>, _>("backoff")?,
    ))
}

async fn list_pipeline_stages_pg(
    pool: &PgPool,
    repo: Option<&str>,
    agent_id: Option<&str>,
) -> Result<Vec<Value>, PipelineRouteError> {
    let rows = sqlx::query(SELECT_STAGES_SQL)
        .bind(repo)
        .bind(agent_id)
        .fetch_all(pool)
        .await
        .map_err(|error| PipelineRouteError::Database(format!("query postgres stages: {error}")))?;

    rows.into_iter()
        .map(|row| {
            pg_stage_row_to_json(&row).map_err(|error| {
                PipelineRouteError::Database(format!("decode postgres stage: {error}"))
            })
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn dispatch_pipeline_history_json(
    id: String,
    kanban_card_id: Option<String>,
    from_agent_id: Option<String>,
    to_agent_id: Option<String>,
    dispatch_type: Option<String>,
    status: Option<String>,
    title: Option<String>,
    context: Option<String>,
    result: Option<String>,
    created_at: Option<String>,
    updated_at: Option<String>,
) -> Value {
    json!({
        "id": id,
        "kanban_card_id": kanban_card_id,
        "from_agent_id": from_agent_id,
        "to_agent_id": to_agent_id,
        "dispatch_type": dispatch_type,
        "status": status,
        "title": title,
        "context": context,
        "result": result,
        "created_at": created_at,
        "updated_at": updated_at,
    })
}

#[allow(clippy::too_many_arguments)]
fn dispatch_history_json(
    id: String,
    dispatch_type: Option<String>,
    status: Option<String>,
    from_agent_id: Option<String>,
    to_agent_id: Option<String>,
    title: Option<String>,
    result: Option<String>,
    created_at: Option<String>,
    updated_at: Option<String>,
) -> Value {
    json!({
        "id": id,
        "dispatch_type": dispatch_type,
        "status": status,
        "from_agent_id": from_agent_id,
        "to_agent_id": to_agent_id,
        "title": title,
        "result": result,
        "created_at": created_at,
        "updated_at": updated_at,
    })
}

async fn list_card_transcripts_pg(
    pool: &PgPool,
    card_id: &str,
    limit: usize,
) -> Result<Vec<Value>, String> {
    let limit = clamp_api_limit(Some(limit)) as i64;
    let rows = sqlx::query(
        "SELECT st.id::BIGINT AS id,
                st.turn_id,
                st.session_key,
                st.channel_id,
                st.agent_id,
                st.provider,
                st.dispatch_id,
                td.kanban_card_id,
                td.title,
                kc.title AS card_title,
                kc.github_issue_number::BIGINT AS github_issue_number,
                st.user_message,
                st.assistant_message,
                st.events_json::TEXT AS events_json,
                st.duration_ms::BIGINT AS duration_ms,
                to_char(st.created_at, 'YYYY-MM-DD HH24:MI:SS') AS created_at
         FROM session_transcripts st
         JOIN task_dispatches td
           ON td.id = st.dispatch_id
         LEFT JOIN kanban_cards kc
           ON kc.id = td.kanban_card_id
         WHERE td.kanban_card_id = $1
         ORDER BY st.created_at DESC, st.id DESC
         LIMIT $2",
    )
    .bind(card_id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("query card transcripts failed: {error}"))?;

    rows.into_iter()
        .map(|row| {
            let events_json = row.try_get::<Option<String>, _>("events_json")?;
            let events = events_json
                .as_deref()
                .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                .and_then(|value| value.as_array().cloned())
                .unwrap_or_default();
            Ok(json!({
                "id": row.try_get::<i64, _>("id")?,
                "turn_id": row.try_get::<String, _>("turn_id")?,
                "session_key": row.try_get::<Option<String>, _>("session_key")?,
                "channel_id": row.try_get::<Option<String>, _>("channel_id")?,
                "agent_id": row.try_get::<Option<String>, _>("agent_id")?,
                "provider": row.try_get::<Option<String>, _>("provider")?,
                "dispatch_id": row.try_get::<Option<String>, _>("dispatch_id")?,
                "kanban_card_id": row.try_get::<Option<String>, _>("kanban_card_id")?,
                "dispatch_title": row.try_get::<Option<String>, _>("title")?,
                "card_title": row.try_get::<Option<String>, _>("card_title")?,
                "github_issue_number": row.try_get::<Option<i64>, _>("github_issue_number")?,
                "user_message": row.try_get::<String, _>("user_message")?,
                "assistant_message": row.try_get::<String, _>("assistant_message")?,
                "events": events,
                "duration_ms": row.try_get::<Option<i64>, _>("duration_ms")?,
                "created_at": row.try_get::<String, _>("created_at")?,
            }))
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()
        .map_err(|error| format!("decode transcript row: {error}"))
}

fn find_current_stage(stages: &[Value], history: &[Value]) -> Value {
    if history.is_empty() || stages.is_empty() {
        return Value::Null;
    }

    let active_dispatch = history.iter().rev().find(|dispatch| {
        let status = dispatch["status"].as_str().unwrap_or("");
        status == "pending" || status == "running" || status == "in_progress"
    });

    let Some(dispatch) = active_dispatch else {
        return Value::Null;
    };

    let dispatch_type = dispatch["dispatch_type"].as_str().unwrap_or("");
    let title = dispatch["title"].as_str().unwrap_or("");
    stages
        .iter()
        .find(|stage| {
            let skill = stage["entry_skill"].as_str().unwrap_or("");
            let name = stage["stage_name"].as_str().unwrap_or("");
            (!skill.is_empty() && (skill == dispatch_type || skill == title))
                || (!name.is_empty() && (name == dispatch_type || name == title))
        })
        .cloned()
        .unwrap_or(Value::Null)
}

fn database_error(error: sqlx::Error) -> PipelineRouteError {
    PipelineRouteError::Database(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stage_with_backoff(backoff: Option<&str>) -> PipelineStageInput {
        PipelineStageInput {
            stage_name: "build".to_string(),
            stage_order: Some(1),
            trigger_after: None,
            entry_skill: None,
            provider: None,
            agent_override_id: None,
            timeout_minutes: Some(60),
            on_failure: Some("retry-with-backoff".to_string()),
            on_failure_target: None,
            max_retries: Some(2),
            backoff: backoff.map(str::to_string),
            skip_condition: None,
            parallel_with: None,
        }
    }

    /// The persistence SQL must carry the `backoff` column on BOTH the write and
    /// read paths. This is the direct regression guard for the #3868 silent drop
    /// (INSERT used to omit the column / SELECT used to never read it). The bind
    /// count must also reach `$14` so the value is actually written.
    #[test]
    fn persistence_sql_includes_backoff_column() {
        assert!(
            INSERT_STAGE_SQL.contains("backoff"),
            "INSERT must persist backoff: {INSERT_STAGE_SQL}"
        );
        assert!(
            INSERT_STAGE_SQL.contains("$14"),
            "INSERT must bind backoff as $14: {INSERT_STAGE_SQL}"
        );
        assert!(
            SELECT_STAGES_SQL.contains("backoff"),
            "SELECT must read backoff back: {SELECT_STAGES_SQL}"
        );
    }

    /// Serialization unit guard: `stage_json` emits the `backoff` it is given
    /// (this is the JSON the DB row feeds through `pg_stage_row_to_json`). The
    /// end-to-end DB round-trip is covered by
    /// `replace_stages_persists_backoff_round_trip_pg`.
    #[test]
    fn stage_json_emits_backoff_field() {
        let value = stage_json(
            1,
            Some("repo".to_string()),
            Some("build".to_string()),
            1,
            None,
            None,
            60,
            Some("retry-with-backoff".to_string()),
            None,
            None,
            None,
            None,
            Some(2),
            None,
            Some("exponential".to_string()),
        );
        assert_eq!(value["backoff"], json!("exponential"));
    }

    /// Absent backoff serializes as JSON null (no spurious default).
    #[test]
    fn stage_json_absent_backoff_is_null() {
        let value = stage_json(
            1, None, None, 1, None, None, 60, None, None, None, None, None, None, None, None,
        );
        assert_eq!(value["backoff"], Value::Null);
    }

    /// A valid backoff passes validation; an unknown value is rejected as
    /// BadRequest (the API contract stays intact after persistence wiring).
    #[test]
    fn invalid_backoff_is_bad_request() {
        validate_pipeline_stages(&[stage_with_backoff(Some("exponential"))])
            .expect("known backoff value should validate");
        // Blank/whitespace-only is treated like absent (normalized to NULL),
        // NOT a BadRequest — consistent with the empty-string and None cases.
        validate_pipeline_stages(&[stage_with_backoff(Some(""))])
            .expect("empty backoff should validate (normalizes to NULL)");
        validate_pipeline_stages(&[stage_with_backoff(Some("   "))])
            .expect("whitespace-only backoff should validate (normalizes to NULL)");

        let err = validate_pipeline_stages(&[stage_with_backoff(Some("bogus"))])
            .expect_err("unknown backoff value must be rejected");
        match err {
            PipelineRouteError::BadRequest { stage, error } => {
                assert_eq!(stage, "build");
                assert!(
                    error.contains("backoff"),
                    "error should name backoff: {error}"
                );
            }
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    /// Empty/whitespace backoff normalizes to NULL so it round-trips as `null`
    /// rather than an empty string; a real value passes through unchanged.
    #[test]
    fn normalize_optional_blanks_to_none() {
        assert_eq!(normalize_optional(None), None);
        assert_eq!(normalize_optional(Some("")), None);
        assert_eq!(normalize_optional(Some("   ")), None);
        assert_eq!(normalize_optional(Some("exponential")), Some("exponential"));
    }

    /// End-to-end DB round-trip against a real Postgres: write stages via
    /// `replace_stages` and read them back via `list_stages`, proving the
    /// `backoff` value actually survives the persistence layer. This is the
    /// regression test for the #3868 silent drop — it would have FAILED before
    /// the INSERT/SELECT/column wiring. Skips cleanly when no local Postgres is
    /// reachable. Also covers absent->null, whitespace-only->NULL, and
    /// invalid->BadRequest through the same write path.
    #[tokio::test]
    async fn replace_stages_persists_backoff_round_trip_pg() {
        let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
            "agentdesk_pipeline_backoff",
            "pipeline stage backoff persistence",
        )
        .await
        else {
            return; // no local Postgres available — skip.
        };
        let pool = pg_db.connect_and_migrate().await;

        let service = PipelineRouteService::new(&pool);

        // (1) A real backoff written via replace_stages reads back identically
        // through list_stages — the direct #3868 silent-drop guard.
        let written = service
            .replace_stages("repo-rt", &[stage_with_backoff(Some("exponential"))])
            .await
            .expect("replace_stages with backoff should succeed");
        assert_eq!(written[0]["backoff"], json!("exponential"));
        let listed = service
            .list_stages(Some("repo-rt"), None)
            .await
            .expect("list_stages should succeed");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0]["backoff"], json!("exponential"));

        // (2) A save that leaves backoff out keeps the stored value.
        let listed = service
            .replace_stages("repo-rt", &[stage_with_backoff(None)])
            .await
            .expect("replace_stages without backoff should succeed");
        assert_eq!(listed[0]["backoff"], json!("exponential"));
        assert_eq!(listed[0]["id"], written[0]["id"]);

        // (3) Whitespace-only backoff clears it to NULL, neither a BadRequest
        // nor a stored "   ".
        let listed = service
            .replace_stages("repo-rt", &[stage_with_backoff(Some("   "))])
            .await
            .expect("whitespace-only backoff should normalize to NULL, not error");
        assert_eq!(listed[0]["backoff"], Value::Null);

        // (4) Invalid backoff is rejected before any write; the prior good row
        // (NULL from case 3) stays intact (validation precedes the tx).
        let err = service
            .replace_stages("repo-rt", &[stage_with_backoff(Some("bogus"))])
            .await
            .expect_err("invalid backoff must be rejected");
        assert!(matches!(err, PipelineRouteError::BadRequest { .. }));
        let listed = service
            .list_stages(Some("repo-rt"), None)
            .await
            .expect("list_stages should succeed");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0]["backoff"], Value::Null);

        pg_db.drop().await;
    }

    fn dashboard_stage(name: &str) -> PipelineStageInput {
        PipelineStageInput {
            stage_name: name.to_string(),
            stage_order: None,
            trigger_after: Some("review_pass".to_string()),
            entry_skill: None,
            provider: None,
            agent_override_id: None,
            timeout_minutes: None,
            on_failure: None,
            on_failure_target: None,
            max_retries: None,
            backoff: None,
            skip_condition: None,
            parallel_with: None,
        }
    }

    /// A card in a stage keeps pointing at that stage's id; a save must not
    /// strand it or reroute it past stages it has not run.
    #[tokio::test]
    async fn stage_saves_refuse_to_strand_open_cards_pg() {
        let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
            "agentdesk_pipeline_stage_cards",
            "pipeline stage card guard",
        )
        .await
        else {
            return;
        };
        let pool = pg_db.connect_and_migrate().await;
        let service = PipelineRouteService::new(&pool);
        let names = |names: &[&str]| {
            names
                .iter()
                .copied()
                .map(dashboard_stage)
                .collect::<Vec<_>>()
        };

        let stages = service
            .replace_stages("repo-rt", &names(&["lint", "e2e", "qa"]))
            .await
            .expect("seed stages");
        let e2e_id = stages[1]["id"].as_i64().expect("e2e id");
        sqlx::query(
            "INSERT INTO kanban_cards (id, repo_id, title, status, pipeline_stage_id)
             VALUES ('card-e2e', 'repo-rt', 'in e2e', 'review', $1)",
        )
        .bind(e2e_id.to_string())
        .execute(&pool)
        .await
        .expect("seed card");

        for (label, attempt) in [
            ("remove e2e", names(&["lint", "qa"])),
            ("rename e2e", names(&["lint", "e2e-v2", "qa"])),
            ("move qa before e2e", names(&["lint", "qa", "e2e"])),
            ("move lint after e2e", names(&["e2e", "lint", "qa"])),
        ] {
            let err = service
                .replace_stages("repo-rt", &attempt)
                .await
                .expect_err(label);
            assert!(
                matches!(err, PipelineRouteError::Conflict(_)),
                "{label}: {err:?}"
            );
        }
        assert!(matches!(
            service.delete_stages("repo-rt").await,
            Err(PipelineRouteError::Conflict(_))
        ));

        let saved = service
            .replace_stages("repo-rt", &names(&["lint", "e2e", "smoke", "qa"]))
            .await
            .expect("adding a stage leaves the card's path intact");
        assert_eq!(saved[1]["id"], json!(e2e_id));

        sqlx::query("UPDATE kanban_cards SET status = 'done' WHERE id = 'card-e2e'")
            .execute(&pool)
            .await
            .expect("close card");
        service
            .replace_stages("repo-rt", &names(&["qa"]))
            .await
            .expect("closed cards do not hold stages");
        assert_eq!(service.delete_stages("repo-rt").await.expect("delete"), 1);

        pg_db.drop().await;
    }

    /// Holds a repo's save lock, as a save does while it runs.
    async fn hold_stage_lock(pool: &PgPool, repo: &str) -> Transaction<'static, Postgres> {
        let mut tx = pool.begin().await.expect("begin");
        sqlx::query(STAGE_SAVE_LOCK_SQL)
            .bind(repo)
            .execute(&mut *tx)
            .await
            .expect("hold stage lock");
        tx
    }

    async fn wait_for_lock_waiters(pool: &PgPool, waiters: i64) {
        for _ in 0..400 {
            let queued = sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM pg_locks
                  WHERE locktype = 'advisory' AND NOT granted
                    AND database = (SELECT oid FROM pg_database WHERE datname = current_database())",
            )
            .fetch_one(pool)
            .await
            .expect("count lock waiters");
            if queued >= waiters {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("{waiters} stage lock waiter(s) never queued");
    }

    fn spawn_save(
        pool: &PgPool,
        repo: &'static str,
        stages: Vec<PipelineStageInput>,
    ) -> tokio::task::JoinHandle<Result<Vec<Value>, PipelineRouteError>> {
        let pool = pool.clone();
        tokio::spawn(async move {
            PipelineRouteService::new(&pool)
                .replace_stages(repo, &stages)
                .await
        })
    }

    fn spawn_advance(pool: &PgPool) -> tokio::task::JoinHandle<Result<Value, String>> {
        let pool = pool.clone();
        tokio::spawn(async move {
            move_card_stage(&pool, "card-walk", StageStep::Advance("review_pass")).await
        })
    }

    async fn card_stage(pool: &PgPool) -> Option<String> {
        sqlx::query_scalar("SELECT pipeline_stage_id FROM kanban_cards WHERE id = 'card-walk'")
            .fetch_one(pool)
            .await
            .expect("card stage")
    }

    /// A card move and a stage save on one repo run one after the other, so the
    /// move never follows an order the save is changing.
    #[tokio::test]
    async fn card_stage_moves_take_turns_with_stage_saves_pg() {
        let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
            "agentdesk_pipeline_stage_moves",
            "pipeline stage moves",
        )
        .await
        else {
            return;
        };
        let pool = pg_db.connect_and_migrate_with_max_connections(4).await;
        let service = PipelineRouteService::new(&pool);
        let names = |names: &[&str]| {
            names
                .iter()
                .copied()
                .map(dashboard_stage)
                .collect::<Vec<_>>()
        };
        let seeded = service
            .replace_stages("repo-rt", &names(&["lint", "e2e", "qa"]))
            .await
            .expect("seed stages");
        let [lint, e2e, qa] =
            [0, 1, 2].map(|idx| seeded[idx]["id"].as_i64().expect("stage id").to_string());
        sqlx::query(
            "INSERT INTO kanban_cards (id, repo_id, title, status)
             VALUES ('card-walk', 'repo-rt', 'walks the stages', 'review')",
        )
        .execute(&pool)
        .await
        .expect("seed card");

        let moved = spawn_advance(&pool).await.expect("join").expect("enter");
        assert_eq!(moved["status"], json!("entered"));
        assert_eq!(card_stage(&pool).await, Some(lint.clone()));

        // The save queued first reorders qa ahead of e2e; the move then takes qa.
        let hold = hold_stage_lock(&pool, "repo-rt").await;
        let save = spawn_save(&pool, "repo-rt", names(&["lint", "qa", "e2e"]));
        wait_for_lock_waiters(&pool, 1).await;
        let advance = spawn_advance(&pool);
        wait_for_lock_waiters(&pool, 2).await;
        hold.commit().await.expect("release");
        save.await.expect("join").expect("reorder behind the card");
        let moved = advance.await.expect("join").expect("advance");
        assert_eq!(moved["stage"]["stage_name"], json!("qa"));
        assert_eq!(card_stage(&pool).await, Some(qa.clone()));

        // The move queued first takes e2e; the save then sees the card there.
        sqlx::query("UPDATE kanban_cards SET pipeline_stage_id = $1 WHERE id = 'card-walk'")
            .bind(&lint)
            .execute(&pool)
            .await
            .expect("back to lint");
        service
            .replace_stages("repo-rt", &names(&["lint", "e2e", "qa"]))
            .await
            .expect("restore order");
        let hold = hold_stage_lock(&pool, "repo-rt").await;
        let advance = spawn_advance(&pool);
        wait_for_lock_waiters(&pool, 1).await;
        let save = spawn_save(&pool, "repo-rt", names(&["lint", "qa", "e2e"]));
        wait_for_lock_waiters(&pool, 2).await;
        hold.commit().await.expect("release");
        let moved = advance.await.expect("join").expect("advance");
        assert_eq!(moved["stage"]["stage_name"], json!("e2e"));
        assert!(matches!(
            save.await.expect("join"),
            Err(PipelineRouteError::Conflict(_))
        ));
        assert_eq!(card_stage(&pool).await, Some(e2e));

        spawn_advance(&pool).await.expect("join").expect("to qa");
        let moved = spawn_advance(&pool).await.expect("join").expect("past qa");
        assert_eq!(moved["status"], json!("completed"));
        assert_eq!(card_stage(&pool).await, None);

        sqlx::query("UPDATE kanban_cards SET pipeline_stage_id = '999999' WHERE id = 'card-walk'")
            .execute(&pool)
            .await
            .expect("point at a gone stage");
        let moved = spawn_advance(&pool)
            .await
            .expect("join")
            .expect("gone stage");
        assert_eq!(moved["status"], json!("missing"));
        assert_eq!(card_stage(&pool).await, Some("999999".to_string()));

        pg_db.drop().await;
    }

    /// Saves on one repo run one after the other, including the first saves on
    /// an empty repo, so each reads what the one before it wrote.
    #[tokio::test]
    async fn stage_saves_take_turns_pg() {
        let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
            "agentdesk_pipeline_stage_saves",
            "pipeline stage saves",
        )
        .await
        else {
            return;
        };
        let pool = pg_db.connect_and_migrate_with_max_connections(4).await;
        let service = PipelineRouteService::new(&pool);

        let hold = hold_stage_lock(&pool, "repo-empty").await;
        let first = spawn_save(&pool, "repo-empty", vec![dashboard_stage("e2e")]);
        wait_for_lock_waiters(&pool, 1).await;
        let second = spawn_save(&pool, "repo-empty", vec![dashboard_stage("qa")]);
        wait_for_lock_waiters(&pool, 2).await;
        hold.commit().await.expect("release");
        first.await.expect("join").expect("first save");
        let listed = second.await.expect("join").expect("second save");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0]["stage_name"], json!("qa"));

        let hold = hold_stage_lock(&pool, "repo-timeout").await;
        let first = spawn_save(
            &pool,
            "repo-timeout",
            vec![PipelineStageInput {
                timeout_minutes: Some(120),
                ..dashboard_stage("qa")
            }],
        );
        wait_for_lock_waiters(&pool, 1).await;
        let second = spawn_save(&pool, "repo-timeout", vec![dashboard_stage("qa")]);
        wait_for_lock_waiters(&pool, 2).await;
        hold.commit().await.expect("release");
        first.await.expect("join").expect("first save");
        let listed = second.await.expect("join").expect("second save");
        assert_eq!(listed[0]["timeout_minutes"], json!(120));

        // A node on a build that moves cards without the lock holds saves back.
        sqlx::query(
            "INSERT INTO worker_nodes (instance_id, status, capabilities, last_heartbeat_at)
             VALUES ('old-build', 'online', '{}'::jsonb, NOW())",
        )
        .execute(&pool)
        .await
        .expect("seed node");
        assert!(matches!(
            service
                .replace_stages("repo-timeout", &[dashboard_stage("qa")])
                .await,
            Err(PipelineRouteError::Unavailable(_))
        ));
        sqlx::query(
            "UPDATE worker_nodes
                SET capabilities = '{\"pipeline\": {\"stage_lock_v1\": true}}'::jsonb
              WHERE instance_id = 'old-build'",
        )
        .execute(&pool)
        .await
        .expect("upgrade node");
        service
            .replace_stages("repo-timeout", &[dashboard_stage("qa")])
            .await
            .expect("every node takes the lock");

        pg_db.drop().await;
    }
}
