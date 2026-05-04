use serde_json::Value;
use sqlx::PgPool;

#[derive(Debug)]
pub enum PipelineOverrideError {
    BadRequest(String),
    NotFound(&'static str),
    Database(String),
}

pub struct PipelineOverrideService<'a> {
    pool: &'a PgPool,
}

impl<'a> PipelineOverrideService<'a> {
    pub fn new(pool: &'a PgPool) -> Self {
        Self { pool }
    }

    pub async fn get_repo_pipeline(&self, repo_id: &str) -> Result<Value, PipelineOverrideError> {
        let config = sqlx::query_scalar::<_, Option<String>>(
            "SELECT pipeline_config::text AS pipeline_config FROM github_repos WHERE id = $1",
        )
        .bind(repo_id)
        .fetch_optional(self.pool)
        .await
        .map_err(database_error)?
        .flatten();

        Ok(parse_stored_config(config.as_deref()))
    }

    pub async fn set_repo_pipeline(
        &self,
        repo_id: &str,
        config: Option<&Value>,
    ) -> Result<(), PipelineOverrideError> {
        let (config_str, repo_override) = parse_pipeline_override_config(config)?;
        self.ensure_repo_exists(repo_id).await?;
        validate_pipeline_override(repo_override.as_ref(), None)?;
        self.validate_against_existing_agent_overrides(repo_id, repo_override.as_ref())
            .await?;
        self.write_repo_pipeline(repo_id, config_str.as_deref())
            .await?;
        crate::pipeline::refresh_override_health_report(Some(self.pool)).await;
        Ok(())
    }

    pub async fn get_agent_pipeline(&self, agent_id: &str) -> Result<Value, PipelineOverrideError> {
        let config = sqlx::query_scalar::<_, Option<String>>(
            "SELECT pipeline_config::text AS pipeline_config FROM agents WHERE id = $1",
        )
        .bind(agent_id)
        .fetch_optional(self.pool)
        .await
        .map_err(database_error)?
        .flatten();

        Ok(parse_stored_config(config.as_deref()))
    }

    pub async fn set_agent_pipeline(
        &self,
        agent_id: &str,
        config: Option<&Value>,
    ) -> Result<(), PipelineOverrideError> {
        let (config_str, agent_override) = parse_pipeline_override_config(config)?;
        self.ensure_agent_exists(agent_id).await?;
        validate_pipeline_override(None, agent_override.as_ref())?;
        self.validate_against_existing_repo_overrides(agent_id, agent_override.as_ref())
            .await?;
        self.write_agent_pipeline(agent_id, config_str.as_deref())
            .await?;
        crate::pipeline::refresh_override_health_report(Some(self.pool)).await;
        Ok(())
    }

