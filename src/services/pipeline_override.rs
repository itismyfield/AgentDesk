use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgPool;
use std::collections::HashMap;

use crate::pipeline::{
    ClockConfig, GateConfig, HookBindings, PhaseGateConfig, StateConfig, TimeoutConfig,
    TransitionConfig,
};

/// A partial pipeline config used for repo/agent-level overrides.
/// Only non-None fields replace the parent's values.
///
/// `deny_unknown_fields` (#5718): an override key that this struct does not
/// declare is a typo or a retired field, and silently dropping it makes the
/// stored override look applied when it is not. Rejecting it surfaces the key
/// in `PipelineOverrideHealthReport::parse_failures` and in the 400 returned by
/// the pipeline-override write API instead. A row that is *already stored* is
/// read back through `parse_override`, which drops the undeclared key and
/// applies the rest (#5718 r3) — refusing it there would take the row's valid
/// sections down with it. Metadata a supported client really does produce is
/// declared as a field instead (see `fsm_edge_bindings`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PipelineOverride {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub states: Option<Vec<StateConfig>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transitions: Option<Vec<TransitionConfig>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gates: Option<HashMap<String, GateConfig>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hooks: Option<HashMap<String, HookBindings>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub events: Option<HashMap<String, Vec<String>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clocks: Option<HashMap<String, ClockConfig>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeouts: Option<HashMap<String, TimeoutConfig>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase_gate: Option<PhaseGateConfig>,
    /// Visual-editor edge metadata (#5718 review r2). The dashboard FSM editor
    /// binds an explicit event name to a `from->to` edge and carries the map in
    /// the override it PUTs (`updateFsmTransitionEvent` in
    /// `dashboard/src/components/agent-manager/usePipelineVisualEditorActions.ts`,
    /// re-emitted by `buildOverridePayload` in `pipeline-visual-editor-model.ts`).
    /// No Rust reader consumes it, but `deny_unknown_fields` would otherwise 400
    /// every save the supported editor makes, so it is declared here and stored
    /// verbatim as raw JSON: the editor round-trips whatever the stored row held,
    /// and a typed shape would reject an older row the way `deny_unknown_fields` did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fsm_edge_bindings: Option<serde_json::Value>,
}

/// Strict parse of a pipeline override — the **write** boundary (#5718 r3).
/// Returns None if the input is empty/null.
///
/// An undeclared key is an error here, and that is what keeps bad data from
/// landing: `parse_pipeline_override_config` below turns it into the 400 the
/// override write API returns, and `crate::pipeline`'s `build_override_health_report`
/// keeps an already-stored row in `parse_failures[]`. Reading a stored row goes
/// through `parse_override` below, which tolerates the key.
pub fn parse_override_strict(json_str: &str) -> Result<Option<PipelineOverride>> {
    let trimmed = json_str.trim();
    if trimmed.is_empty() || trimmed == "null" || trimmed == "{}" {
        return Ok(None);
    }
    // #5718 review r2: same reason as `crate::pipeline::load()`. The resolver
    // warning, the health report's `parse_failures[].error` and the 400 body from
    // the override write API all format this with plain `Display`, so the rejected
    // key has to be in the message itself rather than in the `source()` chain.
    let ovr: PipelineOverride = serde_json::from_str(trimmed)
        .map_err(|error| anyhow::anyhow!("parsing pipeline override JSON: {error}"))?;
    Ok(Some(ovr))
}

/// Read a stored pipeline override, tolerating keys this build does not
/// declare (#5718 r3).
///
/// Every reader of a stored row goes through here — the dispatch resolver
/// (`crate::pipeline`'s `parse_override_for_resolve`), the transition resolver
/// (`kanban::state_machine::resolve_pipeline_with_pg`), the kanban transaction
/// resolver, the auto-queue view and the GitHub sync — so one stored row cannot
/// be read two different ways. Rejecting it on read also discards its valid
/// sections, the pre-#5718 behaviour this must not regress. The retry is not
/// silent: `parse_override_strict` still rejects the row on write and in the
/// health scan, so it keeps its `parse_failures[]` entry and logs the drop here.
///
/// Anything else — malformed JSON, a declared key holding the wrong shape —
/// still returns `Err`, and each caller keeps its existing handling of that.
pub fn parse_override(json_str: &str) -> Result<Option<PipelineOverride>> {
    let strict_error = match parse_override_strict(json_str) {
        Ok(parsed) => return Ok(parsed),
        Err(error) => error,
    };
    let Some((declared, dropped)) = split_undeclared_override_keys(json_str) else {
        return Err(strict_error);
    };
    let ovr: PipelineOverride = serde_json::from_value(serde_json::Value::Object(declared))
        .map_err(|error| anyhow::anyhow!("parsing pipeline override JSON: {error}"))?;
    tracing::warn!(
        "[pipeline] stored override applied with undeclared key(s) dropped [{}]: {strict_error}",
        dropped.join(", ")
    );
    Ok(Some(ovr))
}

