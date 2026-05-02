use anyhow::{Result, anyhow};
use chrono::{DateTime, Duration, Timelike, Utc};
use chrono_tz::Tz;
use croner::Cron;
use croner::parser::{CronParser, Seconds, Year};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sqlx::{PgPool, Postgres, Row, Transaction};
use std::str::FromStr;
use std::sync::Arc;
use uuid::Uuid;

pub const ROUTINE_RUN_LEASE_SECS: u64 = 30 * 60;
const RUN_LEASE_SECS: i64 = ROUTINE_RUN_LEASE_SECS as i64;

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

fn bounded_observation_push(
    observations: &mut Vec<Value>,
    total_bytes: &mut usize,
    max_items: usize,
    max_payload_bytes: usize,
    obs: Value,
) -> bool {
    if observations.len() >= max_items {
        return false;
    }
    let bytes = obs.to_string().len();
    if *total_bytes + bytes > max_payload_bytes {
        return false;
    }
    *total_bytes += bytes;
    observations.push(obs);
    true
}

fn json_str<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

fn json_count(value: &Value) -> u64 {
    value
        .get("count")
        .or_else(|| value.get("occurrences"))
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .clamp(1, 50)
}

fn insert_bounded_json_string(target: &mut Map<String, Value>, payload: &Value, key: &str) {
    if let Some(value) = json_str(payload, key) {
        target.insert(key.to_owned(), Value::String(truncate_chars(value, 512)));
    }
}

fn insert_json_number(target: &mut Map<String, Value>, payload: &Value, key: &str) {
    if let Some(value) = payload.get(key).filter(|value| value.is_number()) {
        target.insert(key.to_owned(), value.clone());
    }
}

fn insert_bounded_string_array(target: &mut Map<String, Value>, payload: &Value, key: &str) {
    let Some(items) = payload.get(key).and_then(Value::as_array) else {
        return;
    };
    let values = items
        .iter()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(|item| Value::String(truncate_chars(item, 80)))
        .take(3)
        .collect::<Vec<_>>();
    if !values.is_empty() {
        target.insert(key.to_owned(), Value::Array(values));
    }
}

fn bounded_observation_value(payload: &Value) -> Value {
    let mut value = Map::new();
    for key in [
        "topic",
        "category",
        "source",
        "signature",
        "timestamp",
        "last_seen_at",
        "approved_at",
        "dispatched_at",
        "suggested_automation",
        "outcome_summary",
    ] {
        insert_bounded_json_string(&mut value, payload, key);
    }
    for key in [
        "count",
        "occurrences",
        "weight",
        "score",
        "evidence_count",
        "evidence_age_ms",
    ] {
        insert_json_number(&mut value, payload, key);
    }
    insert_bounded_string_array(&mut value, payload, "latest_examples");
    insert_bounded_string_array(&mut value, payload, "examples");
    Value::Object(value)
}

fn default_observation_category(source_kind: &str) -> &'static str {
    match source_kind {
        "memento_digest" => "memento-hygiene",
        "release_freshness" => "release-freshness",
        _ => "routine-candidate",
    }
}

fn default_observation_source(source_kind: &str) -> &'static str {
    match source_kind {
        "memento_digest" => "memento_digest",
        "release_freshness" => "precomputed_digest",
        _ => "precomputed_digest",
    }
}

/// Build one observation item from a `kv_meta` precomputed digest row.
///
/// # kv_meta digest ingestion surface contract
///
/// **Key format**: `routine_observation:{source_kind}:{topic}`
///   - `source_kind` maps to a human-readable `source` label (e.g. `memento_digest`,
///     `release_freshness`). Unknown kinds fall back to `precomputed_digest`.
///   - `topic` becomes the observation `signature` base when no explicit `signature` field
///     is present in the payload.
///
/// **TTL / expiry**: rows with `expires_at IS NOT NULL AND expires_at <= NOW()` are
/// excluded by the SQL query in `fetch_recent_run_observations`. Callers must set
/// `expires_at` in `kv_meta` to control observation lifetime. There is no default TTL —
/// omitting `expires_at` keeps the row permanently eligible.
///
/// **Dedup policy**: `evidence_ref` is set to `kv_meta:{key}`. Because `key` is unique
/// in `kv_meta`, two rows with the same logical key cannot produce duplicate observations.
/// The recommender's cross-tick `seen_evidence` map will further prevent re-scoring the
/// same key within the 25-hour dedup window.
///
/// **Payload fields** (all optional, sensible defaults apply):
/// - `topic` — display name / signature base
/// - `count` — occurrence count; drives `occurrences` and `weight` (≥5 → weight=2)
/// - `category` — overrides the source_kind default category
/// - `source` — overrides the source_kind default source label
/// - `signature` — explicit signature; defaults to `{category}:{topic}`
/// - `timestamp` — ISO8601; defaults to now
/// - `weight` — 1 or 2; auto-set from count if absent
/// - `latest_examples` / `examples` — string array, up to 3 items kept, each ≤80 chars
///
/// The returned observation also carries the original `key` and a bounded,
/// allowlisted `value` projection so JS routines can match
/// candidate_review/candidate_approved markers without reverse-parsing
/// `evidence_ref` or receiving raw kv_meta payloads.
///
/// **Role**: this is an internal precomputed digest surface. It is NOT a JS injection
/// endpoint. Callers must write to `kv_meta` directly (Rust service / maintenance job).
fn precomputed_observation_from_kv(
    key: &str,
    raw_value: Option<&str>,
    now: DateTime<Utc>,
) -> Option<Value> {
    let payload: Value = serde_json::from_str(raw_value?.trim()).ok()?;
    let suffix = key.strip_prefix("routine_observation:").unwrap_or(key);
    let (source_kind, key_topic) = suffix
        .split_once(':')
        .map(|(kind, topic)| (kind, topic))
        .unwrap_or((suffix, suffix));
    let topic = json_str(&payload, "topic").unwrap_or(key_topic);
    let count = json_count(&payload);
    let category =
        json_str(&payload, "category").unwrap_or_else(|| default_observation_category(source_kind));
    let source =
        json_str(&payload, "source").unwrap_or_else(|| default_observation_source(source_kind));
    let signature = json_str(&payload, "signature")
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("{category}:{topic}"));
    let timestamp = json_str(&payload, "timestamp")
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| now.to_rfc3339());
    let weight = payload
        .get("weight")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| if count >= 5 { 2 } else { 1 })
        .clamp(1, 2);
    let latest_examples = payload
        .get("latest_examples")
        .or_else(|| payload.get("examples"))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(|example| truncate_chars(example.trim(), 80))
                .filter(|example| !example.is_empty())
                .take(3)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let example_suffix = if latest_examples.is_empty() {
        String::new()
    } else {
        format!("; latest examples: {}", latest_examples.join(" | "))
    };
    let summary = truncate_chars(
        &format!("{topic}: {count} digest signal(s){example_suffix}"),
        240,
    );
    let value = bounded_observation_value(&payload);

    Some(serde_json::json!({
        "key": key,
        "value": value,
        "timestamp": timestamp,
        "source": source,
        "category": category,
        "signature": signature,
        "summary": summary,
        "weight": weight,
        "occurrences": count,
        "evidence_ref": format!("kv_meta:{key}"),
    }))
}

/// Durable PG-backed store for routines and routine_runs.
///
/// All mutating operations are transaction-scoped. Callers never hold a
/// connection across JS execution — claim and finish are always separate
/// transactions (see M-1 in PRD review notes).
#[derive(Clone)]
pub struct RoutineStore {
    pool: Arc<PgPool>,
    default_timezone: String,
}

#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct ClaimedRoutineRun {
    pub run_id: String,
    pub routine_id: String,
    pub agent_id: Option<String>,
    pub script_ref: String,
    pub name: String,
    pub execution_strategy: String,
    pub checkpoint: Option<Value>,
    pub discord_thread_id: Option<String>,
    pub timeout_secs: Option<i32>,
    pub lease_expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