    async fn ensure_repo_exists(&self, repo_id: &str) -> Result<(), PipelineOverrideError> {
        let exists = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM github_repos WHERE id = $1)",
        )
        .bind(repo_id)
        .fetch_one(self.pool)
        .await
        .map_err(database_error)?;
        if exists {
            Ok(())
        } else {
            Err(PipelineOverrideError::NotFound("repo not found"))
        }
    }

    async fn ensure_agent_exists(&self, agent_id: &str) -> Result<(), PipelineOverrideError> {
        let exists =
            sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM agents WHERE id = $1)")
                .bind(agent_id)
                .fetch_one(self.pool)
                .await
                .map_err(database_error)?;
        if exists {
            Ok(())
        } else {
            Err(PipelineOverrideError::NotFound("agent not found"))
        }
    }

    /// When writing a repo override, fetch every agent that pairs with this
    /// repo at runtime — i.e. every agent referenced by `kanban_cards.repo_id
    /// = $1` via `kanban_cards.assigned_agent_id`, plus the
    /// `github_repos.default_agent_id` fallback for unassigned cards — that
    /// carries its own non-null pipeline override, and validate the merged
    /// repo+agent effective pipeline. Reject the write if any combination is
    /// invalid.
    ///
    /// The runtime resolver `crate::pipeline::resolve(repo_override,
    /// agent_override)` is invoked per-card with `(kanban_cards.repo_id,
    /// kanban_cards.assigned_agent_id)`, so the cross-layer gate must check
    /// the same pairs (#1692).
    async fn validate_against_existing_agent_overrides(
        &self,
        repo_id: &str,
        new_repo_override: Option<&crate::pipeline::PipelineOverride>,
    ) -> Result<(), PipelineOverrideError> {
        let rows = sqlx::query_as::<_, (String, Option<String>)>(
            "SELECT DISTINCT a.id, a.pipeline_config::text \
               FROM kanban_cards c \
               JOIN agents a ON a.id = c.assigned_agent_id \
              WHERE c.repo_id = $1 \
                AND a.pipeline_config IS NOT NULL \
                AND TRIM(a.pipeline_config::text) <> '' \
             UNION \
             SELECT a.id, a.pipeline_config::text \
               FROM github_repos r \
               JOIN agents a ON a.id = r.default_agent_id \
              WHERE r.id = $1 \
                AND a.pipeline_config IS NOT NULL \
                AND TRIM(a.pipeline_config::text) <> ''",
        )
        .bind(repo_id)
        .fetch_all(self.pool)
        .await
        .map_err(database_error)?;

        for (agent_id, raw) in rows {
            let raw = match raw {
                Some(value) => value,
                None => continue,
            };
            let existing = match crate::pipeline::parse_override(&raw) {
                Ok(Some(parsed)) => parsed,
                Ok(None) => continue,
                Err(error) => {
                    tracing::warn!(
                        "[pipeline_override] skipping malformed agent override during cross-layer validation \
                         (agent={agent_id}): {error}"
                    );
                    continue;
                }
            };
            if let Err(PipelineOverrideError::BadRequest(message)) =
                validate_pipeline_override(new_repo_override, Some(&existing))
            {
                return Err(PipelineOverrideError::BadRequest(format!(
                    "merged pipeline invalid when combined with existing agent override (agent={agent_id}): {message}"
                )));
            }
        }
        Ok(())
    }

    /// When writing an agent override, fetch every repo that pairs with this
    /// agent at runtime — i.e. every repo referenced by
    /// `kanban_cards.assigned_agent_id = $1` via `kanban_cards.repo_id`, plus
    /// the `github_repos.default_agent_id = $1` fallback for unassigned
    /// cards — that carries its own non-null pipeline override, and validate
    /// the merged repo+agent effective pipeline. Reject the write if any
    /// combination is invalid.
    ///
    /// The runtime resolver `crate::pipeline::resolve(repo_override,
    /// agent_override)` is invoked per-card with `(kanban_cards.repo_id,
    /// kanban_cards.assigned_agent_id)`, so the cross-layer gate must check
    /// the same pairs (#1692).
    async fn validate_against_existing_repo_overrides(
        &self,
        agent_id: &str,
        new_agent_override: Option<&crate::pipeline::PipelineOverride>,
    ) -> Result<(), PipelineOverrideError> {
        let rows = sqlx::query_as::<_, (String, Option<String>)>(
            "SELECT DISTINCT r.id, r.pipeline_config::text \
               FROM kanban_cards c \
               JOIN github_repos r ON r.id = c.repo_id \
              WHERE c.assigned_agent_id = $1 \
                AND r.pipeline_config IS NOT NULL \
                AND TRIM(r.pipeline_config::text) <> '' \
             UNION \
             SELECT r.id, r.pipeline_config::text \
               FROM github_repos r \
              WHERE r.default_agent_id = $1 \
                AND r.pipeline_config IS NOT NULL \
                AND TRIM(r.pipeline_config::text) <> ''",
        )
        .bind(agent_id)
        .fetch_all(self.pool)
        .await
        .map_err(database_error)?;

        for (repo_id, raw) in rows {
            let raw = match raw {
                Some(value) => value,
                None => continue,
            };
            let existing = match crate::pipeline::parse_override(&raw) {
                Ok(Some(parsed)) => parsed,
                Ok(None) => continue,
                Err(error) => {
                    tracing::warn!(
                        "[pipeline_override] skipping malformed repo override during cross-layer validation \
                         (repo={repo_id}): {error}"
                    );
                    continue;
                }
            };
            if let Err(PipelineOverrideError::BadRequest(message)) =
                validate_pipeline_override(Some(&existing), new_agent_override)
            {
                return Err(PipelineOverrideError::BadRequest(format!(
                    "merged pipeline invalid when combined with existing repo override (repo={repo_id}): {message}"
                )));
            }
        }
        Ok(())
    }

    async fn write_repo_pipeline(
        &self,
        repo_id: &str,
        config: Option<&str>,
    ) -> Result<(), PipelineOverrideError> {
        let result =
            sqlx::query("UPDATE github_repos SET pipeline_config = $1::jsonb WHERE id = $2")
                .bind(config)
                .bind(repo_id)
                .execute(self.pool)
                .await
                .map_err(database_error)?;
        if result.rows_affected() == 0 {
            Err(PipelineOverrideError::NotFound("repo not found"))
        } else {
            Ok(())
        }
    }

    async fn write_agent_pipeline(
        &self,
        agent_id: &str,
        config: Option<&str>,
    ) -> Result<(), PipelineOverrideError> {
        let result = sqlx::query("UPDATE agents SET pipeline_config = $1::jsonb WHERE id = $2")
            .bind(config)
            .bind(agent_id)
            .execute(self.pool)
            .await
            .map_err(database_error)?;
        if result.rows_affected() == 0 {
            Err(PipelineOverrideError::NotFound("agent not found"))
        } else {
            Ok(())
        }
    }
}

fn parse_stored_config(config: Option<&str>) -> Value {
    config
        .and_then(|raw| serde_json::from_str(raw).ok())
        .unwrap_or(Value::Null)
}

fn parse_pipeline_override_config(
    config: Option<&Value>,
) -> Result<(Option<String>, Option<crate::pipeline::PipelineOverride>), PipelineOverrideError> {
    match config {
        Some(value) if !value.is_null() => {
            let config = value.to_string();
            match crate::pipeline::parse_override(&config) {
                Ok(parsed) => Ok((Some(config), parsed)),
                Err(error) => Err(PipelineOverrideError::BadRequest(format!(
                    "invalid pipeline config: {error}"
                ))),
            }
        }
        _ => Ok((None, None)),
    }
}

fn validate_pipeline_override(
    repo_override: Option<&crate::pipeline::PipelineOverride>,
    agent_override: Option<&crate::pipeline::PipelineOverride>,
) -> Result<(), PipelineOverrideError> {
    let effective = crate::pipeline::resolve(repo_override, agent_override);
    effective.validate().map_err(|error| {
        PipelineOverrideError::BadRequest(format!("merged pipeline validation failed: {error}"))
    })
}

fn database_error(error: sqlx::Error) -> PipelineOverrideError {
    PipelineOverrideError::Database(error.to_string())
}