/// Split a stored override object into the top-level keys `PipelineOverride`
/// declares and the ones it does not. `None` when the payload is not a JSON
/// object or when every key is declared — the strict error is then about
/// something else and has to stand.
fn split_undeclared_override_keys(
    json_str: &str,
) -> Option<(serde_json::Map<String, serde_json::Value>, Vec<String>)> {
    let serde_json::Value::Object(object) = serde_json::from_str(json_str).ok()? else {
        return None;
    };
    let mut declared = serde_json::Map::new();
    let mut dropped = Vec::new();
    for (key, value) in object {
        // Probe each key on its own with a null value. Every declared field is
        // an `Option`, so null is accepted for all of them, which separates
        // "this build does not declare the key" (dropped) from "declared key
        // holding the wrong shape" (kept — and still fatal in the re-parse
        // above). Asking serde rather than keeping a second list of field names
        // here means a field added later cannot be dropped by a list nobody updated.
        let mut probe = serde_json::Map::new();
        probe.insert(key.clone(), serde_json::Value::Null);
        if serde_json::from_value::<PipelineOverride>(serde_json::Value::Object(probe)).is_ok() {
            declared.insert(key, value);
        } else {
            dropped.push(key);
        }
    }
    (!dropped.is_empty()).then_some((declared, dropped))
}

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
            let existing = match existing_override_for_cross_check("agent", &agent_id, &raw)? {
                Some(parsed) => parsed,
                None => continue,
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
                    existing_override_for_cross_check("repo", &repo_id, value)?
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
        .and_then(|raw| serde_json::to_value(crate::pipeline::parse_override(raw).ok()).ok())
        .unwrap_or(Value::Null)
}

/// Read the *opposite* layer's stored override for the cross-layer conflict
/// check (#5718 r3).
///
/// That row is only here to be merged against the override being written, so a
/// row that cannot be read even leniently has nothing to contribute: skip this
/// pair's conflict check and warn, instead of failing the write. Failing it
/// made a repo+agent pair whose stored rows both carry an undeclared key
/// unrepairable — the check is symmetric, so neither side could be corrected
/// and not even writing `null` could clear one. The layer actually being
/// written stays strictly validated in `parse_pipeline_override_config`, so a
/// bad override still cannot land.
fn existing_override_for_cross_check(
    layer: &str,
    target_id: &str,
    raw: &str,
) -> Result<Option<crate::pipeline::PipelineOverride>, PipelineOverrideError> {
    match crate::pipeline::parse_override(raw) {
        Ok(parsed) => Ok(parsed),
        Err(error) => {
            tracing::warn!(
                "[pipeline] skipping cross-layer check against unreadable {layer} override {target_id}: {error}"
            );
            Ok(None)
        }
    }
}

