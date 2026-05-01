use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgPool, Row};

#[derive(Debug, Clone, Serialize)]
pub struct TestPhaseRun {
    pub id: String,
    pub idempotency_key: String,
    pub phase_key: String,
    pub head_sha: String,
    pub status: String,
    pub issue_id: Option<String>,
    pub card_id: Option<String>,
    pub repo_id: Option<String>,
    pub required_capabilities: Value,
    pub resource_lock_key: Option<String>,
    pub holder_instance_id: Option<String>,
    pub holder_job_id: Option<String>,
    pub evidence: Value,
    pub error: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TestPhaseRunRequest {
    pub phase_key: String,
    pub head_sha: String,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub idempotency_key: Option<String>,
    #[serde(default)]
    pub issue_id: Option<String>,
    #[serde(default)]
    pub card_id: Option<String>,
    #[serde(default)]
    pub repo_id: Option<String>,
    #[serde(default)]
    pub required_capabilities: Option<Value>,
    #[serde(default)]
    pub resource_lock_key: Option<String>,
    #[serde(default)]
    pub holder_instance_id: Option<String>,
    #[serde(default)]
    pub holder_job_id: Option<String>,
    #[serde(default)]
    pub evidence: Option<Value>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TestPhaseRunListQuery {
    #[serde(default)]
    pub phase_key: Option<String>,
    #[serde(default)]
    pub head_sha: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TestPhaseEvidenceQuery {
    pub phase_key: String,
    pub head_sha: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TestPhaseRunStartRequest {
    #[serde(flatten)]
    pub run: TestPhaseRunRequest,
    pub resource_lock_key: String,
    pub holder_instance_id: String,
    pub holder_job_id: String,
    #[serde(default)]
    pub ttl_secs: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TestPhaseRunStartOutcome {
    pub started: bool,
    pub run: Option<TestPhaseRun>,
    pub lock: Option<crate::server::resource_locks::ResourceLock>,
    pub current_lock: Option<crate::server::resource_locks::ResourceLock>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TestPhaseRunCompleteRequest {
    #[serde(flatten)]
    pub run: TestPhaseRunRequest,
    #[serde(default)]
    pub release_lock: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct TestPhaseRunCompleteOutcome {
    pub run: TestPhaseRun,
    pub lock_released: Option<bool>,
}

pub async fn upsert_test_phase_run(
    pool: &PgPool,
    request: &TestPhaseRunRequest,
) -> Result<TestPhaseRun, String> {
    let phase_key = normalize_required("phase_key", &request.phase_key)?;
    let head_sha = normalize_required("head_sha", &request.head_sha)?;
    let status = normalize_status(request.status.as_deref().unwrap_or("queued"))?;
    let idempotency_key = request
        .idempotency_key
        .as_deref()
        .map(|value| normalize_required("idempotency_key", value))
        .transpose()?
        .unwrap_or_else(|| deterministic_idempotency_key(&phase_key, &head_sha));
    let run_id = format!("tpr-{}", uuid::Uuid::new_v4());
    let terminal = is_terminal_status(&status);
    let required_capabilities = request
        .required_capabilities
        .clone()
        .unwrap_or_else(|| json!({}));
    let evidence = request.evidence.clone().unwrap_or_else(|| json!({}));

    let row = sqlx::query(
        r#"
        INSERT INTO test_phase_runs (
            id, idempotency_key, phase_key, head_sha, status, issue_id, card_id,
            repo_id, required_capabilities, resource_lock_key, holder_instance_id,
            holder_job_id, evidence, error, started_at, completed_at, created_at, updated_at
        )
        VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14,
            CASE WHEN $5 = 'running' THEN NOW() ELSE NULL END,
            CASE WHEN $15 THEN NOW() ELSE NULL END,
            NOW(), NOW()
        )
        ON CONFLICT (idempotency_key) DO UPDATE
        SET status = EXCLUDED.status,
            issue_id = COALESCE(EXCLUDED.issue_id, test_phase_runs.issue_id),
            card_id = COALESCE(EXCLUDED.card_id, test_phase_runs.card_id),
            repo_id = COALESCE(EXCLUDED.repo_id, test_phase_runs.repo_id),
            required_capabilities = EXCLUDED.required_capabilities,
            resource_lock_key = COALESCE(EXCLUDED.resource_lock_key, test_phase_runs.resource_lock_key),
            holder_instance_id = COALESCE(EXCLUDED.holder_instance_id, test_phase_runs.holder_instance_id),
            holder_job_id = COALESCE(EXCLUDED.holder_job_id, test_phase_runs.holder_job_id),
            evidence = EXCLUDED.evidence,
            error = EXCLUDED.error,
            started_at = COALESCE(test_phase_runs.started_at, EXCLUDED.started_at),
            completed_at = CASE WHEN $15 THEN NOW() ELSE NULL END,
            updated_at = NOW()
        RETURNING id, idempotency_key, phase_key, head_sha, status, issue_id,
                  card_id, repo_id, required_capabilities, resource_lock_key,
                  holder_instance_id, holder_job_id, evidence, error, started_at,
                  completed_at, created_at, updated_at
        "#,
    )
    .bind(run_id)
    .bind(idempotency_key)
    .bind(phase_key)
    .bind(head_sha)
    .bind(status)
    .bind(clean_optional(request.issue_id.as_deref()))
    .bind(clean_optional(request.card_id.as_deref()))
    .bind(clean_optional(request.repo_id.as_deref()))
    .bind(required_capabilities)
    .bind(clean_optional(request.resource_lock_key.as_deref()))
    .bind(clean_optional(request.holder_instance_id.as_deref()))
    .bind(clean_optional(request.holder_job_id.as_deref()))
    .bind(evidence)
    .bind(clean_optional(request.error.as_deref()))
    .bind(terminal)
    .fetch_one(pool)
    .await
    .map_err(|error| format!("upsert test phase run: {error}"))?;

    run_from_row(row)
}

pub async fn start_test_phase_run(
    pool: &PgPool,
    request: &TestPhaseRunStartRequest,
) -> Result<TestPhaseRunStartOutcome, String> {
    let phase_key = normalize_required("phase_key", &request.run.phase_key)?;
    let head_sha = normalize_required("head_sha", &request.run.head_sha)?;
    let lock_key = normalize_required("resource_lock_key", &request.resource_lock_key)?;
    let holder_instance_id = normalize_required("holder_instance_id", &request.holder_instance_id)?;
    let holder_job_id = normalize_required("holder_job_id", &request.holder_job_id)?;
    let metadata = json!({
        "phase_key": phase_key,
        "head_sha": head_sha,
        "idempotency_key": request.run.idempotency_key
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(""),
    });
    let lock_request = crate::server::resource_locks::ResourceLockRequest {
        lock_key: lock_key.clone(),
        holder_instance_id: holder_instance_id.clone(),
        holder_job_id: holder_job_id.clone(),
        ttl_secs: request.ttl_secs,
        metadata: Some(metadata),
    };
    let lock_outcome = crate::server::resource_locks::acquire_resource_lock(pool, &lock_request)
        .await
        .map_err(|error| format!("start test phase acquire lock: {error}"))?;
    if !lock_outcome.acquired {
        return Ok(TestPhaseRunStartOutcome {
            started: false,
            run: None,
            lock: None,
            current_lock: lock_outcome.current,
        });
    }

    let mut run_request = request.run.clone();
    run_request.status = Some("running".to_string());
    run_request.resource_lock_key = Some(lock_key.clone());
    run_request.holder_instance_id = Some(holder_instance_id.clone());
    run_request.holder_job_id = Some(holder_job_id.clone());
    let run = match upsert_test_phase_run(pool, &run_request).await {
        Ok(run) => run,
        Err(error) => {
            let _ = crate::server::resource_locks::release_resource_lock(
                pool,
                &lock_key,
                &holder_instance_id,
                &holder_job_id,
            )
            .await;
            return Err(error);
        }
    };

    Ok(TestPhaseRunStartOutcome {
        started: true,
        run: Some(run),
        lock: lock_outcome.lock,
        current_lock: None,
    })
}

pub async fn complete_test_phase_run(
    pool: &PgPool,
    request: &TestPhaseRunCompleteRequest,
) -> Result<TestPhaseRunCompleteOutcome, String> {
    let status = normalize_status(request.run.status.as_deref().unwrap_or(""))?;
    if !is_terminal_status(&status) {
        return Err("complete requires status passed, failed, or canceled".to_string());
    }
    let run = upsert_test_phase_run(pool, &request.run).await?;
    let lock_released = if request.release_lock {
        match (
            run.resource_lock_key.as_deref(),
            run.holder_instance_id.as_deref(),
            run.holder_job_id.as_deref(),
        ) {
            (Some(lock_key), Some(holder_instance_id), Some(holder_job_id)) => Some(
                crate::server::resource_locks::release_resource_lock(
                    pool,
                    lock_key,
                    holder_instance_id,
                    holder_job_id,
                )
                .await?,
            ),
            _ => Some(false),
        }
    } else {
        None
    };

    Ok(TestPhaseRunCompleteOutcome { run, lock_released })
}

pub async fn list_test_phase_runs(
    pool: &PgPool,
    query: &TestPhaseRunListQuery,
) -> Result<Vec<TestPhaseRun>, String> {
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    let phase_key = clean_optional(query.phase_key.as_deref());
    let head_sha = clean_optional(query.head_sha.as_deref());
    let status = query.status.as_deref().map(normalize_status).transpose()?;

    let rows = sqlx::query(
        r#"
        SELECT id, idempotency_key, phase_key, head_sha, status, issue_id,
               card_id, repo_id, required_capabilities, resource_lock_key,
               holder_instance_id, holder_job_id, evidence, error, started_at,
               completed_at, created_at, updated_at
          FROM test_phase_runs
         WHERE ($1::TEXT IS NULL OR phase_key = $1)
           AND ($2::TEXT IS NULL OR head_sha = $2)
           AND ($3::TEXT IS NULL OR status = $3)
         ORDER BY updated_at DESC
         LIMIT $4
        "#,
    )
    .bind(phase_key)
    .bind(head_sha)
    .bind(status)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("list test phase runs: {error}"))?;

    rows.into_iter().map(run_from_row).collect()
}

pub async fn latest_passing_evidence(
    pool: &PgPool,
    phase_key: &str,
    head_sha: &str,
) -> Result<Option<TestPhaseRun>, String> {
    let phase_key = normalize_required("phase_key", phase_key)?;
    let head_sha = normalize_required("head_sha", head_sha)?;
    let row = sqlx::query(
        r#"
        SELECT id, idempotency_key, phase_key, head_sha, status, issue_id,
               card_id, repo_id, required_capabilities, resource_lock_key,
               holder_instance_id, holder_job_id, evidence, error, started_at,
               completed_at, created_at, updated_at
          FROM test_phase_runs
         WHERE phase_key = $1
           AND head_sha = $2
           AND status = 'passed'
         ORDER BY completed_at DESC NULLS LAST, updated_at DESC
         LIMIT 1
        "#,
    )
    .bind(phase_key)
    .bind(head_sha)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("latest passing test phase evidence: {error}"))?;

    row.map(run_from_row).transpose()
}

fn normalize_required(field: &str, value: &str) -> Result<String, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(format!("{field} is required"));
    }
    if trimmed.len() > 256 {
        return Err(format!("{field} is too long"));
    }
    Ok(trimmed.to_string())
}

fn normalize_status(value: &str) -> Result<String, String> {
    let status = value.trim().to_ascii_lowercase();
    match status.as_str() {
        "queued" | "running" | "passed" | "failed" | "canceled" => Ok(status),
        _ => Err(format!("unsupported test phase status: {value}")),
    }
}

fn is_terminal_status(status: &str) -> bool {
    matches!(status, "passed" | "failed" | "canceled")
}

fn deterministic_idempotency_key(phase_key: &str, head_sha: &str) -> String {
    format!("{phase_key}:{head_sha}")
}

fn clean_optional(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn run_from_row(row: sqlx::postgres::PgRow) -> Result<TestPhaseRun, String> {
    Ok(TestPhaseRun {
        id: row.get("id"),
        idempotency_key: row.get("idempotency_key"),
        phase_key: row.get("phase_key"),
        head_sha: row.get("head_sha"),
        status: row.get("status"),
        issue_id: row.get("issue_id"),
        card_id: row.get("card_id"),
        repo_id: row.get("repo_id"),
        required_capabilities: row.get("required_capabilities"),
        resource_lock_key: row.get("resource_lock_key"),
        holder_instance_id: row.get("holder_instance_id"),
        holder_job_id: row.get("holder_job_id"),
        evidence: row.get("evidence"),
        error: row.get("error"),
        started_at: row.get("started_at"),
        completed_at: row.get("completed_at"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_validation_allows_only_phase_run_states() {
        assert_eq!(normalize_status("PASSED").unwrap(), "passed");
        assert!(normalize_status("skipped").is_err());
    }

    #[test]
    fn default_idempotency_key_is_phase_and_head_sha() {
        assert_eq!(
            deterministic_idempotency_key("unreal-smoke", "abc123"),
            "unreal-smoke:abc123"
        );
    }

    #[test]
    fn terminal_statuses_close_phase_runs() {
        assert!(!is_terminal_status("running"));
        assert!(is_terminal_status("passed"));
        assert!(is_terminal_status("failed"));
        assert!(is_terminal_status("canceled"));
    }
}