pub struct RoutineRecord {
    pub id: String,
    pub agent_id: Option<String>,
    pub script_ref: String,
    pub name: String,
    pub status: String,
    pub execution_strategy: String,
    pub schedule: Option<String>,
    pub next_due_at: Option<DateTime<Utc>>,
    pub last_run_at: Option<DateTime<Utc>>,
    pub last_result: Option<String>,
    pub checkpoint: Option<Value>,
    pub discord_thread_id: Option<String>,
    pub timeout_secs: Option<i32>,
    pub in_flight_run_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
pub struct RoutineRunRecord {
    pub id: String,
    pub routine_id: String,
    pub status: String,
    pub action: Option<String>,
    pub turn_id: Option<String>,
    pub lease_expires_at: Option<DateTime<Utc>>,
    pub result_json: Option<Value>,
    pub error: Option<String>,
    pub discord_log_status: Option<String>,
    pub discord_log_error: Option<String>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
pub struct RoutineRunSearchRecord {
    pub id: String,
    pub routine_id: String,
    pub agent_id: Option<String>,
    pub script_ref: String,
    pub name: String,
    pub status: String,
    pub action: Option<String>,
    pub result_json: Option<Value>,
    pub error: Option<String>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RoutineMetrics {
    pub routines_total: i64,
    pub routines_enabled: i64,
    pub routines_paused: i64,
    pub routines_detached: i64,
    pub runs_total: i64,
    pub runs_running: i64,
    pub runs_succeeded: i64,
    pub runs_failed: i64,
    pub runs_skipped: i64,
    pub runs_paused: i64,
    pub runs_interrupted: i64,
    pub runs_error: i64,
    pub avg_latency_ms: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct RunningAgentRoutineRun {
    pub run_id: String,
    pub routine_id: String,
    pub script_ref: String,
    pub turn_id: String,
    pub result_json: Option<Value>,
    pub started_at: DateTime<Utc>,
    pub timeout_secs: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct RecoveredRoutineRun {
    pub run_id: String,
    pub routine_id: String,
    pub agent_id: Option<String>,
    pub script_ref: String,
    pub name: String,
    pub discord_thread_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NewRoutine {
    pub agent_id: Option<String>,
    pub script_ref: String,
    pub name: String,
    pub execution_strategy: String,
    pub schedule: Option<String>,
    pub next_due_at: Option<DateTime<Utc>>,
    pub checkpoint: Option<Value>,
    pub discord_thread_id: Option<String>,
    pub timeout_secs: Option<i32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RoutinePatch {
    pub name: Option<String>,
    pub execution_strategy: Option<String>,
    pub schedule: Option<Option<String>>,
    pub next_due_at: Option<Option<DateTime<Utc>>>,
    pub checkpoint: Option<Option<Value>>,
    pub discord_thread_id: Option<Option<String>>,
    pub timeout_secs: Option<Option<i32>>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct RoutineClaimCandidate {
    id: String,
    agent_id: Option<String>,
    script_ref: String,
    name: String,
    execution_strategy: String,
    checkpoint: Option<Value>,
    discord_thread_id: Option<String>,
    timeout_secs: Option<i32>,
}

impl RoutineStore {
    pub fn new_with_timezone(pool: Arc<PgPool>, default_timezone: impl Into<String>) -> Self {
        Self {
            pool,
            default_timezone: default_timezone.into(),
        }
    }

    pub(crate) fn pool(&self) -> &PgPool {
        self.pool.as_ref()
    }

    /// Claim due routines in a short transaction.
    ///
    /// This only creates `routine_runs` rows and marks parent routines
    /// in-flight. JS execution and finish/fail handling must happen after this
    /// transaction commits.
    pub async fn claim_due_runs(&self, limit: u32) -> Result<Vec<ClaimedRoutineRun>> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        let mut tx = self.pool.begin().await?;
        Self::seed_scheduled_due_times(&mut tx, &self.default_timezone).await?;
        let candidates: Vec<RoutineClaimCandidate> = sqlx::query_as(
            r#"
            SELECT id, agent_id, script_ref, name, execution_strategy, checkpoint,
                   discord_thread_id, timeout_secs
            FROM routines
            WHERE status = 'enabled'
              AND next_due_at IS NOT NULL
              AND next_due_at <= NOW()
              AND in_flight_run_id IS NULL
            ORDER BY next_due_at ASC, created_at ASC
            LIMIT $1
            FOR UPDATE SKIP LOCKED
            "#,
        )
        .bind(i64::from(limit))
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| anyhow!("claim due routines: select candidates: {e}"))?;

        let mut claimed = Vec::with_capacity(candidates.len());
        for candidate in candidates {
            claimed.push(Self::insert_running_run(&mut tx, candidate).await?);
        }

        tx.commit().await?;
        Ok(claimed)
    }

    async fn seed_scheduled_due_times(
        tx: &mut Transaction<'_, Postgres>,
        default_timezone: &str,
    ) -> Result<()> {
        let scheduled: Vec<(String, String)> = sqlx::query_as(
            r#"
            SELECT id, schedule
            FROM routines
            WHERE status = 'enabled'
              AND schedule IS NOT NULL
              AND next_due_at IS NULL
              AND in_flight_run_id IS NULL
            FOR UPDATE SKIP LOCKED
            "#,
        )
        .fetch_all(&mut **tx)
        .await
        .map_err(|e| anyhow!("seed routine schedules: select routines: {e}"))?;

        for (routine_id, schedule) in scheduled {
            let next_due_at =
                match Self::next_due_from_schedule_tx(tx, &schedule, default_timezone).await {
                    Ok(value) => value,
                    Err(error) => {
                        tracing::warn!(
                            routine_id,
                            schedule,
                            error = %error,
                            "routine has invalid schedule; next_due_at not seeded"
                        );
                        continue;
                    }
                };
            sqlx::query(
                r#"
                UPDATE routines
                SET next_due_at = $2,
                    updated_at = NOW()
                WHERE id = $1
                  AND status = 'enabled'
                  AND next_due_at IS NULL
                  AND in_flight_run_id IS NULL
                "#,
            )
            .bind(&routine_id)
            .bind(next_due_at)
            .execute(&mut **tx)
            .await
            .map_err(|e| anyhow!("seed routine {routine_id} next_due_at: {e}"))?;
        }

        Ok(())
    }

    pub async fn list_routines(
        &self,
        agent_id: Option<&str>,
        status: Option<&str>,
    ) -> Result<Vec<RoutineRecord>> {
        sqlx::query_as(
            r#"
            SELECT id, agent_id, script_ref, name, status, execution_strategy,
                   schedule, next_due_at, last_run_at, last_result, checkpoint,
                   discord_thread_id, timeout_secs, in_flight_run_id,
                   created_at, updated_at
            FROM routines
            WHERE ($1::text IS NULL OR agent_id = $1)
              AND ($2::text IS NULL OR status = $2)
            ORDER BY created_at DESC, id ASC
            "#,
        )
        .bind(agent_id)
        .bind(status)
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| anyhow!("list routines: {e}"))
    }

    pub async fn get_routine(&self, routine_id: &str) -> Result<Option<RoutineRecord>> {
        sqlx::query_as(
            r#"
            SELECT id, agent_id, script_ref, name, status, execution_strategy,
                   schedule, next_due_at, last_run_at, last_result, checkpoint,
                   discord_thread_id, timeout_secs, in_flight_run_id,
                   created_at, updated_at
            FROM routines
            WHERE id = $1
            "#,
        )
        .bind(routine_id)
        .fetch_optional(&*self.pool)
        .await
        .map_err(|e| anyhow!("get routine {routine_id}: {e}"))
    }

    pub async fn list_runs(&self, routine_id: &str, limit: i64) -> Result<Vec<RoutineRunRecord>> {
        let limit = limit.clamp(1, 100);
        sqlx::query_as(
            r#"
            SELECT id, routine_id, status, action, turn_id, lease_expires_at,
                   result_json, error, discord_log_status, discord_log_error, started_at,
                   finished_at, created_at, updated_at
            FROM routine_runs
            WHERE routine_id = $1
            ORDER BY started_at DESC, created_at DESC
            LIMIT $2
            "#,
        )
        .bind(routine_id)
        .bind(limit)
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| anyhow!("list routine runs {routine_id}: {e}"))
    }

    pub async fn list_running_agent_runs(&self, limit: u32) -> Result<Vec<RunningAgentRoutineRun>> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        sqlx::query_as(
            r#"
            SELECT rr.id AS run_id,
                   rr.routine_id,
                   r.script_ref,
                   rr.turn_id,
                   rr.result_json,
                   rr.started_at,
                   r.timeout_secs
            FROM routine_runs rr
            JOIN routines r ON r.id = rr.routine_id
            WHERE rr.status = 'running'
              AND rr.action = 'agent'
              AND rr.turn_id IS NOT NULL
            ORDER BY rr.started_at ASC, rr.created_at ASC
            LIMIT $1
            "#,
        )
        .bind(i64::from(limit))
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| anyhow!("list running agent routine runs: {e}"))
    }

    pub async fn heartbeat_running_agent_runs(&self) -> Result<u64> {
        let result = sqlx::query(
            r#"
            UPDATE routine_runs
            SET lease_expires_at = NOW() + ($1::bigint * INTERVAL '1 second'),
                updated_at = NOW()
            WHERE status = 'running'
              AND action = 'agent'
              AND turn_id IS NOT NULL
            "#,
        )
        .bind(RUN_LEASE_SECS)
        .execute(&*self.pool)
        .await
        .map_err(|e| anyhow!("heartbeat running agent routine runs: {e}"))?;

        Ok(result.rows_affected())
    }

    pub async fn metrics(
        &self,
        agent_id: Option<&str>,
        since: Option<DateTime<Utc>>,
    ) -> Result<RoutineMetrics> {
        let routine_row = sqlx::query(
            r#"
            SELECT
                COUNT(*)::BIGINT AS routines_total,
                COUNT(*) FILTER (WHERE status = 'enabled')::BIGINT AS routines_enabled,
                COUNT(*) FILTER (WHERE status = 'paused')::BIGINT AS routines_paused,
                COUNT(*) FILTER (WHERE status = 'detached')::BIGINT AS routines_detached
            FROM routines
            WHERE ($1::text IS NULL OR agent_id = $1)
            "#,
        )
        .bind(agent_id)
        .fetch_one(&*self.pool)
        .await
        .map_err(|e| anyhow!("load routine metrics: routines: {e}"))?;

        let run_row = sqlx::query(
            r#"
            SELECT
                COUNT(rr.id)::BIGINT AS runs_total,
                COUNT(rr.id) FILTER (WHERE rr.status = 'running')::BIGINT AS runs_running,
                COUNT(rr.id) FILTER (WHERE rr.status = 'succeeded')::BIGINT AS runs_succeeded,
                COUNT(rr.id) FILTER (WHERE rr.status = 'failed')::BIGINT AS runs_failed,
                COUNT(rr.id) FILTER (WHERE rr.status = 'skipped')::BIGINT AS runs_skipped,
                COUNT(rr.id) FILTER (WHERE rr.status = 'paused')::BIGINT AS runs_paused,
                COUNT(rr.id) FILTER (WHERE rr.status = 'interrupted')::BIGINT AS runs_interrupted,
                COUNT(rr.id) FILTER (
                    WHERE rr.status IN ('failed', 'interrupted') OR rr.error IS NOT NULL
                )::BIGINT AS runs_error,
                AVG(EXTRACT(EPOCH FROM (rr.finished_at - rr.started_at)) * 1000.0)
                    FILTER (WHERE rr.finished_at IS NOT NULL)::DOUBLE PRECISION AS avg_latency_ms
            FROM routine_runs rr
            JOIN routines r ON r.id = rr.routine_id
            WHERE ($1::text IS NULL OR r.agent_id = $1)
              AND ($2::timestamptz IS NULL OR rr.created_at >= $2)
            "#,
        )
        .bind(agent_id)
        .bind(since)
        .fetch_one(&*self.pool)
        .await
        .map_err(|e| anyhow!("load routine metrics: runs: {e}"))?;

        Ok(RoutineMetrics {
            routines_total: get_i64(&routine_row, "routines_total")?,
            routines_enabled: get_i64(&routine_row, "routines_enabled")?,
            routines_paused: get_i64(&routine_row, "routines_paused")?,
            routines_detached: get_i64(&routine_row, "routines_detached")?,
            runs_total: get_i64(&run_row, "runs_total")?,
            runs_running: get_i64(&run_row, "runs_running")?,
            runs_succeeded: get_i64(&run_row, "runs_succeeded")?,
            runs_failed: get_i64(&run_row, "runs_failed")?,
            runs_skipped: get_i64(&run_row, "runs_skipped")?,
            runs_paused: get_i64(&run_row, "runs_paused")?,
            runs_interrupted: get_i64(&run_row, "runs_interrupted")?,
            runs_error: get_i64(&run_row, "runs_error")?,
            avg_latency_ms: run_row
                .try_get("avg_latency_ms")
                .map_err(|e| anyhow!("decode routine metric avg_latency_ms: {e}"))?,
        })
    }

