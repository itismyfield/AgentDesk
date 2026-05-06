use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::json;
use sqlx::Row;

use crate::services as services_layer;

use super::AppState;

// ── Body types ─────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct DecisionItem {
    pub item_id: i64,
    pub decision: String,
}

#[derive(Debug, Deserialize)]
pub struct UpdateDecisionsBody {
    pub decisions: Vec<DecisionItem>,
}

#[derive(Debug, Deserialize)]
pub struct ReviewTargetRecoveryBody {
    pub dispatch_id: Option<String>,
    pub card_id: Option<String>,
    pub target_commit: Option<String>,
    pub worktree_path: Option<String>,
    pub reason: Option<String>,
}

#[derive(Debug)]
struct ReviewRecoveryDispatch {
    id: String,
    card_id: String,
    status: String,
    context: serde_json::Value,
}

fn validate_review_decision(decision: &str) -> bool {
    decision == "accept" || decision == "reject"
}

fn trimmed_optional(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn json_object_from_text(
    raw: Option<String>,
) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    let Some(raw) = raw.filter(|value| !value.trim().is_empty()) else {
        return Ok(serde_json::Map::new());
    };
    let value: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|error| format!("dispatch context is not valid JSON: {error}"))?;
    value
        .as_object()
        .cloned()
        .ok_or_else(|| "dispatch context must be a JSON object".to_string())
}

async fn load_review_recovery_dispatch_pg(
    pool: &sqlx::PgPool,
    dispatch_id: Option<&str>,
    card_id: Option<&str>,
) -> Result<Option<ReviewRecoveryDispatch>, String> {
    let row = if let Some(dispatch_id) = dispatch_id {
        sqlx::query(
            "SELECT id, kanban_card_id, dispatch_type, status, context
             FROM task_dispatches
             WHERE id = $1",
        )
        .bind(dispatch_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("load review dispatch {dispatch_id}: {error}"))?
    } else if let Some(card_id) = card_id {
        sqlx::query(
            "SELECT id, kanban_card_id, dispatch_type, status, context
             FROM task_dispatches
             WHERE kanban_card_id = $1
               AND dispatch_type = 'review'
               AND status IN ('pending', 'dispatched', 'failed')
             ORDER BY updated_at DESC, created_at DESC, id DESC
             LIMIT 1",
        )
        .bind(card_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("load latest review dispatch for card {card_id}: {error}"))?
    } else {
        return Err("provide dispatch_id or card_id".to_string());
    };

    let Some(row) = row else {
        return Ok(None);
    };

    let loaded_dispatch_id: String = row
        .try_get("id")
        .map_err(|error| format!("decode dispatch id: {error}"))?;
    let loaded_card_id: Option<String> = row
        .try_get("kanban_card_id")
        .map_err(|error| format!("decode dispatch card id: {error}"))?;
    let Some(loaded_card_id) = loaded_card_id.filter(|value| !value.trim().is_empty()) else {
        return Err(format!(
            "dispatch {loaded_dispatch_id} is not attached to a card"
        ));
    };
    if let Some(expected_card_id) = card_id {
        if expected_card_id != loaded_card_id {
            return Err(format!(
                "dispatch {loaded_dispatch_id} belongs to card {loaded_card_id}, not {expected_card_id}"
            ));
        }
    }

    let dispatch_type: Option<String> = row
        .try_get("dispatch_type")
        .map_err(|error| format!("decode dispatch type: {error}"))?;
    if dispatch_type.as_deref() != Some("review") {
        return Err(format!(
            "review recovery only accepts review dispatches, got {}",
            dispatch_type.unwrap_or_else(|| "<none>".to_string())
        ));
    }

    let status: String = row
        .try_get("status")
        .map_err(|error| format!("decode dispatch status: {error}"))?;
    let context = serde_json::Value::Object(json_object_from_text(
        row.try_get("context")
            .map_err(|error| format!("decode dispatch context: {error}"))?,
    )?);

    Ok(Some(ReviewRecoveryDispatch {
        id: loaded_dispatch_id,
        card_id: loaded_card_id,
        status,
        context,
    }))
}