fn parse_pipeline_override_config(
    config: Option<&Value>,
) -> Result<(Option<String>, Option<crate::pipeline::PipelineOverride>), PipelineOverrideError> {
    match config {
        Some(value) if !value.is_null() => {
            let config = value.to_string();
            match crate::pipeline::parse_override_strict(&config) {
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
mod cross_layer_read_tests {
    use super::*;

    /// #5718 r3 (R3): the cross-layer check exists to catch a *conflict* between
    /// the two layers. An opposite-layer row carrying an undeclared key still
    /// has readable sections and is checked against them; one that cannot be
    /// read at all offers nothing to compare, so the write proceeds with that
    /// pair skipped rather than returning 400 and leaving the pair unfixable.
    #[test]
    fn unreadable_existing_row_skips_the_check_instead_of_blocking_the_write() {
        let with_undeclared_key = r#"{"stage_failure_policy":{"default":"fail"},"gates":{}}"#;
        let readable = existing_override_for_cross_check("agent", "agent-1", with_undeclared_key)
            .expect("an undeclared key in the opposite layer must not block the write")
            .expect("the row must not be treated as empty");
        assert!(
            readable.gates.is_some(),
            "the readable sections must still take part in the conflict check"
        );
        let served = parse_stored_config(Some(with_undeclared_key));
        parse_pipeline_override_config(Some(&served)).expect("GET body must be accepted by PUT");

        let unreadable =
            existing_override_for_cross_check("repo", "acme/widgets", r#"{"gates": 5}"#)
                .expect("an unreadable opposite-layer row must skip the check, not fail the write");
        assert!(
            unreadable.is_none(),
            "nothing to compare against means no conflict check for that pair"
        );
    }

    /// The layer being written keeps its own strict validation — that is what
    /// stops a new bad override from landing, and R3 must not relax it.
    #[test]
    fn the_layer_being_written_is_still_parsed_strictly() {
        let payload = serde_json::json!({ "stage_failure_policy": { "default": "fail" } });
        let error = parse_pipeline_override_config(Some(&payload))
            .expect_err("an undeclared key in the incoming override must still be rejected");
        let PipelineOverrideError::BadRequest(message) = error else {
            panic!("an undeclared key must be a 400, not another error class");
        };
        assert!(
            message.contains("stage_failure_policy"),
            "the 400 must name the rejected key, got: {message}"
        );
    }
}

#[cfg(test)]
mod pipeline_override_pg_tests {
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
            let Some(base) = crate::db::postgres::postgres_test_database_url_base() else {
                drop(lifecycle);
                return None;
            };
            let admin_url = if let Ok(url) = std::env::var("POSTGRES_TEST_ADMIN_URL") {
                let trimmed = url.trim();
                if trimmed.is_empty() {
                    format!("{base}/postgres")
                } else {
                    trimmed.to_string()
                }
            } else {
                format!("{base}/postgres")
            };
            let database_name = format!(
                "agentdesk_pipeline_override_{}",
                uuid::Uuid::new_v4().simple()
            );
            let database_url = format!("{base}/{database_name}");
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
    /// #5718 r3 (R3): both layers of the pair hold a row this build cannot read,
    /// which is exactly when an operator needs to write. The cross-layer check
    /// has nothing to compare against, so it is skipped and the repair lands —
    /// including the `null` that clears the row. Blocking it made the pair
    /// unfixable from either side.
    async fn repo_write_repairs_the_pair_when_the_existing_agent_override_is_unreadable() {
        let Some(pg_db) = TestPostgresDb::create().await else {
            return;
        };
        let Some(pool) = pg_db.connect_with_minimal_schema().await else {
            pg_db.drop().await;
            return;
        };
        crate::pipeline::ensure_loaded();
        seed_agent(&pool, "agent-1720-a", Some(r#"{"states":["broken"]}"#)).await;
        seed_repo_with_default_agent(
            &pool,
            "repo-1720-a",
            "agent-1720-a",
            Some(r#"{"states":["broken"]}"#),
        )
        .await;
        seed_card(
            &pool,
            "card-1720-a",
            Some("repo-1720-a"),
            Some("agent-1720-a"),
        )
        .await;

        let service = PipelineOverrideService::new(&pool);
        service
            .set_repo_pipeline("repo-1720-a", Some(&valid_repo_override()))
            .await
            .expect("an unreadable agent row must not block the repo repair");

        let stored: Option<String> = sqlx::query_scalar(
            "SELECT pipeline_config::text FROM github_repos WHERE id = 'repo-1720-a'",
        )
        .fetch_one(&pool)
        .await
        .expect("repo pipeline_config lookup");
        let stored = stored.expect("the repaired repo override must be stored");
        assert!(
            stored.contains("OnCardTransition"),
            "the written override must replace the unreadable row, got: {stored}"
        );

        service
            .set_repo_pipeline("repo-1720-a", None)
            .await
            .expect("clearing the row with null must not be blocked either");
        let cleared: Option<String> = sqlx::query_scalar(
            "SELECT pipeline_config::text FROM github_repos WHERE id = 'repo-1720-a'",
        )
        .fetch_one(&pool)
        .await
        .expect("repo pipeline_config lookup");
        assert!(
            cleared.is_none(),
            "null must clear the repo override; got {cleared:?}"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    /// #5718 r3 (R1/R3): the paired repo row carries a key this build does not
    /// declare. It is still read — with that key dropped — so the cross-layer
    /// check runs on the sections that survive, and the agent write lands
    /// instead of 400ing on a row the operator cannot reach from here.
    async fn agent_write_is_not_blocked_by_an_undeclared_key_in_the_repo_override() {
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
            Some(r#"{"stage_failure_policy":{"default":"fail"}}"#),
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
        service
            .set_agent_pipeline("agent-1720-b", Some(&valid_agent_override()))
            .await
            .expect("an undeclared key in the repo row must not block the agent write");

        let stored: Option<String> = sqlx::query_scalar(
            "SELECT pipeline_config::text FROM agents WHERE id = 'agent-1720-b'",
        )
        .fetch_one(&pool)
        .await
        .expect("agent pipeline_config lookup");
        let stored = stored.expect("the agent override must be stored");
        assert!(
            stored.contains("OnCardTransition"),
            "the written override must be the one stored, got: {stored}"
        );

        pool.close().await;
        pg_db.drop().await;
    }
}