    pub async fn search_run_results(
        &self,
        query: &str,
        agent_id: Option<&str>,
        status: Option<&str>,
        since: Option<DateTime<Utc>>,
        limit: i64,
    ) -> Result<Vec<RoutineRunSearchRecord>> {
        let limit = limit.clamp(1, 100);
        let pattern = format!("%{}%", escape_like_pattern(query));
        sqlx::query_as(
            r#"
            SELECT
                rr.id,
                rr.routine_id,
                r.agent_id,
                r.script_ref,
                r.name,
                rr.status,
                rr.action,
                rr.result_json,
                rr.error,
                rr.started_at,
                rr.finished_at,
                rr.created_at,
                rr.updated_at
            FROM routine_runs rr
            JOIN routines r ON r.id = rr.routine_id
            WHERE rr.result_json IS NOT NULL
              AND rr.result_json::text ILIKE $1 ESCAPE '\'
              AND ($2::text IS NULL OR r.agent_id = $2)
              AND ($3::text IS NULL OR rr.status = $3)
              AND ($4::timestamptz IS NULL OR rr.created_at >= $4)
            ORDER BY rr.created_at DESC
            LIMIT $5
            "#,
        )
        .bind(pattern)
        .bind(agent_id)
        .bind(status)
        .bind(since)
        .bind(limit)
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| anyhow!("search routine run results: {e}"))
    }

    /// Fetch recent routine and system signals formatted as bounded observation items.
    ///
    /// Used to populate `ctx.observations` in `RoutineTickContext` so JS routines
    /// can accumulate evidence of recurring patterns without raw log or DB scanning.
    /// Precomputed digest rows must be stored under `routine_observation:*` keys
    /// in `kv_meta`; this keeps memento-derived inputs bounded to topic/count/examples
    /// snapshots instead of raw memory bodies.
    /// Results are truncated to `max_items` and `max_payload_bytes` before return.
    pub async fn fetch_recent_run_observations(
        &self,
        current_script_ref: Option<&str>,
        max_items: usize,
        max_payload_bytes: usize,
    ) -> Result<Vec<serde_json::Value>> {
        if max_items == 0 || max_payload_bytes == 0 {
            return Ok(Vec::new());
        }

        // Per-source hard caps for fair merge so no single source monopolises the
        // ctx.observations slot budget (total 100 items / 64 KB).
        const CAP_KV_META: i64 = 20;
        const CAP_API_FRICTION: i64 = 20;
        const CAP_OUTBOX: i64 = 20;
        const CAP_ROUTINE_RUNS: i64 = 40;

        let now = Utc::now();

        // --- Source 1: kv_meta precomputed digests (cap 20) ---
        let digest_rows = match sqlx::query(
            r#"
            SELECT key, value
            FROM kv_meta
            WHERE key LIKE 'routine_observation:%'
              AND (expires_at IS NULL OR expires_at > NOW())
            ORDER BY COALESCE(
                       NULLIF(value::jsonb ->> 'timestamp', ''),
                       NULLIF(value::jsonb ->> 'last_seen_at', ''),
                       NULLIF(value::jsonb ->> 'approved_at', ''),
                       NULLIF(value::jsonb ->> 'dispatched_at', ''),
                       ''
                     ) DESC,
                     key ASC
            LIMIT $1
            "#,
        )
        .bind(CAP_KV_META)
        .fetch_all(&*self.pool)
        .await
        {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "skipping precomputed routine observation source"
                );
                Vec::new()
            }
        };

        let mut kv_obs: Vec<serde_json::Value> = Vec::new();
        for row in &digest_rows {
            let key: String = row.try_get("key").unwrap_or_default();
            let value: Option<String> = row.try_get("value").ok().flatten();
            if let Some(obs) = precomputed_observation_from_kv(&key, value.as_deref(), now) {
                kv_obs.push(obs);
            }
        }

