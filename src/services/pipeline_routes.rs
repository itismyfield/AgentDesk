use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{PgPool, Row};

use crate::db::table_metadata;
use crate::utils::api::clamp_api_limit;

/// #1082 -- accepted `on_failure` policy values.
/// Kept in sync with `crate::pipeline::OnFailurePolicy`.
pub const STAGE_ON_FAILURE_VALUES: &[&str] =
    &["escalate", "retry-with-backoff", "fallback-stage", "fail"];

/// #1082 -- accepted `backoff` policy values.
pub const STAGE_BACKOFF_VALUES: &[&str] = &["exponential", "linear", "none"];

/// `replace_stages` upsert. `backoff` ($14) added by #3868 so the validated
/// value is persisted instead of silently dropped. Column order MUST match the
/// `.bind(...)` chain in `replace_stages`.
const INSERT_STAGE_SQL: &str = "INSERT INTO pipeline_stages (
    repo_id, stage_name, stage_order, trigger_after, entry_skill,
    timeout_minutes, on_failure, skip_condition, provider, agent_override_id,
    on_failure_target, max_retries, parallel_with, backoff
 ) VALUES (
    $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14
 )";

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

#[derive(Debug)]
pub enum PipelineRouteError {
    BadRequest { stage: String, error: String },
    NotFound(String),
    Readonly { table: String, source: &'static str },
    Database(String),
}

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
    /// declarative stage metadata (#3868) so it round-trips through GET.
    /// #3916 added the typed policy *resolver* (`OnFailurePolicy`/`TimeoutConfig`
    /// in `decide_timeout`), but it is NOT yet on the live timeout path — the
    /// live sweep (policies/timeouts) does not route through the reducer, and
    /// these per-stage-row DB columns are still read by no executor. Both remain
    /// declarative pending the deferred live-wiring follow-up to #3916.
    pub backoff: Option<String>,
    pub skip_condition: Option<String>,
    pub parallel_with: Option<String>,
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
        self.ensure_table_writable("pipeline_stages").await?;
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
            let timeout = stage.timeout_minutes.unwrap_or(60);
            let on_failure = stage.on_failure.as_deref().unwrap_or("fail");
            let max_retries = stage.max_retries.unwrap_or(0);

            sqlx::query(INSERT_STAGE_SQL)
                .bind(repo)
                .bind(&stage.stage_name)
                .bind(order)
                .bind(stage.trigger_after.as_deref())
                .bind(stage.entry_skill.as_deref())
                .bind(timeout)
                .bind(on_failure)
                .bind(stage.skip_condition.as_deref())
                .bind(stage.provider.as_deref())
                .bind(stage.agent_override_id.as_deref())
                .bind(stage.on_failure_target.as_deref())
                .bind(max_retries)
                .bind(stage.parallel_with.as_deref())
                .bind(normalize_optional(stage.backoff.as_deref()))
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
        self.ensure_table_writable("pipeline_stages").await?;
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

    async fn ensure_table_writable(&self, table: &str) -> Result<(), PipelineRouteError> {
        let source = table_metadata::source_of_truth_pg(self.pool, table).await;
        if let Some(source) = source
            && source.is_readonly()
        {
            return Err(PipelineRouteError::Readonly {
                table: table.to_string(),
                source: source_label(source),
            });
        }
        Ok(())
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

fn validate_pipeline_stages(stages: &[PipelineStageInput]) -> Result<(), PipelineRouteError> {
    for stage in stages {
        if let Err(error) = validate_on_failure(stage.on_failure.as_deref()) {
            return Err(PipelineRouteError::BadRequest {
                stage: stage.stage_name.clone(),
                error,
            });
        }
        // Validate the *normalized* value so a blank/whitespace-only backoff is
        // treated identically to absent (both persist as NULL), instead of an
        // empty string passing but "   " erroring — consistent with the INSERT,
        // which also binds `normalize_optional(stage.backoff)`.
        if let Err(error) = validate_backoff(normalize_optional(stage.backoff.as_deref())) {
            return Err(PipelineRouteError::BadRequest {
                stage: stage.stage_name.clone(),
                error,
            });
        }
        if let Some(max_retries) = stage.max_retries
            && max_retries < 0
        {
            return Err(PipelineRouteError::BadRequest {
                stage: stage.stage_name.clone(),
                error: format!("max_retries={max_retries} must be >= 0"),
            });
        }
    }
    Ok(())
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

fn source_label(source: table_metadata::Source) -> &'static str {
    match source {
        table_metadata::Source::File => "file",
        table_metadata::Source::FileCanonical => "file-canonical",
        table_metadata::Source::Db => "db",
    }
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

        // `pipeline_stages` seeds as `file-canonical` (read-only) in 0019; flip
        // it to `db` so the API write path is exercised rather than rejected.
        sqlx::query(
            "UPDATE db_table_metadata SET source_of_truth = 'db' \
             WHERE table_name = 'pipeline_stages'",
        )
        .execute(&pool)
        .await
        .expect("flip pipeline_stages to db source-of-truth");

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

        // (2) Absent backoff round-trips as null (no spurious default).
        let listed = service
            .replace_stages("repo-rt", &[stage_with_backoff(None)])
            .await
            .expect("replace_stages without backoff should succeed");
        assert_eq!(listed[0]["backoff"], Value::Null);

        // (3) Whitespace-only backoff normalizes to NULL (consistent with
        // absent/""), neither a BadRequest nor a stored "   ".
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
}