fn worktree_head(path: &str) -> Result<String, String> {
    if !std::path::Path::new(path).is_dir() {
        return Err(format!(
            "worktree_path does not exist or is not a directory: {path}"
        ));
    }
    services_layer::platform::git_head_commit(path)
        .ok_or_else(|| format!("cannot resolve git HEAD for worktree_path: {path}"))
}

fn branch_for_recovery_target(path: Option<&str>, commit: &str) -> Option<String> {
    let path = path?;
    let preferred = services_layer::platform::shell::git_branch_name(path);
    services_layer::platform::shell::git_branch_containing_commit(
        path,
        commit,
        preferred.as_deref(),
        None,
    )
    .or(preferred)
}

fn context_string_field<'a>(context: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    context
        .get(key)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

async fn active_other_review_dispatch_exists_pg(
    pool: &sqlx::PgPool,
    card_id: &str,
    dispatch_id: &str,
) -> Result<bool, String> {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(
             SELECT 1
             FROM task_dispatches
             WHERE kanban_card_id = $1
               AND id <> $2
               AND dispatch_type = 'review'
               AND status IN ('pending', 'dispatched')
         )",
    )
    .bind(card_id)
    .bind(dispatch_id)
    .fetch_one(pool)
    .await
    .map_err(|error| format!("check active review dispatch conflict: {error}"))
}

