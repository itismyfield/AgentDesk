use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{PgPool, Row};

use crate::utils::api::clamp_api_limit;

/// The only `skip_condition` the runtime evaluates (policies/review-automation.js).
pub const STAGE_SKIP_CONDITIONS: &[&str] = &["no_rs_changes"];

const INSERT_STAGE_SQL: &str = "INSERT INTO pipeline_stages (
    repo_id, stage_name, stage_order, trigger_after, skip_condition, provider, agent_override_id
 ) VALUES ($1, $2, $3, $4, $5, $6, $7)";

const SELECT_STAGES_SQL: &str =
    "SELECT id, repo_id, stage_name, stage_order, trigger_after, skip_condition, provider,
        agent_override_id
 FROM pipeline_stages
 WHERE ($1::text IS NULL OR repo_id = $1)
   AND ($2::text IS NULL OR agent_override_id = $2)
 ORDER BY stage_order ASC";

#[derive(Debug)]
pub enum PipelineRouteError {
    BadRequest { stage: String, error: String },
    NotFound(String),
    Database(String),
}

/// Fields the runtime reads (policies/pipeline.js, review-automation.js).
/// Older clients may still send the retired metadata fields; serde ignores them.
#[derive(Debug, Deserialize)]
pub struct PipelineStageInput {
    pub stage_name: String,
    pub stage_order: Option<i64>,
    pub trigger_after: Option<String>,
    pub provider: Option<String>,
    pub agent_override_id: Option<String>,
    pub skip_condition: Option<String>,
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

        sqlx::query("DELETE FROM pipeline_stages WHERE repo_id = $1")
            .bind(repo)
            .execute(&mut *tx)
            .await
            .map_err(|error| PipelineRouteError::Database(format!("delete: {error}")))?;

        for (idx, stage) in stages.iter().enumerate() {
            let order = stage.stage_order.unwrap_or(idx as i64 + 1);

            sqlx::query(INSERT_STAGE_SQL)
                .bind(repo)
                .bind(&stage.stage_name)
                .bind(order)
                .bind(stage.trigger_after.as_deref())
                .bind(normalize_optional(stage.skip_condition.as_deref()))
                .bind(normalize_optional(stage.provider.as_deref()))
                .bind(normalize_optional(stage.agent_override_id.as_deref()))
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
        let result = sqlx::query("DELETE FROM pipeline_stages WHERE repo_id = $1")
            .bind(repo)
            .execute(self.pool)
            .await
            .map_err(database_error)?;
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

fn normalize_optional(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|v| !v.is_empty())
}

fn validate_pipeline_stages(stages: &[PipelineStageInput]) -> Result<(), PipelineRouteError> {
    for stage in stages {
        if let Some(condition) = normalize_optional(stage.skip_condition.as_deref())
            && !STAGE_SKIP_CONDITIONS.contains(&condition)
        {
            return Err(PipelineRouteError::BadRequest {
                stage: stage.stage_name.clone(),
                error: format!(
                    "skip_condition='{condition}' is not evaluated by the runtime; expected one of {STAGE_SKIP_CONDITIONS:?}"
                ),
            });
        }
    }
    Ok(())
}

fn pg_stage_row_to_json(row: &sqlx::postgres::PgRow) -> Result<Value, sqlx::Error> {
    let repo_id = row.try_get::<Option<String>, _>("repo_id")?;
    Ok(json!({
        "id": row.try_get::<i64, _>("id")?,
        "repo_id": repo_id,
        "repo": repo_id,
        "stage_name": row.try_get::<Option<String>, _>("stage_name")?,
        "stage_order": row.try_get::<i64, _>("stage_order")?,
        "trigger_after": row.try_get::<Option<String>, _>("trigger_after")?,
        "skip_condition": row.try_get::<Option<String>, _>("skip_condition")?,
        "provider": row.try_get::<Option<String>, _>("provider")?,
        "agent_override_id": row.try_get::<Option<String>, _>("agent_override_id")?,
    }))
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
            let name = stage["stage_name"].as_str().unwrap_or("");
            !name.is_empty() && (name == dispatch_type || name == title)
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

    fn stage(skip_condition: Option<&str>) -> PipelineStageInput {
        PipelineStageInput {
            stage_name: "e2e".to_string(),
            stage_order: None,
            trigger_after: Some("review_pass".to_string()),
            provider: Some("counter".to_string()),
            agent_override_id: None,
            skip_condition: skip_condition.map(str::to_string),
        }
    }

    #[test]
    fn only_runtime_skip_conditions_validate() {
        validate_pipeline_stages(&[stage(None), stage(Some(" ")), stage(Some("no_rs_changes"))])
            .expect("absent, blank and no_rs_changes are accepted");
        let err = validate_pipeline_stages(&[stage(Some("label:hotfix"))])
            .expect_err("a condition the runtime never evaluates is rejected");
        assert!(matches!(err, PipelineRouteError::BadRequest { .. }));
    }

    /// Migrations used to mark `pipeline_stages` file-canonical, so every write
    /// through this service failed with 405 on a real install.
    #[tokio::test]
    async fn replace_stages_writes_on_a_migrated_database_pg() {
        let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
            "agentdesk_pipeline_stages",
            "pipeline stage persistence",
        )
        .await
        else {
            return;
        };
        let pool = pg_db.connect_and_migrate().await;
        let service = PipelineRouteService::new(&pool);

        let written = service
            .replace_stages("repo-rt", &[stage(Some("no_rs_changes"))])
            .await
            .expect("replace_stages succeeds without any source-of-truth override");
        assert_eq!(written.len(), 1);
        assert_eq!(written[0]["stage_order"], json!(1));
        assert_eq!(written[0]["skip_condition"], json!("no_rs_changes"));
        assert_eq!(written[0]["provider"], json!("counter"));

        assert_eq!(service.delete_stages("repo-rt").await.expect("delete"), 1);
        assert!(
            service
                .list_stages(Some("repo-rt"), None)
                .await
                .expect("list")
                .is_empty()
        );

        pg_db.drop().await;
    }
}