        // --- Source 2: api_friction_issues (cap 20) ---
        let api_rows = match sqlx::query(
            r#"
            SELECT fingerprint,
                   endpoint,
                   friction_type,
                   docs_category,
                   title,
                   event_count,
                   COALESCE(last_event_at, updated_at, created_at) AS last_seen_at
            FROM api_friction_issues
            WHERE COALESCE(last_event_at, updated_at, created_at) > NOW() - INTERVAL '30 days'
              AND event_count >= 2
            ORDER BY COALESCE(last_event_at, updated_at, created_at) DESC
            LIMIT $1
            "#,
        )
        .bind(CAP_API_FRICTION)
        .fetch_all(&*self.pool)
        .await
        {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(error = %error, "skipping api friction observation source");
                Vec::new()
            }
        };

        let mut friction_obs: Vec<serde_json::Value> = Vec::new();
        for row in &api_rows {
            let fingerprint: String = row.try_get("fingerprint").unwrap_or_default();
            let endpoint: String = row.try_get("endpoint").unwrap_or_default();
            let friction_type: String = row.try_get("friction_type").unwrap_or_default();
            let title: String = row.try_get("title").unwrap_or_default();
            let docs_category: Option<String> = row.try_get("docs_category").ok().flatten();
            let event_count: i32 = row.try_get("event_count").unwrap_or(1);
            let last_seen_at: DateTime<Utc> =
                row.try_get("last_seen_at").unwrap_or_else(|_| Utc::now());
            let docs_suffix = docs_category
                .as_deref()
                .filter(|value| !value.is_empty())
                .map(|value| format!(" docs={value}"))
                .unwrap_or_default();
            let summary = truncate_chars(
                &format!(
                    "{endpoint} {friction_type}: {} ({event_count} reports{docs_suffix})",
                    title.trim()
                ),
                240,
            );
            friction_obs.push(serde_json::json!({
                "timestamp": last_seen_at.to_rfc3339(),
                "source": "api_friction",
                "category": "api-friction",
                "signature": format!("api-friction:{fingerprint}"),
                "summary": summary,
                "weight": 2,
                "occurrences": event_count.max(1).min(50),
                "evidence_ref": format!("api_friction_issues:{fingerprint}"),
            }));
        }

        // --- Source 3: message_outbox grouped failures (cap 20) ---
        let outbox_rows = match sqlx::query(
            r#"
            SELECT COALESCE(NULLIF(source, ''), 'message_outbox') AS source,
                   COALESCE(NULLIF(reason_code, ''), status) AS reason_code,
                   status,
                   COUNT(*)::BIGINT AS occurrence_count,
                   MAX(created_at) AS last_seen_at,
                   MAX(error) FILTER (WHERE error IS NOT NULL) AS last_error
            FROM message_outbox
            WHERE created_at > NOW() - INTERVAL '24 hours'
              AND (status IN ('failed', 'error') OR error IS NOT NULL)
            GROUP BY source, reason_code, status
            ORDER BY MAX(created_at) DESC
            LIMIT $1
            "#,
        )
        .bind(CAP_OUTBOX)
        .fetch_all(&*self.pool)
        .await
        {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(error = %error, "skipping outbox delivery observation source");
                Vec::new()
            }
        };

        let mut outbox_obs: Vec<serde_json::Value> = Vec::new();
        for row in &outbox_rows {
            let source: String = row
                .try_get("source")
                .unwrap_or_else(|_| "message_outbox".into());
            let reason_code: String = row.try_get("reason_code").unwrap_or_default();
            let status: String = row.try_get("status").unwrap_or_default();
            let occurrence_count: i64 = row.try_get("occurrence_count").unwrap_or(1);
            let last_seen_at: DateTime<Utc> =
                row.try_get("last_seen_at").unwrap_or_else(|_| Utc::now());
            let last_error: Option<String> = row.try_get("last_error").ok().flatten();
            let summary =
                if let Some(error) = last_error.as_deref().filter(|value| !value.is_empty()) {
                    format!(
                        "{source} outbox {status} for {reason_code}: {}",
                        truncate_chars(error, 120)
                    )
                } else {
                    format!("{source} outbox {status} for {reason_code}")
                };
            outbox_obs.push(serde_json::json!({
                "timestamp": last_seen_at.to_rfc3339(),
                "source": "message_outbox",
                "category": "outbox-delivery",
                "signature": format!("outbox-delivery:{source}:{reason_code}:{status}"),
                "summary": truncate_chars(&summary, 240),
                "weight": 2,
                "occurrences": occurrence_count.max(1).min(50),
                "evidence_ref": format!("message_outbox:{source}:{reason_code}:{status}"),
            }));
        }

        // --- Source 4: routine_runs grouped by (script_ref, action, status) (cap 40) ---
        // Grouped so that repeated failures from the same routine emit one observation with
        // a stable evidence_ref instead of one raw row per UUID run.  The raw UUID approach
        // caused evidence_count to inflate ~N/tick and saturated score=100 after one tick.
        let run_rows = match sqlx::query(
            r#"
            WITH grouped_runs AS (
                SELECT r.script_ref,
                       (ARRAY_AGG(r.name ORDER BY rr.started_at DESC, rr.id DESC))[1] AS name,
                       COALESCE(rr.action, 'run') AS action,
                       rr.status,
                       COUNT(*)::BIGINT AS occurrence_count,
                       MAX(rr.started_at) AS latest_at,
                       (ARRAY_AGG(rr.error ORDER BY rr.started_at DESC, rr.id DESC)
                           FILTER (WHERE rr.error IS NOT NULL))[1] AS last_error
                FROM routine_runs rr
                JOIN routines r ON r.id = rr.routine_id
                WHERE rr.status IN ('succeeded', 'failed', 'skipped', 'error')
                  AND rr.started_at > NOW() - INTERVAL '24 hours'
                  AND ($1::text IS NULL OR r.script_ref <> $1)
                GROUP BY r.script_ref, COALESCE(rr.action, 'run'), rr.status
                ORDER BY MAX(rr.started_at) DESC
                LIMIT $2
            )
            SELECT grouped_runs.*,
                   sample.sample_ids
            FROM grouped_runs
            LEFT JOIN LATERAL (
                SELECT ARRAY(
                    SELECT rr_sample.id::text
                    FROM routine_runs rr_sample
                    JOIN routines r_sample ON r_sample.id = rr_sample.routine_id
                    WHERE r_sample.script_ref = grouped_runs.script_ref
                      AND COALESCE(rr_sample.action, 'run') = grouped_runs.action
                      AND rr_sample.status = grouped_runs.status
                      AND rr_sample.started_at > NOW() - INTERVAL '24 hours'
                      AND ($1::text IS NULL OR r_sample.script_ref <> $1)
                    ORDER BY rr_sample.started_at DESC
                    LIMIT 3
                ) AS sample_ids
            ) sample ON TRUE
            ORDER BY grouped_runs.latest_at DESC
            "#,
        )
        .bind(current_script_ref)
        .bind(CAP_ROUTINE_RUNS)
        .fetch_all(&*self.pool)
        .await
        {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(error = %error, "skipping routine run observation source");
                Vec::new()
            }
        };

        let mut run_obs: Vec<serde_json::Value> = Vec::new();
        for row in &run_rows {
            let script_ref: String = row.try_get("script_ref").unwrap_or_default();
            let name: String = row.try_get("name").unwrap_or_default();
            let action: String = row.try_get("action").unwrap_or_else(|_| "run".into());
            let status: String = row.try_get("status").unwrap_or_default();
            let occurrence_count: i64 = row.try_get("occurrence_count").unwrap_or(1);
            let latest_at: DateTime<Utc> = row.try_get("latest_at").unwrap_or_else(|_| Utc::now());
            let last_error: Option<String> = row.try_get("last_error").ok().flatten();
            let sample_ids: Vec<String> = row.try_get("sample_ids").unwrap_or_default();

            let weight: u8 = if status == "failed" || status == "error" {
                2
            } else {
                1
            };
            let summary = if let Some(ref err) = last_error {
                format!(
                    "{name} {action} {status}×{occurrence_count}: {}",
                    truncate_chars(err, 120)
                )
            } else {
                format!("{name} {action} {status}×{occurrence_count}")
            };
            let sample_refs: Vec<serde_json::Value> = sample_ids
                .iter()
                .take(3)
                .map(|id| serde_json::Value::String(format!("routine_run:{id}")))
                .collect();

            run_obs.push(serde_json::json!({
                "timestamp": latest_at.to_rfc3339(),
                "source": "routine_result",
                "category": "routine-candidate",
                "signature": format!("{script_ref}:{action}:{status}"),
                "summary": truncate_chars(&summary, 240),
                "weight": weight,
                "occurrences": (occurrence_count as u32).clamp(1, 50),
                // Stable across ticks: does not contain a per-run UUID.
                "evidence_ref": format!("routine_runs:{script_ref}:{action}:{status}"),
                "sample_evidence_refs": sample_refs,
            }));
        }

        // --- Fair merge: fill global cap from all sources in order ---
        let mut observations = Vec::with_capacity(max_items.min(100));
        let mut total_bytes: usize = 0;
        for obs in kv_obs
            .into_iter()
            .chain(friction_obs)
            .chain(outbox_obs)
            .chain(run_obs)
        {
            if !bounded_observation_push(
                &mut observations,
                &mut total_bytes,
                max_items,
                max_payload_bytes,
                obs,
            ) {
                break;
            }
        }

        Ok(observations)
    }

    pub async fn fetch_active_routine_automation_inventory(
        &self,
        max_items: usize,
        max_payload_bytes: usize,
    ) -> Result<Vec<serde_json::Value>> {
        let limit = (max_items as i64).min(100);
        let rows = sqlx::query(
            r#"
            SELECT script_ref, name, updated_at
            FROM routines
            WHERE status <> 'detached'
            ORDER BY updated_at DESC
            LIMIT $1
            "#,
        )
        .bind(limit)
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| anyhow!("fetch active routine automation inventory: {e}"))?;

        let mut inventory = Vec::with_capacity(rows.len());
        let mut total_bytes: usize = 0;

        for row in &rows {
            let script_ref: String = row.try_get("script_ref").unwrap_or_default();
            let name: String = row.try_get("name").unwrap_or_default();
            let updated_at: DateTime<Utc> =
                row.try_get("updated_at").unwrap_or_else(|_| Utc::now());

            let item = serde_json::json!({
                "pattern_id": format!("{script_ref}:*"),
                "status": "implemented",
                "reason": "registered routine",
                "source_ref": format!("routine:{name}"),
                "updated_at": updated_at.to_rfc3339(),
            });

            let bytes = item.to_string().len();
            if total_bytes + bytes > max_payload_bytes {
                break;
            }
            total_bytes += bytes;
            inventory.push(item);
        }

        Ok(inventory)
    }

    pub async fn attach_routine(&self, new_routine: NewRoutine) -> Result<RoutineRecord> {
        validate_execution_strategy(&new_routine.execution_strategy)?;
        let schedule = normalize_schedule(new_routine.schedule)?;
        validate_timeout_secs(new_routine.timeout_secs)?;
        let discord_thread_id = normalize_optional_text(new_routine.discord_thread_id);
        let next_due_at = if let Some(value) = new_routine.next_due_at {
            Some(value)
        } else if let Some(schedule) = schedule.as_deref() {
            Some(self.next_due_from_schedule(schedule).await?)
        } else {
            None
        };
        let id = Uuid::new_v4().to_string();
        sqlx::query_as(
            r#"
            INSERT INTO routines (
                id, agent_id, script_ref, name, status, execution_strategy,
                schedule, next_due_at, checkpoint, discord_thread_id, timeout_secs
            )
            VALUES ($1, $2, $3, $4, 'enabled', $5, $6, $7, $8, $9, $10)
            RETURNING id, agent_id, script_ref, name, status, execution_strategy,
                      schedule, next_due_at, last_run_at, last_result, checkpoint,
                      discord_thread_id, timeout_secs, in_flight_run_id,
                      created_at, updated_at
            "#,
        )
        .bind(id)
        .bind(new_routine.agent_id)
        .bind(new_routine.script_ref)
        .bind(new_routine.name)
        .bind(new_routine.execution_strategy)
        .bind(schedule)
        .bind(next_due_at)
        .bind(new_routine.checkpoint)
        .bind(discord_thread_id)
        .bind(new_routine.timeout_secs)
        .fetch_one(&*self.pool)
        .await
        .map_err(|e| anyhow!("attach routine: {e}"))
    }

    pub async fn patch_routine(
        &self,
        routine_id: &str,
        patch: RoutinePatch,
    ) -> Result<Option<RoutineRecord>> {
        if let Some(strategy) = patch.execution_strategy.as_deref() {
            validate_execution_strategy(strategy)?;
        }
        validate_timeout_secs(patch.timeout_secs.flatten())?;
        let schedule_was_set = patch.schedule.is_some();
        let schedule = match patch.schedule {
            Some(value) => normalize_schedule(value)?,
            None => None,
        };
        let discord_thread_id_was_set = patch.discord_thread_id.is_some();
        let discord_thread_id = patch
            .discord_thread_id
            .map(|value| normalize_optional_text(value));
        let timeout_secs_was_set = patch.timeout_secs.is_some();
        let timeout_secs = patch.timeout_secs.flatten();
        let next_due_was_set = patch.next_due_at.is_some();
        let mut next_due_at = patch.next_due_at.flatten();
        let mut update_next_due_at = next_due_was_set;
        if schedule_was_set && schedule.is_some() && !next_due_was_set {
            next_due_at = Some(
                self.next_due_from_schedule(
                    schedule
                        .as_deref()
                        .expect("checked schedule is present after is_some"),
                )
                .await?,
            );
            update_next_due_at = true;
        }
        if schedule_was_set && schedule.is_none() && !next_due_was_set {
            update_next_due_at = true;
        }
        sqlx::query_as(
            r#"
            UPDATE routines
            SET name = COALESCE($2, name),
                execution_strategy = COALESCE($3, execution_strategy),
                schedule = CASE WHEN $4 THEN $5 ELSE schedule END,
                next_due_at = CASE WHEN $6 THEN $7 ELSE next_due_at END,
                checkpoint = CASE WHEN $8 THEN $9 ELSE checkpoint END,
                discord_thread_id = CASE WHEN $10 THEN $11 ELSE discord_thread_id END,
                timeout_secs = CASE WHEN $12 THEN $13 ELSE timeout_secs END,
                updated_at = NOW()
            WHERE id = $1
              AND status <> 'detached'
            RETURNING id, agent_id, script_ref, name, status, execution_strategy,
                      schedule, next_due_at, last_run_at, last_result, checkpoint,
                      discord_thread_id, timeout_secs, in_flight_run_id,
                      created_at, updated_at
            "#,
        )
        .bind(routine_id)
        .bind(patch.name)
        .bind(patch.execution_strategy)
        .bind(schedule_was_set)
        .bind(schedule)
        .bind(update_next_due_at)
        .bind(next_due_at)
        .bind(patch.checkpoint.is_some())
        .bind(patch.checkpoint.flatten())
        .bind(discord_thread_id_was_set)
        .bind(discord_thread_id.flatten())
        .bind(timeout_secs_was_set)
        .bind(timeout_secs)
        .fetch_optional(&*self.pool)
        .await
        .map_err(|e| anyhow!("patch routine {routine_id}: {e}"))
    }

    /// Claim one enabled routine immediately, independent of its schedule.
    pub async fn claim_run_now(&self, routine_id: &str) -> Result<Option<ClaimedRoutineRun>> {
        let mut tx = self.pool.begin().await?;
        let candidate: Option<RoutineClaimCandidate> = sqlx::query_as(
            r#"
            SELECT id, agent_id, script_ref, name, execution_strategy, checkpoint,
                   discord_thread_id, timeout_secs
            FROM routines
            WHERE id = $1
              AND status = 'enabled'
              AND in_flight_run_id IS NULL
            FOR UPDATE SKIP LOCKED
            "#,
        )
        .bind(routine_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| anyhow!("claim run-now routine {routine_id}: select candidate: {e}"))?;

        let claimed = match candidate {
            Some(candidate) => Some(Self::insert_running_run(&mut tx, candidate).await?),
            None => None,
        };

        tx.commit().await?;
        Ok(claimed)
    }

    pub async fn finish_run(
        &self,
        run_id: &str,
        result_json: Option<Value>,
        checkpoint: Option<Value>,
        last_result: Option<&str>,
        next_due_at: Option<DateTime<Utc>>,
    ) -> Result<bool> {
        self.close_run(
            run_id,
            CloseRun {
                run_status: "succeeded",
                action: Some("complete"),
                result_json,
                error: None,
                checkpoint,
                last_result,
                next_due_at: NextDueAtUpdate::from_optional_preserve(next_due_at),
                pause_routine: false,
            },
        )
        .await
    }

    pub async fn skip_run(
        &self,
        run_id: &str,
        result_json: Option<Value>,
        checkpoint: Option<Value>,
        last_result: Option<&str>,
        next_due_at: Option<DateTime<Utc>>,
    ) -> Result<bool> {
        self.close_run(
            run_id,
            CloseRun {
                run_status: "skipped",
                action: Some("skip"),
                result_json,
                error: None,
                checkpoint,
                last_result,
                next_due_at: NextDueAtUpdate::from_optional_preserve(next_due_at),
                pause_routine: false,
            },
        )
        .await
    }

    pub async fn pause_after_run(
        &self,
        run_id: &str,
        result_json: Option<Value>,
        checkpoint: Option<Value>,
        last_result: Option<&str>,
    ) -> Result<bool> {
        self.close_run(
            run_id,
            CloseRun {
                run_status: "paused",
                action: Some("pause"),
                result_json,
                error: None,
                checkpoint,
                last_result,
                next_due_at: NextDueAtUpdate::Clear,
                pause_routine: true,
            },
        )
        .await
    }

    pub async fn fail_run(
        &self,
        run_id: &str,
        error: &str,
        result_json: Option<Value>,
        next_due_at: Option<DateTime<Utc>>,
    ) -> Result<bool> {
        self.close_run(
            run_id,
            CloseRun {
                run_status: "failed",
                action: None,
                result_json,
                error: Some(error),
                checkpoint: None,
                last_result: Some(error),
                next_due_at: NextDueAtUpdate::from_optional_preserve(next_due_at),
                pause_routine: false,
            },
        )
        .await
    }

    pub async fn mark_agent_turn_started(
        &self,
        run_id: &str,
        turn_id: &str,
        result_json: Option<Value>,
    ) -> Result<bool> {
        let result = sqlx::query(
            r#"
            UPDATE routine_runs
            SET action = 'agent',
                turn_id = $2,
                result_json = $3,
                lease_expires_at = NOW() + ($4::bigint * INTERVAL '1 second'),
                updated_at = NOW()
            WHERE id = $1
              AND status = 'running'
            "#,
        )
        .bind(run_id)
        .bind(turn_id)
        .bind(result_json)
        .bind(RUN_LEASE_SECS)
        .execute(&*self.pool)
        .await
        .map_err(|e| anyhow!("mark routine agent turn started {run_id}: {e}"))?;

        Ok(result.rows_affected() == 1)
    }

    pub async fn complete_agent_run(
        &self,
        run_id: &str,
        result_json: Option<Value>,
        checkpoint: Option<Value>,
        last_result: Option<&str>,
        next_due_at: NextDueAtUpdate,
    ) -> Result<bool> {
        self.close_run(
            run_id,
            CloseRun {
                run_status: "succeeded",
                action: Some("agent"),
                result_json,
                error: None,
                checkpoint,
                last_result,
                next_due_at,
                pause_routine: false,
            },
        )
        .await
    }

    pub async fn fail_agent_run(
        &self,
        run_id: &str,
        error: &str,
        result_json: Option<Value>,
        next_due_at: Option<DateTime<Utc>>,
    ) -> Result<bool> {
        self.close_run(
            run_id,
            CloseRun {
                run_status: "failed",
                action: Some("agent"),
                result_json,
                error: Some(error),
                checkpoint: None,
                last_result: Some(error),
                next_due_at: NextDueAtUpdate::from_optional_preserve(next_due_at),
                pause_routine: false,
            },
        )
        .await
    }

    pub async fn pause_routine(&self, routine_id: &str) -> Result<bool> {
        let result = sqlx::query(
            r#"
            UPDATE routines
            SET status = 'paused',
                next_due_at = NULL,
                updated_at = NOW()
            WHERE id = $1
              AND status = 'enabled'
            "#,
        )
        .bind(routine_id)
        .execute(&*self.pool)
        .await
        .map_err(|e| anyhow!("pause routine {routine_id}: {e}"))?;

        Ok(result.rows_affected() == 1)
    }

    /// Resume a paused routine. `next_due_at` is authoritative; pass `None`
    /// for manual-only routines.
    pub async fn resume_routine(
        &self,
        routine_id: &str,
        next_due_at: Option<DateTime<Utc>>,
    ) -> Result<bool> {
        let result = sqlx::query(
            r#"
            UPDATE routines
            SET status = 'enabled',
                next_due_at = $2,
                updated_at = NOW()
            WHERE id = $1
              AND status = 'paused'
            "#,
        )
        .bind(routine_id)
        .bind(next_due_at)
        .execute(&*self.pool)
        .await
        .map_err(|e| anyhow!("resume routine {routine_id}: {e}"))?;

        Ok(result.rows_affected() == 1)
    }

    pub async fn detach_routine(&self, routine_id: &str) -> Result<bool> {
        let result = sqlx::query(
            r#"
            UPDATE routines
            SET status = 'detached',
                next_due_at = NULL,
                updated_at = NOW()
            WHERE id = $1
              AND status <> 'detached'
              AND in_flight_run_id IS NULL
            "#,
        )
        .bind(routine_id)
        .execute(&*self.pool)
        .await
        .map_err(|e| anyhow!("detach routine {routine_id}: {e}"))?;

        Ok(result.rows_affected() == 1)
    }

    /// Extend the lease for a running routine run.
    ///
    /// Executors must call this periodically while JS execution is active.
    /// Boot recovery only interrupts rows whose lease has expired.
    pub async fn heartbeat_run(&self, run_id: &str) -> Result<bool> {
        let result = sqlx::query(
            r#"
            UPDATE routine_runs
            SET lease_expires_at = NOW() + ($2::bigint * INTERVAL '1 second'),
                updated_at = NOW()
            WHERE id = $1
              AND status = 'running'
            "#,
        )
        .bind(run_id)
        .bind(RUN_LEASE_SECS)
        .execute(&*self.pool)
        .await
        .map_err(|e| anyhow!("heartbeat routine run {run_id}: {e}"))?;

        Ok(result.rows_affected() == 1)
    }

    pub async fn record_discord_log_result(
        &self,
        run_id: &str,
        status: &str,
        error: Option<&str>,
    ) -> Result<bool> {
        let result = sqlx::query(
            r#"
            UPDATE routine_runs
            SET discord_log_status = CASE
                    WHEN discord_log_status = 'failed' AND $2 <> 'failed'
                    THEN discord_log_status
                    ELSE $2
                END,
                discord_log_error = CASE
                    WHEN discord_log_status = 'failed' AND $2 <> 'failed'
                    THEN discord_log_error
                    ELSE $3
                END,
                updated_at = NOW()
            WHERE id = $1
            "#,
        )
        .bind(run_id)
        .bind(status)
        .bind(error)
        .execute(&*self.pool)
        .await
        .map_err(|e| anyhow!("record routine discord log result {run_id}: {e}"))?;

        Ok(result.rows_affected() == 1)
    }

    pub async fn update_discord_thread_id(
        &self,
        routine_id: &str,
        discord_thread_id: &str,
    ) -> Result<bool> {
        let normalized = normalize_optional_text(Some(discord_thread_id.to_string()))
            .ok_or_else(|| anyhow!("discord_thread_id must not be empty"))?;
        let result = sqlx::query(
            r#"
            UPDATE routines
            SET discord_thread_id = $2,
                updated_at = NOW()
            WHERE id = $1
              AND status <> 'detached'
            "#,
        )
        .bind(routine_id)
        .bind(normalized)
        .execute(&*self.pool)
        .await
        .map_err(|e| anyhow!("update routine {routine_id} discord_thread_id: {e}"))?;

        Ok(result.rows_affected() == 1)
    }

    /// Interrupt the current in-flight run for a routine after an explicit
    /// session reset/kill invalidates the provider context.
    pub async fn interrupt_in_flight_run(
        &self,
        routine_id: &str,
        error: &str,
        result_json: Option<Value>,
    ) -> Result<Option<String>> {
        let mut tx = self.pool.begin().await?;

        let run_id: Option<String> = sqlx::query_scalar(
            r#"
            SELECT rr.id
            FROM routine_runs rr
            JOIN routines r ON r.in_flight_run_id = rr.id
            WHERE r.id = $1
              AND rr.routine_id = $1
              AND rr.status = 'running'
            FOR UPDATE OF rr
            "#,
        )
        .bind(routine_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| anyhow!("interrupt routine {routine_id}: lock running run: {e}"))?;

        let Some(run_id) = run_id else {
            tx.commit().await?;
            return Ok(None);
        };

        let routine_updated = sqlx::query(
            r#"
            UPDATE routines
            SET in_flight_run_id = NULL,
                last_result = $2,
                updated_at = NOW()
            WHERE id = $1
              AND in_flight_run_id = $3
            "#,
        )
        .bind(routine_id)
        .bind(error)
        .bind(&run_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| anyhow!("interrupt routine {routine_id}: clear in-flight: {e}"))?;

        if routine_updated.rows_affected() != 1 {
            tx.commit().await?;
            return Ok(None);
        }

        let run_updated = sqlx::query(
            r#"
            UPDATE routine_runs
            SET status = 'interrupted',
                result_json = COALESCE($3, result_json),
                error = $2,
                finished_at = NOW(),
                lease_expires_at = NULL,
                updated_at = NOW()
            WHERE id = $1
              AND routine_id = $4
              AND status = 'running'
            "#,
        )
        .bind(&run_id)
        .bind(error)
        .bind(result_json)
        .bind(routine_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| anyhow!("interrupt routine run {run_id}: {e}"))?;

        if run_updated.rows_affected() != 1 {
            return Err(anyhow!(
                "interrupt routine run {run_id}: running run guard lost row"
            ));
        }

        tx.commit().await?;
        Ok(Some(run_id))
    }

    /// Boot recovery: mark expired-lease `running` runs as `interrupted`, clear
    /// `in_flight_run_id` on their parent routines. Called once at worker
    /// startup before the tick loop begins. Running rows without an expired
    /// lease are left alone so a second server instance cannot interrupt work
    /// that another instance is actively executing.
    ///
    /// Returns the expired-lease runs that were recovered.
    pub async fn recover_stale_running_runs(&self) -> Result<Vec<RecoveredRoutineRun>> {
        let mut tx = self.pool.begin().await?;

        // Close expired leases. The UPDATE re-checks status and lease expiry
        // under the row lock so a concurrently finished run is not clobbered.
        let recovered: Vec<RecoveredRoutineRun> = sqlx::query_as(
            r#"
            WITH expired AS (
                SELECT id
                FROM routine_runs
                WHERE status = 'running'
                  AND lease_expires_at IS NOT NULL
                  AND lease_expires_at < NOW()
                FOR UPDATE SKIP LOCKED
            ),
            closed AS (
                UPDATE routine_runs AS rr
                SET status = 'interrupted',
                    finished_at = NOW(),
                    updated_at = NOW(),
                    lease_expires_at = NULL,
                    error = 'interrupted by expired routine lease'
                FROM expired
                WHERE rr.id = expired.id
                  AND rr.status = 'running'
                  AND rr.lease_expires_at IS NOT NULL
                  AND rr.lease_expires_at < NOW()
                RETURNING rr.id, rr.routine_id
            )
            SELECT closed.id AS run_id,
                   r.id AS routine_id,
                   r.agent_id,
                   r.script_ref,
                   r.name,
                   r.discord_thread_id
            FROM closed
            JOIN routines r ON r.id = closed.routine_id
            "#,
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| anyhow!("recover: close expired routine leases: {e}"))?;

        if recovered.is_empty() {
            tx.commit().await?;
            return Ok(Vec::new());
        }

        let recovered_routine_ids: Vec<&str> = recovered
            .iter()
            .map(|run| run.routine_id.as_str())
            .collect();
        let recovered_run_ids: Vec<&str> =
            recovered.iter().map(|run| run.run_id.as_str()).collect();

        // Release only locks that still point at the interrupted run.
        sqlx::query(
            r#"
            UPDATE routines AS r
            SET in_flight_run_id = NULL,
                updated_at = NOW()
            FROM UNNEST($1::text[], $2::text[]) AS recovered(routine_id, run_id)
            WHERE r.id = recovered.routine_id
              AND r.in_flight_run_id = recovered.run_id
            "#,
        )
        .bind(&recovered_routine_ids)
        .bind(&recovered_run_ids)
        .execute(&mut *tx)
        .await
        .map_err(|e| anyhow!("recover: clear in_flight_run_id: {e}"))?;

        tx.commit().await?;
        Ok(recovered)
    }

    async fn next_due_from_schedule(&self, schedule: &str) -> Result<DateTime<Utc>> {
        let now: DateTime<Utc> = sqlx::query_scalar("SELECT NOW()")
            .fetch_one(&*self.pool)
            .await
            .map_err(|e| anyhow!("load database time for routine schedule: {e}"))?;
        next_due_after(schedule, &self.default_timezone, now)
    }

    async fn next_due_from_schedule_tx(
        tx: &mut Transaction<'_, Postgres>,
        schedule: &str,
        default_timezone: &str,
    ) -> Result<DateTime<Utc>> {
        let now: DateTime<Utc> = sqlx::query_scalar("SELECT NOW()")
            .fetch_one(&mut **tx)
            .await
            .map_err(|e| anyhow!("load database time for routine schedule in tx: {e}"))?;
        next_due_after(schedule, default_timezone, now)
    }

    async fn next_due_from_schedule_anchor_tx(
        tx: &mut Transaction<'_, Postgres>,
        schedule: &str,
        default_timezone: &str,
        anchor: DateTime<Utc>,
    ) -> Result<DateTime<Utc>> {
        let now: DateTime<Utc> = sqlx::query_scalar("SELECT NOW()")
            .fetch_one(&mut **tx)
            .await
            .map_err(|e| anyhow!("load database time for anchored routine schedule: {e}"))?;
        next_due_after_anchor(schedule, default_timezone, anchor, now)
    }

    async fn insert_running_run(
        tx: &mut Transaction<'_, Postgres>,
        candidate: RoutineClaimCandidate,
    ) -> Result<ClaimedRoutineRun> {
        let run_id = Uuid::new_v4().to_string();

        let lease_expires_at: DateTime<Utc> = sqlx::query_scalar(
            r#"
            INSERT INTO routine_runs (id, routine_id, status, lease_expires_at)
            VALUES ($1, $2, 'running', NOW() + ($3::bigint * INTERVAL '1 second'))
            RETURNING lease_expires_at
            "#,
        )
        .bind(&run_id)
        .bind(&candidate.id)
        .bind(RUN_LEASE_SECS)
        .fetch_one(&mut **tx)
        .await
        .map_err(|e| anyhow!("claim routine {}: insert running run: {e}", candidate.id))?;

        let updated = sqlx::query(
            r#"
            UPDATE routines
            SET in_flight_run_id = $1,
                last_run_at = NOW(),
                updated_at = NOW()
            WHERE id = $2
              AND in_flight_run_id IS NULL
            "#,
        )
        .bind(&run_id)
        .bind(&candidate.id)
        .execute(&mut **tx)
        .await
        .map_err(|e| anyhow!("claim routine {}: mark in-flight: {e}", candidate.id))?;

        if updated.rows_affected() != 1 {
            return Err(anyhow!(
                "claim routine {}: in-flight guard rejected locked candidate",
                candidate.id
            ));
        }

        Ok(ClaimedRoutineRun {
            run_id,
            routine_id: candidate.id,
            agent_id: candidate.agent_id,
            script_ref: candidate.script_ref,
            name: candidate.name,
            execution_strategy: candidate.execution_strategy,
            checkpoint: candidate.checkpoint,
            discord_thread_id: candidate.discord_thread_id,
            timeout_secs: candidate.timeout_secs,
            lease_expires_at,
        })
    }

    async fn close_run(&self, run_id: &str, close: CloseRun<'_>) -> Result<bool> {
        let mut tx = self.pool.begin().await?;

        let target: Option<(String, Option<String>, Option<DateTime<Utc>>, DateTime<Utc>)> =
            sqlx::query_as(
                r#"
            SELECT r.id, r.schedule, r.next_due_at, rr.started_at
            FROM routine_runs rr
            JOIN routines r ON r.id = rr.routine_id
            WHERE rr.id = $1
              AND rr.status = 'running'
            FOR UPDATE OF rr, r
            "#,
            )
            .bind(run_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| anyhow!("close run {run_id}: lock running run: {e}"))?;

        let Some((routine_id, schedule, current_next_due_at, started_at)) = target else {
            tx.commit().await?;
            return Ok(false);
        };
        let scheduled_next_due_at = if close.next_due_at.should_update() {
            close.next_due_at.value()
        } else if let Some(schedule) = schedule.as_deref() {
            match current_next_due_at {
                Some(anchor) if anchor <= started_at => Some(
                    Self::next_due_from_schedule_anchor_tx(
                        &mut tx,
                        schedule,
                        &self.default_timezone,
                        anchor,
                    )
                    .await?,
                ),
                Some(_) => None,
                None => Some(
                    Self::next_due_from_schedule_tx(&mut tx, schedule, &self.default_timezone)
                        .await?,
                ),
            }
        } else {
            None
        };
        let should_update_next_due_at =
            close.next_due_at.should_update() || scheduled_next_due_at.is_some();

        let routine_updated = sqlx::query(
            r#"
            UPDATE routines
            SET in_flight_run_id = NULL,
                status = CASE WHEN $5 THEN 'paused' ELSE status END,
                next_due_at = CASE WHEN $7 THEN $2 ELSE next_due_at END,
                checkpoint = COALESCE($3, checkpoint),
                last_result = $4,
                updated_at = NOW()
            WHERE id = $1
              AND in_flight_run_id = $6
            "#,
        )
        .bind(&routine_id)
        .bind(scheduled_next_due_at)
        .bind(&close.checkpoint)
        .bind(close.last_result)
        .bind(close.pause_routine)
        .bind(run_id)
        .bind(should_update_next_due_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| anyhow!("close run {run_id}: update routine {routine_id}: {e}"))?;

        if routine_updated.rows_affected() != 1 {
            tx.commit().await?;
            return Ok(false);
        }

        let run_updated = sqlx::query(
            r#"
            UPDATE routine_runs
            SET status = $2,
                action = $3,
                result_json = $4,
                error = $5,
                finished_at = NOW(),
                lease_expires_at = NULL,
                updated_at = NOW()
            WHERE id = $1
              AND status = 'running'
            "#,
        )
        .bind(run_id)
        .bind(close.run_status)
        .bind(close.action)
        .bind(&close.result_json)
        .bind(close.error)
        .execute(&mut *tx)
        .await
        .map_err(|e| anyhow!("close run {run_id}: update run: {e}"))?;

        if run_updated.rows_affected() != 1 {
            return Err(anyhow!("close run {run_id}: running run guard lost row"));
        }

        tx.commit().await?;
        Ok(true)
    }
}