async fn recover_review_target_pg(
    pool: &sqlx::PgPool,
    body: ReviewTargetRecoveryBody,
) -> Result<serde_json::Value, (StatusCode, String)> {
    let dispatch_id = trimmed_optional(body.dispatch_id);
    let card_id = trimmed_optional(body.card_id);
    let worktree_path = trimmed_optional(body.worktree_path);
    let requested_commit = trimmed_optional(body.target_commit);
    let reason = trimmed_optional(body.reason)
        .unwrap_or_else(|| "manual review target recovery".to_string());

    let dispatch =
        load_review_recovery_dispatch_pg(pool, dispatch_id.as_deref(), card_id.as_deref())
            .await
            .map_err(|error| (StatusCode::BAD_REQUEST, error))?
            .ok_or_else(|| {
                (
                    StatusCode::NOT_FOUND,
                    "review dispatch not found".to_string(),
                )
            })?;

    let mut context_obj = dispatch.context.as_object().cloned().unwrap_or_default();
    let inferred_worktree_commit = if requested_commit.is_none() {
        worktree_path
            .as_deref()
            .map(worktree_head)
            .transpose()
            .map_err(|error| (StatusCode::BAD_REQUEST, error))?
    } else {
        None
    };
    let existing_commit =
        context_string_field(&dispatch.context, "reviewed_commit").map(str::to_string);
    let target_commit = requested_commit
        .or(inferred_worktree_commit)
        .or(existing_commit)
        .ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                "provide target_commit, worktree_path, or recover a dispatch with reviewed_commit in context".to_string(),
            )
        })?;

    if let Some(path) = worktree_path.as_deref() {
        let head = worktree_head(path).map_err(|error| (StatusCode::BAD_REQUEST, error))?;
        if head != target_commit {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("worktree_path HEAD {head} does not match target_commit {target_commit}"),
            ));
        }
    }

    let validation_repo = worktree_path
        .as_deref()
        .or_else(|| context_string_field(&dispatch.context, "target_repo"))
        .or_else(|| context_string_field(&dispatch.context, "worktree_path"));
    let belongs = crate::dispatch::commit_belongs_to_card_issue_pg(
        pool,
        &dispatch.card_id,
        &target_commit,
        validation_repo,
    )
    .await;
    if !belongs {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            format!(
                "target_commit {target_commit} does not reference or belong to card {}",
                dispatch.card_id
            ),
        ));
    }

    if dispatch.status == "failed"
        && active_other_review_dispatch_exists_pg(pool, &dispatch.card_id, &dispatch.id)
            .await
            .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error))?
    {
        return Err((
            StatusCode::CONFLICT,
            format!(
                "cannot requeue failed review dispatch {} while another active review dispatch exists for card {}",
                dispatch.id, dispatch.card_id
            ),
        ));
    }

    let previous_context = dispatch.context.clone();
    context_obj.insert("reviewed_commit".to_string(), json!(target_commit));
    if let Some(path) = worktree_path.as_deref() {
        context_obj.insert("worktree_path".to_string(), json!(path));
    } else if let Some(existing_path) = context_string_field(&dispatch.context, "worktree_path") {
        match worktree_head(existing_path) {
            Ok(head) if head == target_commit => {}
            _ => {
                context_obj.remove("worktree_path");
            }
        }
    }
    if let Some(branch) = branch_for_recovery_target(worktree_path.as_deref(), &target_commit) {
        context_obj.insert("branch".to_string(), json!(branch));
    }
    let cleared_markers = [
        context_obj.remove("review_target_reject_reason").is_some(),
        context_obj.remove("review_target_warning").is_some(),
    ]
    .into_iter()
    .filter(|removed| *removed)
    .count();

    let new_context = serde_json::Value::Object(context_obj);
    let from_status = dispatch.status.clone();
    let to_status = if dispatch.status == "failed" {
        "pending"
    } else {
        dispatch.status.as_str()
    };
    let payload = json!({
        "reason": reason,
        "previous_context": previous_context,
        "new_context": new_context,
        "cleared_failure_markers": cleared_markers,
        "target_commit": context_string_field(&new_context, "reviewed_commit"),
        "worktree_path": context_string_field(&new_context, "worktree_path"),
    });

    let mut tx = pool.begin().await.map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("begin recovery transaction: {error}"),
        )
    })?;

    sqlx::query(
        "UPDATE task_dispatches
         SET context = $1,
             status = $2,
             result = CASE WHEN status = 'failed' THEN NULL ELSE result END,
             completed_at = CASE WHEN status = 'failed' THEN NULL ELSE completed_at END,
             updated_at = NOW()
         WHERE id = $3",
    )
    .bind(new_context.to_string())
    .bind(to_status)
    .bind(&dispatch.id)
    .execute(&mut *tx)
    .await
    .map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("update review dispatch target: {error}"),
        )
    })?;

    sqlx::query(
        "INSERT INTO dispatch_events (
            dispatch_id,
            kanban_card_id,
            dispatch_type,
            from_status,
            to_status,
            transition_source,
            payload_json
         ) VALUES ($1, $2, 'review', $3, $4, 'manual_review_target_recovery', $5)",
    )
    .bind(&dispatch.id)
    .bind(&dispatch.card_id)
    .bind(&from_status)
    .bind(to_status)
    .bind(payload.clone())
    .execute(&mut *tx)
    .await
    .map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("insert dispatch recovery event: {error}"),
        )
    })?;

    sqlx::query(
        "INSERT INTO audit_logs (entity_type, entity_id, action, actor)
         VALUES ('task_dispatch', $1, 'review_target_recovered', 'operator')",
    )
    .bind(&dispatch.id)
    .execute(&mut *tx)
    .await
    .map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("insert recovery audit log: {error}"),
        )
    })?;

    tx.commit().await.map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("commit recovery transaction: {error}"),
        )
    })?;

    Ok(json!({
        "ok": true,
        "dispatch_id": dispatch.id,
        "card_id": dispatch.card_id,
        "from_status": from_status,
        "to_status": to_status,
        "target": {
            "reviewed_commit": context_string_field(&new_context, "reviewed_commit"),
            "worktree_path": context_string_field(&new_context, "worktree_path"),
            "branch": context_string_field(&new_context, "branch"),
        },
        "cleared_failure_markers": cleared_markers,
    }))
}

