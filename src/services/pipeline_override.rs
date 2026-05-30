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
        let (config_str, _) = self
            .validate_agent_pipeline_config(agent_id, config)
            .await?;
        self.write_agent_pipeline(agent_id, config_str.as_deref())
            .await?;
        crate::pipeline::refresh_override_health_report(Some(self.pool)).await;
        Ok(())
    }

    pub async fn validate_agent_pipeline_config(
        &self,
        agent_id: &str,
        config: Option<&Value>,
    ) -> Result<(Option<String>, Option<crate::pipeline::PipelineOverride>), PipelineOverrideError>
    {
        let (config_str, agent_override) = parse_pipeline_override_config(config)?;
        self.ensure_agent_exists(agent_id).await?;
        crate::pipeline::ensure_loaded();
        self.validate_against_existing_repo_overrides(agent_id, agent_override.as_ref())
            .await?;
        Ok((config_str, agent_override))
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
    /// = $1` via `kanban_cards.assigned_agent_id` — that carries its own
    /// non-null pipeline override, and validate the merged repo+agent
    /// effective pipeline. Unassigned repo cards use the default+repo context,
    /// already covered by validating `new_repo_override` without an agent.
    /// Reject the write if any actual card context is invalid.
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
                    return Err(PipelineOverrideError::BadRequest(format!(
                        "malformed existing agent override (agent={agent_id}) blocks pipeline override write: {error}"
                    )));
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
    /// `kanban_cards.assigned_agent_id = $1` via `kanban_cards.repo_id` — and
    /// validate the merged repo+agent effective pipeline. Assigned standalone
    /// cards (`repo_id IS NULL`) and dangling repo references (`repo_id` with
    /// no matching `github_repos` row) validate the default+agent merge in
    /// addition to any repo-backed contexts. If the agent is not currently
    /// paired with any repo, validate the default+agent merge so standalone
    /// agent configs remain guarded. Reject the write if any actual card
    /// context is invalid.
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
              WHERE c.assigned_agent_id = $1",
        )
        .bind(agent_id)
        .fetch_all(self.pool)
        .await
        .map_err(database_error)?;

        let has_default_agent_assigned_card = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(
                SELECT 1
                  FROM kanban_cards c
                  LEFT JOIN github_repos r ON r.id = c.repo_id
                 WHERE c.assigned_agent_id = $1
                   AND (c.repo_id IS NULL OR r.id IS NULL)
             )",
        )
        .bind(agent_id)
        .fetch_one(self.pool)
        .await
        .map_err(database_error)?;

        if rows.is_empty() {
            validate_pipeline_override(None, new_agent_override)?;
            return Ok(());
        }

        if has_default_agent_assigned_card {
            if let Err(PipelineOverrideError::BadRequest(message)) =
                validate_pipeline_override(None, new_agent_override)
            {
                return Err(PipelineOverrideError::BadRequest(format!(
                    "merged pipeline invalid for standalone assigned card or dangling repo assigned card (agent={agent_id}): {message}"
                )));
            }
        }

        for (repo_id, raw) in rows {
            let existing = match raw.as_deref() {
                Some(value) if !value.trim().is_empty() => {
                    match crate::pipeline::parse_override(value) {
                        Ok(parsed) => parsed,
                        Err(error) => {
                            return Err(PipelineOverrideError::BadRequest(format!(
                                "malformed existing repo override (repo={repo_id}) blocks pipeline override write: {error}"
                            )));
                        }
                    }
                }
                _ => None,
            };
            if let Err(PipelineOverrideError::BadRequest(message)) =
                validate_pipeline_override(existing.as_ref(), new_agent_override)
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

#[cfg(test)]
mod tests {
    use super::*;

    struct TestPostgresDb {
        _lifecycle: crate::db::postgres::PostgresTestLifecycleGuard,
        admin_url: String,
        database_name: String,
        database_url: String,
    }

    impl TestPostgresDb {
        async fn create() -> Option<Self> {
            let lifecycle = crate::db::postgres::lock_test_lifecycle();
            let admin_url = postgres_admin_url();
            let database_name = format!(
                "agentdesk_pipeline_override_{}",
                uuid::Uuid::new_v4().simple()
            );
            let database_url = format!("{}/{}", postgres_base_database_url(), database_name);
            if let Err(error) = crate::db::postgres::create_test_database(
                &admin_url,
                &database_name,
                "pipeline_override tests",
            )
            .await
            {
                eprintln!(
                    "skipping postgres pipeline_override test: create database failed: {error}"
                );
                return None;
            }
            Some(Self {
                _lifecycle: lifecycle,
                admin_url,
                database_name,
                database_url,
            })
        }

        async fn connect_with_minimal_schema(&self) -> Option<PgPool> {
            match crate::db::postgres::connect_test_pool(
                &self.database_url,
                "pipeline_override tests",
            )
            .await
            {
                Ok(pool) => {
                    if let Err(error) = create_minimal_pipeline_override_schema(&pool).await {
                        eprintln!(
                            "skipping postgres pipeline_override test: create schema failed: {error}"
                        );
                        pool.close().await;
                        return None;
                    }
                    Some(pool)
                }
                Err(error) => {
                    eprintln!("skipping postgres pipeline_override test: connect failed: {error}");
                    None
                }
            }
        }

        async fn drop(self) {
            let _ = crate::db::postgres::drop_test_database(
                &self.admin_url,
                &self.database_name,
                "pipeline_override tests",
            )
            .await;
        }
    }

    fn postgres_base_database_url() -> String {
        if let Ok(base) = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE") {
            let trimmed = base.trim();
            if !trimmed.is_empty() {
                return trimmed.trim_end_matches('/').to_string();
            }
        }
        let user = std::env::var("PGUSER")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                std::env::var("USER")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            })
            .unwrap_or_else(|| "postgres".to_string());
        let password = std::env::var("PGPASSWORD")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let host = std::env::var("PGHOST")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "127.0.0.1".to_string());
        let port = std::env::var("PGPORT")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "5432".to_string());

        match password {
            Some(password) => format!("postgres://{user}:{password}@{host}:{port}"),
            None => format!("postgres://{user}@{host}:{port}"),
        }
    }

    fn postgres_admin_url() -> String {
        if let Ok(url) = std::env::var("POSTGRES_TEST_ADMIN_URL") {
            let trimmed = url.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
        format!("{}/postgres", postgres_base_database_url())
    }

    async fn create_minimal_pipeline_override_schema(pool: &PgPool) -> Result<(), sqlx::Error> {
        sqlx::query(
            "CREATE TABLE agents (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                pipeline_config JSONB
             )",
        )
        .execute(pool)
        .await?;
        sqlx::query(
            "CREATE TABLE github_repos (
                id TEXT PRIMARY KEY,
                display_name TEXT,
                default_agent_id TEXT,
                pipeline_config JSONB
             )",
        )
        .execute(pool)
        .await?;
        sqlx::query(
            "CREATE TABLE kanban_cards (
                id TEXT PRIMARY KEY,
                repo_id TEXT,
                assigned_agent_id TEXT
             )",
        )
        .execute(pool)
        .await?;
        Ok(())
    }

    async fn seed_agent(pool: &PgPool, agent_id: &str, pipeline_config: Option<&str>) {
        sqlx::query(
            "INSERT INTO agents (id, name, pipeline_config)
             VALUES ($1, $2, $3::jsonb)",
        )
        .bind(agent_id)
        .bind(format!("Agent {agent_id}"))
        .bind(pipeline_config)
        .execute(pool)
        .await
        .expect("seed agents");
    }

    async fn seed_repo_with_default_agent(
        pool: &PgPool,
        repo_id: &str,
        default_agent_id: &str,
        pipeline_config: Option<&str>,
    ) {
        sqlx::query(
            "INSERT INTO github_repos (id, display_name, default_agent_id, pipeline_config)
             VALUES ($1, $2, $3, $4::jsonb)",
        )
        .bind(repo_id)
        .bind(format!("Repo {repo_id}"))
        .bind(default_agent_id)
        .bind(pipeline_config)
        .execute(pool)
        .await
        .expect("seed github_repos");
    }

    fn valid_repo_override() -> Value {
        serde_json::json!({
            "hooks": {
                "ready": {"on_enter": ["OnCardTransition"], "on_exit": []}
            }
        })
    }

    fn valid_agent_override() -> Value {
        serde_json::json!({
            "hooks": {
                "ready": {"on_enter": ["OnCardTransition"], "on_exit": []}
            }
        })
    }

    fn invalid_slug_state_override() -> Value {
        serde_json::json!({
            "states": [
                {"id": "backlog", "label": "Backlog"},
                {"id": "qa-test", "label": "QA Test"},
                {"id": "done", "label": "Done", "terminal": true}
            ],
            "transitions": [
                {"from": "backlog", "to": "qa-test", "type": "free"},
                {"from": "qa-test", "to": "done", "type": "gated", "gates": ["review_passed"]}
            ],
            "gates": {
                "review_passed": {"type": "builtin", "check": "review_verdict_pass"}
            }
        })
    }

    fn repo_override_with_staging_review_state() -> Value {
        serde_json::json!({
            "states": [
                {"id": "backlog", "label": "Backlog"},
                {"id": "ready", "label": "Ready"},
                {"id": "requested", "label": "Requested"},
                {"id": "in_progress", "label": "In Progress"},
                {"id": "review", "label": "Review"},
                {"id": "staging_review", "label": "Staging Review"},
                {"id": "done", "label": "Done", "terminal": true}
            ]
        })
    }

    fn repo_override_strips_in_progress() -> Value {
        serde_json::json!({
            "states": [
                {"id": "backlog", "label": "Backlog"},
                {"id": "ready", "label": "Ready"},
                {"id": "requested", "label": "Requested"},
                {"id": "done", "label": "Done", "terminal": true}
            ],
            "transitions": [
                {"from": "backlog", "to": "ready", "type": "free"},
                {"from": "ready", "to": "requested", "type": "free"},
                {"from": "requested", "to": "done", "type": "free"}
            ],
            "hooks": {
                "requested": {"on_enter": ["OnCardTransition"], "on_exit": []},
                "done": {"on_enter": ["OnCardTransition", "OnCardTerminal"], "on_exit": []}
            },
            "clocks": {
                "requested": {"set": "requested_at"},
                "done": {"set": "completed_at"}
            },
            "timeouts": {
                "requested": {
                    "duration": "45m",
                    "clock": "requested_at",
                    "max_retries": 1,
                    "backoff": "exponential",
                    "on_exhaust": "requested",
                    "on_exhaust_policy": "escalate"
                }
            }
        })
    }

    fn agent_override_to_staging_review() -> Value {
        serde_json::json!({
            "transitions": [
                {"from": "in_progress", "to": "staging_review", "type": "free"}
            ],
            "hooks": {
                "staging_review": {"on_enter": ["OnReviewEnter"], "on_exit": []}
            }
        })
    }

    fn agent_override_uses_in_progress() -> Value {
        serde_json::json!({
            "transitions": [
                {"from": "backlog", "to": "in_progress", "type": "free"}
            ]
        })
    }

    async fn seed_card(
        pool: &PgPool,
        card_id: &str,
        repo_id: Option<&str>,
        assigned_agent_id: Option<&str>,
    ) {
        sqlx::query(
            "INSERT INTO kanban_cards (id, repo_id, assigned_agent_id)
             VALUES ($1, $2, $3)",
        )
        .bind(card_id)
        .bind(repo_id)
        .bind(assigned_agent_id)
        .execute(pool)
        .await
        .expect("seed kanban_cards");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn repo_write_rejects_state_id_that_would_fail_kanban_status_check() {
        let Some(pg_db) = TestPostgresDb::create().await else {
            return;
        };
        let Some(pool) = pg_db.connect_with_minimal_schema().await else {
            pg_db.drop().await;
            return;
        };
        crate::pipeline::ensure_loaded();
        seed_agent(&pool, "agent-slug-a", None).await;
        seed_repo_with_default_agent(&pool, "repo-slug-a", "agent-slug-a", None).await;

        let service = PipelineOverrideService::new(&pool);
        let result = service
            .set_repo_pipeline("repo-slug-a", Some(&invalid_slug_state_override()))
            .await;

        match result {
            Err(PipelineOverrideError::BadRequest(message)) => {
                assert!(
                    message.contains("kanban status slug contract ^[a-z][a-z0-9_]*$"),
                    "BadRequest must explain kanban status slug contract, got: {message}"
                );
            }
            other => panic!(
                "expected BadRequest for invalid state id, got: {:?}",
                other.map(|()| "Ok").unwrap_or("non-BadRequest err")
            ),
        }

        let stored: Option<String> = sqlx::query_scalar(
            "SELECT pipeline_config::text FROM github_repos WHERE id = 'repo-slug-a'",
        )
        .fetch_one(&pool)
        .await
        .expect("repo pipeline_config lookup");
        assert!(
            stored.is_none(),
            "repo pipeline_config must remain NULL after rejected write; got {stored:?}"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn agent_write_rejects_standalone_assigned_card_context_when_repo_pair_valid() {
        let Some(pg_db) = TestPostgresDb::create().await else {
            return;
        };
        let Some(pool) = pg_db.connect_with_minimal_schema().await else {
            pg_db.drop().await;
            return;
        };
        crate::pipeline::ensure_loaded();
        seed_agent(&pool, "agent-standalone-context", None).await;
        seed_repo_with_default_agent(
            &pool,
            "repo-provides-staging-review",
            "agent-standalone-context",
            Some(&repo_override_with_staging_review_state().to_string()),
        )
        .await;
        seed_card(
            &pool,
            "card-repo-agent-context",
            Some("repo-provides-staging-review"),
            Some("agent-standalone-context"),
        )
        .await;
        seed_card(
            &pool,
            "card-standalone-agent-context",
            None,
            Some("agent-standalone-context"),
        )
        .await;

        let service = PipelineOverrideService::new(&pool);
        let result = service
            .set_agent_pipeline(
                "agent-standalone-context",
                Some(&agent_override_to_staging_review()),
            )
            .await;

        match result {
            Err(PipelineOverrideError::BadRequest(message)) => {
                assert!(
                    message.contains("standalone assigned card"),
                    "BadRequest must explain standalone default+agent validation, got: {message}"
                );
                assert!(
                    message.contains("staging_review"),
                    "BadRequest must include the missing standalone state context, got: {message}"
                );
            }
            other => panic!(
                "expected BadRequest for standalone assigned card context, got: {:?}",
                other.map(|()| "Ok").unwrap_or("non-BadRequest err")
            ),
        }

        let stored: Option<String> = sqlx::query_scalar(
            "SELECT pipeline_config::text FROM agents WHERE id = 'agent-standalone-context'",
        )
        .fetch_one(&pool)
        .await
        .expect("agent pipeline_config lookup");
        assert!(
            stored.is_none(),
            "agent pipeline_config must remain NULL after rejected standalone validation; got {stored:?}"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn agent_write_rejects_dangling_repo_assigned_card_context_when_repo_pair_valid() {
        let Some(pg_db) = TestPostgresDb::create().await else {
            return;
        };
        let Some(pool) = pg_db.connect_with_minimal_schema().await else {
            pg_db.drop().await;
            return;
        };
        crate::pipeline::ensure_loaded();
        seed_agent(&pool, "agent-dangling-repo-context", None).await;
        seed_repo_with_default_agent(
            &pool,
            "repo-provides-staging-review-for-dangling",
            "agent-dangling-repo-context",
            Some(&repo_override_with_staging_review_state().to_string()),
        )
        .await;
        seed_card(
            &pool,
            "card-valid-repo-agent-context",
            Some("repo-provides-staging-review-for-dangling"),
            Some("agent-dangling-repo-context"),
        )
        .await;
        seed_card(
            &pool,
            "card-dangling-repo-agent-context",
            Some("repo-missing-staging-review"),
            Some("agent-dangling-repo-context"),
        )
        .await;

        let service = PipelineOverrideService::new(&pool);
        let result = service
            .set_agent_pipeline(
                "agent-dangling-repo-context",
                Some(&agent_override_to_staging_review()),
            )
            .await;

        match result {
            Err(PipelineOverrideError::BadRequest(message)) => {
                assert!(
                    message.contains("dangling repo assigned card"),
                    "BadRequest must explain dangling repo default+agent validation, got: {message}"
                );
                assert!(
                    message.contains("staging_review"),
                    "BadRequest must include the missing dangling repo state context, got: {message}"
                );
            }
            other => panic!(
                "expected BadRequest for dangling repo assigned card context, got: {:?}",
                other.map(|()| "Ok").unwrap_or("non-BadRequest err")
            ),
        }

        let stored: Option<String> = sqlx::query_scalar(
            "SELECT pipeline_config::text FROM agents WHERE id = 'agent-dangling-repo-context'",
        )
        .fetch_one(&pool)
        .await
        .expect("agent pipeline_config lookup");
        assert!(
            stored.is_none(),
            "agent pipeline_config must remain NULL after rejected dangling repo validation; got {stored:?}"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn repo_write_accepts_unassigned_repo_card_without_default_agent_fallback() {
        let Some(pg_db) = TestPostgresDb::create().await else {
            return;
        };
        let Some(pool) = pg_db.connect_with_minimal_schema().await else {
            pg_db.drop().await;
            return;
        };
        crate::pipeline::ensure_loaded();
        seed_agent(
            &pool,
            "agent-default-fallback-only",
            Some(&agent_override_uses_in_progress().to_string()),
        )
        .await;
        seed_repo_with_default_agent(
            &pool,
            "repo-unassigned-card",
            "agent-default-fallback-only",
            None,
        )
        .await;
        seed_card(
            &pool,
            "card-unassigned-repo-context",
            Some("repo-unassigned-card"),
            None,
        )
        .await;

        let service = PipelineOverrideService::new(&pool);
        service
            .set_repo_pipeline(
                "repo-unassigned-card",
                Some(&repo_override_strips_in_progress()),
            )
            .await
            .expect("unassigned repo cards validate default+repo, not default_agent fallback");

        let stored: Option<String> = sqlx::query_scalar(
            "SELECT pipeline_config::text FROM github_repos WHERE id = 'repo-unassigned-card'",
        )
        .fetch_one(&pool)
        .await
        .expect("repo pipeline_config lookup");
        let stored = stored.expect("repo pipeline_config should be stored");
        assert!(
            stored.contains("ready"),
            "repo pipeline_config should contain the accepted repo override; got {stored}"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn repo_write_rejects_malformed_existing_agent_override() {
        let Some(pg_db) = TestPostgresDb::create().await else {
            return;
        };
        let Some(pool) = pg_db.connect_with_minimal_schema().await else {
            pg_db.drop().await;
            return;
        };
        crate::pipeline::ensure_loaded();
        seed_agent(&pool, "agent-1720-a", Some(r#"{"states":["broken"]}"#)).await;
        seed_repo_with_default_agent(&pool, "repo-1720-a", "agent-1720-a", None).await;
        seed_card(
            &pool,
            "card-1720-a",
            Some("repo-1720-a"),
            Some("agent-1720-a"),
        )
        .await;

        let service = PipelineOverrideService::new(&pool);
        let result = service
            .set_repo_pipeline("repo-1720-a", Some(&valid_repo_override()))
            .await;

        match result {
            Err(PipelineOverrideError::BadRequest(message)) => {
                assert!(
                    message.contains("malformed existing agent override"),
                    "BadRequest must explain malformed existing agent override, got: {message}"
                );
                assert!(
                    message.contains("agent-1720-a"),
                    "BadRequest must name the offending agent, got: {message}"
                );
            }
            other => panic!(
                "expected BadRequest naming malformed agent-1720-a, got: {:?}",
                other.map(|()| "Ok").unwrap_or("non-BadRequest err")
            ),
        }

        let stored: Option<String> = sqlx::query_scalar(
            "SELECT pipeline_config::text FROM github_repos WHERE id = 'repo-1720-a'",
        )
        .fetch_one(&pool)
        .await
        .expect("repo pipeline_config lookup");
        assert!(
            stored.is_none(),
            "repo pipeline_config must remain NULL after rejected write; got {stored:?}"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn agent_write_rejects_malformed_existing_repo_override() {
        let Some(pg_db) = TestPostgresDb::create().await else {
            return;
        };
        let Some(pool) = pg_db.connect_with_minimal_schema().await else {
            pg_db.drop().await;
            return;
        };
        crate::pipeline::ensure_loaded();
        seed_agent(&pool, "agent-1720-b", None).await;
        seed_repo_with_default_agent(
            &pool,
            "repo-1720-b",
            "agent-1720-b",
            Some(r#"{"states":["broken"]}"#),
        )
        .await;
        seed_card(
            &pool,
            "card-1720-b",
            Some("repo-1720-b"),
            Some("agent-1720-b"),
        )
        .await;

        let service = PipelineOverrideService::new(&pool);
        let result = service
            .set_agent_pipeline("agent-1720-b", Some(&valid_agent_override()))
            .await;

        match result {
            Err(PipelineOverrideError::BadRequest(message)) => {
                assert!(
                    message.contains("malformed existing repo override"),
                    "BadRequest must explain malformed existing repo override, got: {message}"
                );
                assert!(
                    message.contains("repo-1720-b"),
                    "BadRequest must name the offending repo, got: {message}"
                );
            }
            other => panic!(
                "expected BadRequest naming malformed repo-1720-b, got: {:?}",
                other.map(|()| "Ok").unwrap_or("non-BadRequest err")
            ),
        }

        let stored: Option<String> = sqlx::query_scalar(
            "SELECT pipeline_config::text FROM agents WHERE id = 'agent-1720-b'",
        )
        .fetch_one(&pool)
        .await
        .expect("agent pipeline_config lookup");
        assert!(
            stored.is_none(),
            "agent pipeline_config must remain NULL after rejected write; got {stored:?}"
        );

        pool.close().await;
        pg_db.drop().await;
    }
}