fn validate_execution_strategy(strategy: &str) -> Result<()> {
    match strategy {
        "fresh" | "persistent" => Ok(()),
        other => Err(anyhow!(
            "unsupported routine execution_strategy '{other}'; expected fresh or persistent"
        )),
    }
}

pub fn validate_routine_schedule(schedule: &str) -> Result<()> {
    parse_routine_schedule(schedule).map(|_| ())
}

fn normalize_schedule(schedule: Option<String>) -> Result<Option<String>> {
    schedule
        .map(|schedule| {
            let schedule = schedule.trim().to_string();
            validate_routine_schedule(&schedule)?;
            Ok(schedule)
        })
        .transpose()
}

fn normalize_optional_text(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn validate_timeout_secs(timeout_secs: Option<i32>) -> Result<()> {
    if let Some(value) = timeout_secs
        && value <= 0
    {
        return Err(anyhow!("routine timeout_secs must be greater than zero"));
    }
    Ok(())
}

enum ParsedRoutineSchedule {
    Every(Duration),
    Cron(Cron),
}

fn parse_routine_schedule(schedule: &str) -> Result<ParsedRoutineSchedule> {
    let trimmed = schedule.trim();
    if trimmed.is_empty() {
        return Err(anyhow!(
            "unsupported routine schedule '{schedule}'; expected @every <duration> or 5-field cron"
        ));
    }
    if trimmed.starts_with("@every ") || trimmed.starts_with("every ") {
        return parse_schedule_interval(trimmed).map(ParsedRoutineSchedule::Every);
    }
    if trimmed.starts_with('@') {
        return Err(anyhow!(
            "unsupported routine schedule '{schedule}'; expected @every <duration> or 5-field cron"
        ));
    }

    let field_count = trimmed.split_whitespace().count();
    if field_count != 5 {
        return Err(anyhow!(
            "unsupported routine cron schedule '{schedule}'; expected exactly 5 fields"
        ));
    }
    let cron = CronParser::builder()
        .seconds(Seconds::Disallowed)
        .year(Year::Disallowed)
        .build()
        .parse(trimmed)
        .map_err(|e| anyhow!("invalid routine cron schedule '{schedule}': {e}"))?;
    Ok(ParsedRoutineSchedule::Cron(cron))
}

fn next_due_after(
    schedule: &str,
    default_timezone: &str,
    now: DateTime<Utc>,
) -> Result<DateTime<Utc>> {
    match parse_routine_schedule(schedule)? {
        ParsedRoutineSchedule::Every(duration) => next_every_due_after(duration, now),
        ParsedRoutineSchedule::Cron(cron) => next_cron_due_after(cron, default_timezone, now),
    }
}

fn next_due_after_anchor(
    schedule: &str,
    default_timezone: &str,
    anchor: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<DateTime<Utc>> {
    match parse_routine_schedule(schedule)? {
        ParsedRoutineSchedule::Every(duration) => {
            next_every_due_after_anchor(duration, anchor, now)
        }
        ParsedRoutineSchedule::Cron(cron) => next_cron_due_after(cron, default_timezone, now),
    }
}

fn next_every_due_after(duration: Duration, now: DateTime<Utc>) -> Result<DateTime<Utc>> {
    checked_add_duration(
        truncate_to_second(now),
        duration,
        "compute next routine interval occurrence",
    )
}

fn next_every_due_after_anchor(
    duration: Duration,
    anchor: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<DateTime<Utc>> {
    let interval_secs = duration.num_seconds();
    if interval_secs <= 0 {
        return Err(anyhow!(
            "routine schedule duration must be greater than zero"
        ));
    }

    let anchor = truncate_to_second(anchor);
    let reference = truncate_to_second(now);
    let elapsed_secs = reference.signed_duration_since(anchor).num_seconds();
    let steps = if elapsed_secs < 0 {
        1
    } else {
        elapsed_secs
            .checked_div(interval_secs)
            .and_then(|value| value.checked_add(1))
            .ok_or_else(|| anyhow!("compute anchored routine interval occurrence: overflow"))?
    };
    let total_secs = interval_secs
        .checked_mul(steps)
        .ok_or_else(|| anyhow!("compute anchored routine interval occurrence: overflow"))?;

    checked_add_duration(
        anchor,
        Duration::seconds(total_secs),
        "compute anchored routine interval occurrence",
    )
}

fn next_cron_due_after(
    cron: Cron,
    default_timezone: &str,
    now: DateTime<Utc>,
) -> Result<DateTime<Utc>> {
    let timezone = Tz::from_str(default_timezone)
        .map_err(|_| anyhow!("invalid routines.default_timezone '{default_timezone}'"))?;
    let zoned_now = now.with_timezone(&timezone);
    cron.find_next_occurrence(&zoned_now, false)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|e| anyhow!("compute next routine cron occurrence: {e}"))
}

fn truncate_to_second(value: DateTime<Utc>) -> DateTime<Utc> {
    value
        .with_nanosecond(0)
        .expect("DateTime<Utc> nanosecond truncation should be valid")
}

fn checked_add_duration(
    base: DateTime<Utc>,
    duration: Duration,
    context: &'static str,
) -> Result<DateTime<Utc>> {
    base.checked_add_signed(duration)
        .ok_or_else(|| anyhow!("{context}: timestamp overflow"))
}

fn parse_schedule_interval(schedule: &str) -> Result<Duration> {
    let trimmed = schedule.trim();
    let duration = trimmed
        .strip_prefix("@every ")
        .or_else(|| trimmed.strip_prefix("every "))
        .unwrap_or(trimmed)
        .trim();
    if duration.is_empty() {
        return Err(anyhow!(
            "unsupported routine schedule '{schedule}'; expected @every <duration>"
        ));
    }

    let split_at = duration
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(duration.len());
    let (amount, unit) = duration.split_at(split_at);
    if amount.is_empty() || unit.trim().is_empty() {
        return Err(anyhow!(
            "unsupported routine schedule '{schedule}'; expected @every <duration>"
        ));
    }
    let amount: i64 = amount
        .parse()
        .map_err(|e| anyhow!("invalid routine schedule amount '{amount}': {e}"))?;
    if amount <= 0 {
        return Err(anyhow!(
            "routine schedule duration must be greater than zero"
        ));
    }

    let multiplier = match unit.trim().to_ascii_lowercase().as_str() {
        "s" | "sec" | "secs" | "second" | "seconds" => 1,
        "m" | "min" | "mins" | "minute" | "minutes" => 60,
        "h" | "hr" | "hrs" | "hour" | "hours" => 60 * 60,
        "d" | "day" | "days" => 60 * 60 * 24,
        other => {
            return Err(anyhow!(
                "unsupported routine schedule unit '{other}'; expected s, m, h, or d"
            ));
        }
    };
    let seconds = amount
        .checked_mul(multiplier)
        .ok_or_else(|| anyhow!("routine schedule duration is too large"))?;
    Ok(Duration::seconds(seconds))
}

fn get_i64(row: &sqlx::postgres::PgRow, column: &str) -> Result<i64> {
    row.try_get(column)
        .map_err(|e| anyhow!("decode routine metric {column}: {e}"))
}

fn escape_like_pattern(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

#[derive(Debug, Clone, Copy)]
pub enum NextDueAtUpdate {
    Preserve,
    Set(DateTime<Utc>),
    Clear,
}

impl NextDueAtUpdate {
    fn from_optional_preserve(next_due_at: Option<DateTime<Utc>>) -> Self {
        next_due_at.map(Self::Set).unwrap_or(Self::Preserve)
    }

    fn should_update(&self) -> bool {
        !matches!(self, Self::Preserve)
    }

    fn value(&self) -> Option<DateTime<Utc>> {
        match self {
            Self::Preserve | Self::Clear => None,
            Self::Set(value) => Some(*value),
        }
    }
}

struct CloseRun<'a> {
    run_status: &'a str,
    action: Option<&'a str>,
    result_json: Option<Value>,
    error: Option<&'a str>,
    checkpoint: Option<Value>,
    last_result: Option<&'a str>,
    next_due_at: NextDueAtUpdate,
    pause_routine: bool,
}

#[cfg(test)]
mod tests {
    use super::{
        next_due_after, next_due_after_anchor, parse_schedule_interval,
        precomputed_observation_from_kv, truncate_chars, validate_routine_schedule,
    };
    use chrono::{TimeZone, Timelike, Utc};
    use serde_json::Value;

    // Integration tests that require a live PG connection live in
    // src/integration_tests.rs and are gated on the `integration` feature.
    // The store SQL is compiled by `cargo check`; concurrent claim/recovery
    // behavior should be covered by PG integration tests once the runtime
    // harness starts executing routines.

    #[test]
    fn parses_supported_interval_schedules() {
        assert_eq!(
            parse_schedule_interval("@every 30s").unwrap().num_seconds(),
            30
        );
        assert_eq!(
            parse_schedule_interval("every 5m").unwrap().num_seconds(),
            300
        );
        assert_eq!(parse_schedule_interval("2h").unwrap().num_seconds(), 7200);
        assert_eq!(parse_schedule_interval("1d").unwrap().num_seconds(), 86_400);
    }

    #[test]
    fn rejects_invalid_interval_schedules() {
        assert!(validate_routine_schedule("").is_err());
        assert!(validate_routine_schedule("@every 0s").is_err());
        assert!(validate_routine_schedule("@daily").is_err());
        assert!(validate_routine_schedule("* * * * * *").is_err());
        assert!(validate_routine_schedule("60 9 * * *").is_err());
    }

    #[test]
    fn accepts_standard_cron_schedules() {
        assert!(validate_routine_schedule("*/5 * * * *").is_ok());
        assert!(validate_routine_schedule("30 9 * * 1-5").is_ok());
    }

    #[test]
    fn cron_next_due_uses_default_timezone() {
        let now = Utc.with_ymd_and_hms(2026, 4, 29, 0, 0, 0).unwrap();
        let next_due = next_due_after("30 9 * * 1-5", "Asia/Seoul", now).unwrap();
        let next_due_kst = next_due.with_timezone(&chrono_tz::Asia::Seoul);
        assert_eq!(next_due_kst.hour(), 9);
        assert_eq!(next_due_kst.minute(), 30);
    }

    #[test]
    fn every_next_due_uses_utc_interval() {
        let now = Utc.with_ymd_and_hms(2026, 4, 29, 0, 0, 0).unwrap();
        let next_due = next_due_after("@every 1h", "Asia/Seoul", now).unwrap();
        assert_eq!(
            next_due,
            Utc.with_ymd_and_hms(2026, 4, 29, 1, 0, 0).unwrap()
        );
    }

    #[test]
    fn every_next_due_truncates_subsecond_jitter() {
        let now = Utc
            .with_ymd_and_hms(2026, 4, 30, 3, 32, 8)
            .unwrap()
            .with_nanosecond(830_000_000)
            .unwrap();
        let next_due = next_due_after("@every 1m", "Asia/Seoul", now).unwrap();
        assert_eq!(
            next_due,
            Utc.with_ymd_and_hms(2026, 4, 30, 3, 33, 8).unwrap()
        );
    }

    #[test]
    fn anchored_every_next_due_skips_missed_intervals_and_stays_second_aligned() {
        let anchor = Utc
            .with_ymd_and_hms(2026, 4, 30, 3, 31, 8)
            .unwrap()
            .with_nanosecond(830_000_000)
            .unwrap();
        let now = Utc
            .with_ymd_and_hms(2026, 4, 30, 3, 32, 8)
            .unwrap()
            .with_nanosecond(831_000_000)
            .unwrap();
        let next_due = next_due_after_anchor("@every 1m", "Asia/Seoul", anchor, now).unwrap();
        assert_eq!(
            next_due,
            Utc.with_ymd_and_hms(2026, 4, 30, 3, 33, 8).unwrap()
        );
    }

    #[test]
    fn truncate_chars_does_not_split_multibyte_text() {
        let text = "가".repeat(121);
        let truncated = truncate_chars(&text, 120);
        assert!(truncated.ends_with("..."));
        assert_eq!(truncated.trim_end_matches("...").chars().count(), 120);
    }

    #[test]
    fn precomputed_memento_digest_observation_uses_digest_fields_only() {
        let now = Utc.with_ymd_and_hms(2026, 4, 30, 7, 0, 0).unwrap();
        let raw = serde_json::json!({
            "topic": "api friction repeats",
            "count": 7,
            "latest_examples": ["GET /api/docs before retry", "kanban docs lookup"],
            "raw_memory_body": "SECRET_RAW_MEMORY_BODY_SHOULD_NOT_LEAK",
            "timestamp": "2026-04-30T06:59:00Z"
        })
        .to_string();

        let obs = precomputed_observation_from_kv(
            "routine_observation:memento_digest:api-friction",
            Some(&raw),
            now,
        )
        .expect("digest observation");

        assert_eq!(
            obs.get("source").and_then(Value::as_str),
            Some("memento_digest")
        );
        assert_eq!(
            obs.get("category").and_then(Value::as_str),
            Some("memento-hygiene")
        );
        assert_eq!(obs.get("occurrences").and_then(Value::as_u64), Some(7));
        assert_eq!(
            obs.get("key").and_then(Value::as_str),
            Some("routine_observation:memento_digest:api-friction")
        );
        assert_eq!(
            obs.pointer("/value/topic").and_then(Value::as_str),
            Some("api friction repeats")
        );
        assert!(
            obs.pointer("/value/raw_memory_body").is_none(),
            "raw kv_meta payload fields must not be forwarded"
        );
        let summary = obs.get("summary").and_then(Value::as_str).unwrap();
        assert!(summary.contains("api friction repeats"));
        assert!(summary.contains("GET /api/docs before retry"));
        assert!(!summary.contains("SECRET_RAW_MEMORY_BODY"));
    }

    #[test]
    fn candidate_marker_observation_keeps_only_bounded_value_fields() {
        let now = Utc.with_ymd_and_hms(2026, 5, 2, 10, 0, 0).unwrap();
        let raw = serde_json::json!({
            "signature": "candidate-a",
            "score": 91,
            "evidence_count": 8,
            "category": "routine-candidate",
            "suggested_automation": "x".repeat(600),
            "outcome_summary": "safe summary",
            "last_seen_at": "2026-05-02T09:59:00Z",
            "raw_memory_body": "SECRET_RAW_MEMORY_BODY_SHOULD_NOT_LEAK"
        })
        .to_string();

        let obs = precomputed_observation_from_kv(
            "routine_observation:candidate_review:candidate-a",
            Some(&raw),
            now,
        )
        .expect("candidate review observation");

        assert_eq!(
            obs.pointer("/value/signature").and_then(Value::as_str),
            Some("candidate-a")
        );
        assert_eq!(
            obs.pointer("/value/score").and_then(Value::as_u64),
            Some(91)
        );
        assert_eq!(
            obs.pointer("/value/evidence_count").and_then(Value::as_u64),
            Some(8)
        );
        assert!(
            obs.pointer("/value/suggested_automation")
                .and_then(Value::as_str)
                .unwrap()
                .chars()
                .count()
                <= 515
        );
        assert!(obs.pointer("/value/raw_memory_body").is_none());
    }

    #[test]
    fn bounded_push_enforces_item_cap() {
        use super::bounded_observation_push;
        let mut obs: Vec<Value> = Vec::new();
        let mut total_bytes = 0usize;
        let item = serde_json::json!({"x": "y"});
        assert!(bounded_observation_push(
            &mut obs,
            &mut total_bytes,
            2,
            usize::MAX,
            item.clone()
        ));
        assert!(bounded_observation_push(
            &mut obs,
            &mut total_bytes,
            2,
            usize::MAX,
            item.clone()
        ));
        assert!(!bounded_observation_push(
            &mut obs,
            &mut total_bytes,
            2,
            usize::MAX,
            item.clone()
        ));
        assert_eq!(obs.len(), 2);
    }

    #[test]
    fn bounded_push_enforces_byte_cap() {
        use super::bounded_observation_push;
        let mut obs: Vec<Value> = Vec::new();
        let mut total_bytes = 0usize;
        let item = serde_json::json!({"summary": "aaaa"});
        let item_size = item.to_string().len();
        let cap = item_size + 1; // only room for 1 item
        assert!(bounded_observation_push(
            &mut obs,
            &mut total_bytes,
            usize::MAX,
            cap,
            item.clone()
        ));
        assert!(!bounded_observation_push(
            &mut obs,
            &mut total_bytes,
            usize::MAX,
            cap,
            item.clone()
        ));
        assert_eq!(obs.len(), 1);
    }

    #[test]
    fn routine_runs_evidence_ref_format_is_stable() {
        let script_ref = "monitoring/my-script.js";
        let action = "run";
        let status = "failed";
        let evidence_ref = format!("routine_runs:{script_ref}:{action}:{status}");
        assert_eq!(
            evidence_ref,
            "routine_runs:monitoring/my-script.js:run:failed"
        );
        assert!(
            evidence_ref.starts_with("routine_runs:"),
            "evidence_ref must be prefixed with 'routine_runs:'"
        );
    }
}