async fn update_decisions_pg(
    pool: &sqlx::PgPool,
    dispatch_id: &str,
    decisions: &[DecisionItem],
) -> Result<Vec<serde_json::Value>, String> {
    for item in decisions {
        let affected = sqlx::query(
            "UPDATE review_decisions
             SET decision = $1, decided_at = NOW()
             WHERE dispatch_id = $2 AND id = $3",
        )
        .bind(&item.decision)
        .bind(dispatch_id)
        .bind(item.item_id)
        .execute(pool)
        .await
        .map_err(|error| format!("update review_decisions: {error}"))?
        .rows_affected();

        if affected == 0 {
            sqlx::query(
                "INSERT INTO review_decisions (id, dispatch_id, decision, decided_at)
                 VALUES ($1, $2, $3, NOW())
                 ON CONFLICT (id) DO UPDATE SET
                    dispatch_id = EXCLUDED.dispatch_id,
                    decision = EXCLUDED.decision,
                    decided_at = EXCLUDED.decided_at",
            )
            .bind(item.item_id)
            .bind(dispatch_id)
            .bind(&item.decision)
            .execute(pool)
            .await
            .map_err(|error| format!("upsert review_decisions: {error}"))?;
        }
    }

    let rows = sqlx::query(
        "SELECT id, kanban_card_id, dispatch_id, item_index::BIGINT AS item_index, decision, decided_at::text AS decided_at
         FROM review_decisions
         WHERE dispatch_id = $1
         ORDER BY id",
    )
    .bind(dispatch_id)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("load review_decisions: {error}"))?;

    Ok(rows
        .into_iter()
        .map(|row| {
            json!({
                "id": row.try_get::<i64, _>("id").ok(),
                "kanban_card_id": row.try_get::<Option<String>, _>("kanban_card_id").ok().flatten(),
                "dispatch_id": row.try_get::<Option<String>, _>("dispatch_id").ok().flatten(),
                "item_index": row.try_get::<Option<i64>, _>("item_index").ok().flatten(),
                "decision": row.try_get::<Option<String>, _>("decision").ok().flatten(),
                "decided_at": row.try_get::<Option<String>, _>("decided_at").ok().flatten(),
            })
        })
        .collect())
}

async fn resolve_review_card_id_pg(
    pool: &sqlx::PgPool,
    dispatch_id: &str,
) -> Result<Option<String>, String> {
    if let Some(row) = sqlx::query(
        "SELECT kanban_card_id
         FROM review_decisions
         WHERE dispatch_id = $1
         LIMIT 1",
    )
    .bind(dispatch_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load review decision card: {error}"))?
    {
        let card_id = row
            .try_get::<Option<String>, _>("kanban_card_id")
            .map_err(|error| format!("decode review decision card: {error}"))?;
        if card_id.is_some() {
            return Ok(card_id);
        }
    }

    let row = sqlx::query(
        "SELECT kanban_card_id
         FROM task_dispatches
         WHERE id = $1",
    )
    .bind(dispatch_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load dispatch card: {error}"))?;

    match row {
        Some(row) => row
            .try_get::<Option<String>, _>("kanban_card_id")
            .map_err(|error| format!("decode dispatch card: {error}")),
        None => Ok(None),
    }
}

// ── Handlers ───────────────────────────────────────────────────

/// PATCH /api/kanban-reviews/:id/decisions
pub async fn update_decisions(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateDecisionsBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    for item in &body.decisions {
        if !validate_review_decision(&item.decision) {
            return (
                StatusCode::BAD_REQUEST,
                Json(
                    json!({"error": format!("invalid decision '{}', must be 'accept' or 'reject'", item.decision)}),
                ),
            );
        }
    }

    let Some(pg_pool) = state.pg_pool_ref() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "postgres pool unavailable"})),
        );
    };

    match update_decisions_pg(pg_pool, &id, &body.decisions).await {
        Ok(decisions) => (
            StatusCode::OK,
            Json(json!({"review": {"dispatch_id": id, "decisions": decisions}})),
        ),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        ),
    }
}

/// POST /api/reviews/recovery
pub async fn recover_review_target(
    State(state): State<AppState>,
    Json(body): Json<ReviewTargetRecoveryBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pg_pool) = state.pg_pool_ref() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "postgres pool unavailable"})),
        );
    };

    match recover_review_target_pg(pg_pool, body).await {
        Ok(value) => (StatusCode::OK, Json(value)),
        Err((status, error)) => (status, Json(json!({"error": error}))),
    }
}

/// POST /api/kanban-reviews/:id/trigger-rework
pub async fn trigger_rework(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pg_pool) = state.pg_pool_ref() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "postgres pool unavailable"})),
        );
    };

    let card_id = match resolve_review_card_id_pg(pg_pool, &id).await {
        Ok(Some(card_id)) => card_id,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "review or dispatch not found"})),
            );
        }
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            );
        }
    };

    match crate::kanban::transition_status_with_opts_pg_only(
        pg_pool,
        &state.engine,
        &card_id,
        "in_progress",
        "trigger-rework",
        crate::engine::transition::ForceIntent::OperatorOverride,
    )
    .await
    {
        Ok(_) => (StatusCode::OK, Json(json!({"ok": true}))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        ),
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::engine::PolicyEngine;
    use std::sync::Arc;

    fn test_db() -> Db {
        let conn = sqlite_test::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        crate::db::wrap_conn(conn)
    }

    fn test_engine(db: &Db) -> PolicyEngine {
        let config = crate::config::Config::default();
        PolicyEngine::new_with_legacy_db(&config, db.clone()).unwrap()
    }

    fn test_state_with_pg(db: Db, engine: PolicyEngine, pg_pool: sqlx::PgPool) -> AppState {
        let tx = crate::server::ws::new_broadcast();
        let buf = crate::server::ws::spawn_batch_flusher(tx.clone());
        AppState {
            legacy_db_override: Some(db),
            pg_pool: Some(pg_pool),
            engine,
            config: Arc::new(crate::config::Config::default()),
            broadcast_tx: tx,
            batch_buffer: buf,
            health_registry: None,
            cluster_instance_id: None,
        }
    }

    struct TestPostgresDb {
        _lock: crate::db::postgres::PostgresTestLifecycleGuard,
        admin_url: String,
        database_name: String,
        database_url: String,
    }

    impl TestPostgresDb {
        async fn create() -> Self {
            let lock = crate::db::postgres::lock_test_lifecycle();
            let admin_url = postgres_admin_database_url();
            let database_name = format!("agentdesk_reviews_{}", uuid::Uuid::new_v4().simple());
            let database_url = format!("{}/{}", postgres_base_database_url(), database_name);
            crate::db::postgres::create_test_database(&admin_url, &database_name, "reviews tests")
                .await
                .unwrap();
            Self {
                _lock: lock,
                admin_url,
                database_name,
                database_url,
            }
        }

        async fn migrate(&self) -> sqlx::PgPool {
            crate::db::postgres::connect_test_pool_and_migrate(&self.database_url, "reviews tests")
                .await
                .unwrap()
        }

        async fn drop(self) {
            crate::db::postgres::drop_test_database(
                &self.admin_url,
                &self.database_name,
                "reviews tests",
            )
            .await
            .unwrap();
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
            .unwrap_or_else(|| "localhost".to_string());
        let port = std::env::var("PGPORT")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "5432".to_string());

        match password {
            Some(password) => format!("postgresql://{user}:{password}@{host}:{port}"),
            None => format!("postgresql://{user}@{host}:{port}"),
        }
    }

    fn postgres_admin_database_url() -> String {
        let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "postgres".to_string());
        format!("{}/{}", postgres_base_database_url(), admin_db)
    }

    fn run_git(repo: &std::path::Path, args: &[&str]) -> String {
        let output = services_layer::git::git_command()
            .args(args)
            .current_dir(repo)
            .output()
            .unwrap_or_else(|error| panic!("run git {args:?}: {error}"));
        if !output.status.success() {
            panic!(
                "git {:?} failed: {}\n{}",
                args,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn init_repo_with_issue_commit(issue_number: i64) -> (tempfile::TempDir, String) {
        let tempdir = tempfile::tempdir().unwrap();
        let repo = tempdir.path();
        run_git(repo, &["init"]);
        run_git(repo, &["config", "user.email", "agentdesk@example.invalid"]);
        run_git(repo, &["config", "user.name", "AgentDesk Test"]);
        std::fs::write(repo.join("README.md"), format!("issue {issue_number}\n")).unwrap();
        run_git(repo, &["add", "README.md"]);
        run_git(
            repo,
            &[
                "commit",
                "-m",
                &format!("fix: recovery target (#{issue_number})"),
            ],
        );
        let commit = run_git(repo, &["rev-parse", "HEAD"]);
        (tempdir, commit)
    }

    async fn seed_review_recovery_card_and_dispatch(
        pg_pool: &sqlx::PgPool,
        card_id: &str,
        dispatch_id: &str,
        issue_number: i64,
        status: &str,
    ) {
        sqlx::query(
            "INSERT INTO kanban_cards (
                id, title, status, review_status, latest_dispatch_id, github_issue_number,
                created_at, updated_at
             ) VALUES ($1, $2, 'review', 'reviewing', $3, $4, NOW(), NOW())",
        )
        .bind(card_id)
        .bind(format!("Issue {issue_number}"))
        .bind(dispatch_id)
        .bind(issue_number)
        .execute(pg_pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result,
                created_at, updated_at
             ) VALUES ($1, $2, 'project-agentdesk', 'review', $3, 'Review dispatch', $4, $5, NOW(), NOW())",
        )
        .bind(dispatch_id)
        .bind(card_id)
        .bind(status)
        .bind(
            json!({
                "reviewed_commit": "1111111111111111111111111111111111111111",
                "worktree_path": "/tmp/stale-review-target",
                "review_target_reject_reason": "latest_work_target_issue_mismatch",
                "review_target_warning": "stale target"
            })
            .to_string(),
        )
        .bind(json!({"error": "stale target"}).to_string())
        .execute(pg_pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn update_decisions_pg_round_trip() {
        let pg_db = TestPostgresDb::create().await;
        let pg_pool = pg_db.migrate().await;
        let sqlite_db = test_db();
        let engine = test_engine(&sqlite_db);
        let state = test_state_with_pg(sqlite_db, engine, pg_pool.clone());

        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, created_at, updated_at)
             VALUES ($1, $2, $3, NOW(), NOW())",
        )
        .bind("card-1")
        .bind("Review card")
        .bind("review")
        .execute(&pg_pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO review_decisions (id, kanban_card_id, dispatch_id, item_index, decision)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(1_i64)
        .bind("card-1")
        .bind("dispatch-1")
        .bind(0_i64)
        .bind("reject")
        .execute(&pg_pool)
        .await
        .unwrap();

        let (status, Json(body)) = update_decisions(
            State(state),
            Path("dispatch-1".to_string()),
            Json(UpdateDecisionsBody {
                decisions: vec![
                    DecisionItem {
                        item_id: 1,
                        decision: "accept".to_string(),
                    },
                    DecisionItem {
                        item_id: 2,
                        decision: "reject".to_string(),
                    },
                ],
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["review"]["dispatch_id"], "dispatch-1");
        assert_eq!(body["review"]["decisions"][0]["decision"], "accept");
        assert_eq!(body["review"]["decisions"][1]["decision"], "reject");

        let stored: Vec<(i64, Option<String>)> = sqlx::query_as(
            "SELECT id, decision
             FROM review_decisions
             WHERE dispatch_id = $1
             ORDER BY id",
        )
        .bind("dispatch-1")
        .fetch_all(&pg_pool)
        .await
        .unwrap();
        assert_eq!(
            stored,
            vec![
                (1, Some("accept".to_string())),
                (2, Some("reject".to_string()))
            ]
        );

        pg_pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn trigger_rework_pg_only_transitions_review_card_to_in_progress() {
        let pg_db = TestPostgresDb::create().await;
        let pg_pool = pg_db.migrate().await;
        let sqlite_db = test_db();
        let engine = test_engine(&sqlite_db);
        let state = test_state_with_pg(sqlite_db, engine, pg_pool.clone());

        sqlx::query(
            "INSERT INTO kanban_cards (
                id, title, status, review_status, latest_dispatch_id, created_at, updated_at
             ) VALUES ($1, $2, $3, $4, $5, NOW(), NOW())",
        )
        .bind("card-review-1")
        .bind("Review card")
        .bind("review")
        .bind("reviewing")
        .bind("dispatch-review-1")
        .execute(&pg_pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
             ) VALUES ($1, $2, $3, $4, $5, $6, NOW(), NOW())",
        )
        .bind("dispatch-review-1")
        .bind("card-review-1")
        .bind("project-agentdesk")
        .bind("review")
        .bind("completed")
        .bind("Review dispatch")
        .execute(&pg_pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO review_decisions (id, kanban_card_id, dispatch_id, item_index, decision)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(1_i64)
        .bind("card-review-1")
        .bind("dispatch-review-1")
        .bind(0_i64)
        .bind("reject")
        .execute(&pg_pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO card_review_state (
                card_id, state, review_round, last_verdict, review_entered_at, updated_at
             ) VALUES ($1, $2, $3, $4, NOW(), NOW())",
        )
        .bind("card-review-1")
        .bind("reviewing")
        .bind(1_i64)
        .bind("reject")
        .execute(&pg_pool)
        .await
        .unwrap();

        let (status, Json(body)) =
            trigger_rework(State(state), Path("dispatch-review-1".to_string())).await;

        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["ok"], true);

        let card_row = sqlx::query(
            "SELECT status, started_at IS NOT NULL AS has_started_at
             FROM kanban_cards
             WHERE id = $1",
        )
        .bind("card-review-1")
        .fetch_one(&pg_pool)
        .await
        .unwrap();
        let status_text: String = card_row.try_get("status").unwrap();
        let has_started_at: bool = card_row.try_get("has_started_at").unwrap();
        assert_eq!(status_text, "in_progress");
        assert!(
            has_started_at,
            "started_at should be set on in_progress entry"
        );

        let review_state_row = sqlx::query(
            "SELECT state, last_verdict
             FROM card_review_state
             WHERE card_id = $1",
        )
        .bind("card-review-1")
        .fetch_one(&pg_pool)
        .await
        .unwrap();
        let review_state: String = review_state_row.try_get("state").unwrap();
        let last_verdict: Option<String> = review_state_row.try_get("last_verdict").unwrap();
        assert_eq!(review_state, "reviewing");
        assert!(
            last_verdict.is_none(),
            "rework entry should clear last verdict"
        );

        let audit_log_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::BIGINT
             FROM kanban_audit_logs
             WHERE card_id = $1 AND from_status = $2 AND to_status = $3",
        )
        .bind("card-review-1")
        .bind("review")
        .bind("in_progress")
        .fetch_one(&pg_pool)
        .await
        .unwrap();
        assert_eq!(audit_log_count, 1);

        pg_pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn recover_review_target_updates_context_requeues_failed_dispatch_and_audits() {
        let pg_db = TestPostgresDb::create().await;
        let pg_pool = pg_db.migrate().await;
        let sqlite_db = test_db();
        let engine = test_engine(&sqlite_db);
        let state = test_state_with_pg(sqlite_db, engine, pg_pool.clone());
        let (repo, commit) = init_repo_with_issue_commit(1874);

        seed_review_recovery_card_and_dispatch(
            &pg_pool,
            "card-review-recover",
            "dispatch-review-recover",
            1874,
            "failed",
        )
        .await;

        let (status, Json(body)) = recover_review_target(
            State(state),
            Json(ReviewTargetRecoveryBody {
                dispatch_id: Some("dispatch-review-recover".to_string()),
                card_id: None,
                target_commit: Some(commit.clone()),
                worktree_path: Some(repo.path().display().to_string()),
                reason: Some("operator corrected stale cwd".to_string()),
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["ok"], true);
        assert_eq!(body["from_status"], "failed");
        assert_eq!(body["to_status"], "pending");
        assert_eq!(body["cleared_failure_markers"], 2);

        let row = sqlx::query(
            "SELECT status, context, result
             FROM task_dispatches
             WHERE id = $1",
        )
        .bind("dispatch-review-recover")
        .fetch_one(&pg_pool)
        .await
        .unwrap();
        let stored_status: String = row.try_get("status").unwrap();
        let stored_context_raw: String = row.try_get("context").unwrap();
        let stored_result: Option<String> = row.try_get("result").unwrap();
        let stored_context: serde_json::Value = serde_json::from_str(&stored_context_raw).unwrap();
        assert_eq!(stored_status, "pending");
        assert!(
            stored_result.is_none(),
            "failed result marker should be cleared"
        );
        assert_eq!(stored_context["reviewed_commit"], commit);
        assert_eq!(
            stored_context["worktree_path"],
            repo.path().display().to_string()
        );
        assert!(stored_context.get("review_target_reject_reason").is_none());
        assert!(stored_context.get("review_target_warning").is_none());

        let dispatch_event_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::BIGINT
             FROM dispatch_events
             WHERE dispatch_id = $1
               AND transition_source = 'manual_review_target_recovery'",
        )
        .bind("dispatch-review-recover")
        .fetch_one(&pg_pool)
        .await
        .unwrap();
        assert_eq!(dispatch_event_count, 1);

        let audit_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::BIGINT
             FROM audit_logs
             WHERE entity_type = 'task_dispatch'
               AND entity_id = $1
               AND action = 'review_target_recovered'",
        )
        .bind("dispatch-review-recover")
        .fetch_one(&pg_pool)
        .await
        .unwrap();
        assert_eq!(audit_count, 1);

        pg_pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn recover_review_target_rejects_commit_for_different_issue() {
        let pg_db = TestPostgresDb::create().await;
        let pg_pool = pg_db.migrate().await;
        let sqlite_db = test_db();
        let engine = test_engine(&sqlite_db);
        let state = test_state_with_pg(sqlite_db, engine, pg_pool.clone());
        let (repo, wrong_commit) = init_repo_with_issue_commit(9999);

        seed_review_recovery_card_and_dispatch(
            &pg_pool,
            "card-review-reject",
            "dispatch-review-reject",
            1874,
            "failed",
        )
        .await;

        let (status, Json(body)) = recover_review_target(
            State(state),
            Json(ReviewTargetRecoveryBody {
                dispatch_id: Some("dispatch-review-reject".to_string()),
                card_id: None,
                target_commit: Some(wrong_commit),
                worktree_path: Some(repo.path().display().to_string()),
                reason: Some("operator attempted wrong target".to_string()),
            }),
        )
        .await;

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .contains("does not reference or belong")
        );

        let stored_status: String =
            sqlx::query_scalar("SELECT status FROM task_dispatches WHERE id = $1")
                .bind("dispatch-review-reject")
                .fetch_one(&pg_pool)
                .await
                .unwrap();
        assert_eq!(stored_status, "failed");

        let event_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::BIGINT
             FROM dispatch_events
             WHERE dispatch_id = $1",
        )
        .bind("dispatch-review-reject")
        .fetch_one(&pg_pool)
        .await
        .unwrap();
        assert_eq!(event_count, 0);

        pg_pool.close().await;
        pg_db.drop().await;
    }
}
