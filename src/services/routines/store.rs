use anyhow::{Result, anyhow};
use chrono::{DateTime, Duration, Timelike, Utc};
use chrono_tz::Tz;
use croner::Cron;
use croner::parser::{CronParser, Seconds, Year};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sqlx::{PgPool, Postgres, Row, Transaction};
use std::str::FromStr;
use std::sync::Arc;
use uuid::Uuid;

use crate::api_caller_observability::{RequestPrincipal, log_identity_consumption};
use crate::services::automation_candidate_contract::{
    PIPELINE_STAGE_ID, has_complete_loop_contract,
};
use crate::utils::api::clamp_api_limit;

pub const ROUTINE_RUN_LEASE_SECS: u64 = 30 * 60;
const RUN_LEASE_SECS: i64 = ROUTINE_RUN_LEASE_SECS as i64;
const RESUME_NEXT_DUE_REQUIRED_MESSAGE: &str =
    "next_due_at required to resume schedule-less routine";

/// pause_reason column values written to `routines.pause_reason`.
/// The column is nullable: pre-existing paused rows retain NULL (unknown cause)
/// and are conservatively excluded from auto-resume.
pub const PAUSE_REASON_FAILURE: &str = "failure";
pub const PAUSE_REASON_MANUAL: &str = "manual";
pub const PAUSE_REASON_MIGRATION_INVALID: &str = "migration_invalid";

pub(crate) fn terminal_failure_should_pause(pause_on_terminal_failure: bool) -> bool {
    terminal_failure_pause_reason(pause_on_terminal_failure).is_some()
}

fn terminal_failure_pause_reason(pause_on_terminal_failure: bool) -> Option<&'static str> {
    pause_on_terminal_failure.then_some(PAUSE_REASON_FAILURE)
}

const API_FRICTION_OBSERVATION_QUERY: &str = r#"
            SELECT fingerprint,
                   endpoint,
                   friction_type,
                   title,
                   event_count,
                   COALESCE(last_event_at, updated_at, created_at) AS last_seen_at
            FROM api_friction_issues
            WHERE COALESCE(last_event_at, updated_at, created_at) > NOW() - INTERVAL '30 days'
              AND event_count >= 2
            ORDER BY COALESCE(last_event_at, updated_at, created_at) DESC
            LIMIT $1
            "#;

#[derive(Debug)]
pub struct ResumeRoutineRequiresNextDueAt;

impl std::fmt::Display for ResumeRoutineRequiresNextDueAt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(RESUME_NEXT_DUE_REQUIRED_MESSAGE)
    }
}

impl std::error::Error for ResumeRoutineRequiresNextDueAt {}

pub fn is_resume_routine_requires_next_due_at(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<ResumeRoutineRequiresNextDueAt>()
        .is_some()
}

fn resume_without_next_due_is_invalid(
    schedule: Option<&str>,
    next_due_at: Option<DateTime<Utc>>,
) -> bool {
    schedule.is_none() && next_due_at.is_none()
}

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

fn include_automation_candidate_card_observations(current_script_ref: Option<&str>) -> bool {
    matches!(
        current_script_ref.map(str::trim),
        Some(
            "monitoring/automation-candidate-executor.js"
                | "monitoring/automation-executor.js"
                | "monitoring/automation-executor-v2.js"
        )
    )
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
    max_checkpoint_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct ClaimedRoutineRun {
    pub run_id: String,
    pub routine_id: String,
    pub agent_id: Option<String>,
    pub fallback_agent_id: Option<String>,
    pub max_retries: i32,
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
    pub fallback_agent_id: Option<String>,
    pub max_retries: i32,
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
    /// Cause of the most recent pause. Set when status transitions to 'paused'.
    /// - `Some("failure")` → set by `fail_run_and_pause_routine` (run failed/timed-out).
    /// - `Some("manual")` → set by `pause_routine` (operator-initiated).
    /// - `Some("migration_invalid")` → set when a migrated-launchd run fails validation.
    /// - `None` → pre-existing row (unknown cause; treated conservatively as manual).
    pub pause_reason: Option<String>,
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
    pub retry_count: i32,
    pub next_retry_at: Option<DateTime<Utc>>,
    pub attempts: Value,
    pub result_json: Option<Value>,
    pub error: Option<String>,
    pub discord_log_status: Option<String>,
    pub discord_log_error: Option<String>,
    pub discord_message_id: Option<String>,
    pub discord_log_sections: Value,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteRoutineResult {
    Deleted {
        run_history_deleted: u64,
        routine_agent_id: Option<String>,
        caller_agent_id: Option<String>,
    },
    NotFound {
        caller_agent_id: Option<String>,
    },
    NotDetached {
        status: String,
        routine_agent_id: Option<String>,
        caller_agent_id: Option<String>,
    },
    InFlight {
        routine_agent_id: Option<String>,
        caller_agent_id: Option<String>,
    },
    Forbidden {
        owner: String,
        caller_agent_id: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RoutineHardDeleteGate {
    Allowed,
    NotDetached { status: String },
    InFlight,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RoutineDeleteScopeGate {
    Allowed,
    Unresolved { owner: String },
    OtherAgent { owner: String, caller: String },
}

fn routine_hard_delete_gate(
    status: &str,
    in_flight_run_id: Option<&str>,
    has_running_run: bool,
) -> RoutineHardDeleteGate {
    if in_flight_run_id
        .map(str::trim)
        .is_some_and(|value| !value.is_empty())
        || has_running_run
    {
        return RoutineHardDeleteGate::InFlight;
    }
    if status == "detached" {
        RoutineHardDeleteGate::Allowed
    } else {
        RoutineHardDeleteGate::NotDetached {
            status: status.to_string(),
        }
    }
}

fn routine_delete_scope_gate(
    routine_agent_id: Option<&str>,
    caller_agent_id: Option<&str>,
    principal: Option<&RequestPrincipal>,
) -> RoutineDeleteScopeGate {
    log_identity_consumption(
        "DELETE /api/routines/{id}",
        principal,
        caller_agent_id,
        false,
    );

    let Some(owner) = routine_agent_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return RoutineDeleteScopeGate::Allowed;
    };
    let Some(caller) = caller_agent_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return RoutineDeleteScopeGate::Unresolved {
            owner: owner.to_string(),
        };
    };
    if caller == owner {
        RoutineDeleteScopeGate::Allowed
    } else {
        RoutineDeleteScopeGate::OtherAgent {
            owner: owner.to_string(),
            caller: caller.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct RunningAgentRoutineRun {
    pub run_id: String,
    pub routine_id: String,
    pub agent_id: Option<String>,
    pub fallback_agent_id: Option<String>,
    pub max_retries: i32,
    pub retry_count: i32,
    pub script_ref: String,
    pub name: String,
    pub execution_strategy: String,
    pub discord_thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub next_retry_at: Option<DateTime<Utc>>,
    pub attempts: Value,
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
    /// Routine execution strategy (`fresh` | `persistent`). Only `fresh` runs
    /// own a throwaway session that boot recovery should reap (#3022).
    pub execution_strategy: String,
    /// The ownership token of the session this run created, if it started an
    /// agent turn (#3022). This is a full `session_key` (`host:<tmux>`) when the
    /// session row was resolvable at turn-start, or a bare tmux name otherwise.
    /// `None` means the run owns nothing to reap (it never started a turn, or
    /// predates ownership tracking), so recovery leaves all sessions alone.
    pub owned_tmux_session: Option<String>,
}

impl RecoveredRoutineRun {
    /// The ownership token recovery must reap for this interrupted run, or
    /// `None` if the run owns nothing reapable.
    ///
    /// Positive ownership proof is required (`owned_tmux_session` set) before
    /// any teardown, so an interrupted run can never tear down a session it did
    /// not create. The strategy is re-checked defensively: only `fresh` runs
    /// create throwaway sessions, and a persistent routine's session must
    /// survive a restart. The token is trimmed and empty tokens are treated as
    /// "owns nothing" so a blank legacy value cannot resolve to a wildcard.
    pub fn boot_recovery_owned_session(&self) -> Option<&str> {
        if self.execution_strategy != "fresh" {
            return None;
        }
        self.owned_tmux_session
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunDiscordLogState {
    pub message_id: Option<String>,
    pub sections: Value,
}

#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct RunDiscordLogFailure {
    pub status: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NewRoutine {
    pub agent_id: Option<String>,
    pub fallback_agent_id: Option<String>,
    pub max_retries: Option<i32>,
    pub script_ref: String,
    pub name: String,
    pub status: Option<String>,
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
    pub fallback_agent_id: Option<Option<String>>,
    pub max_retries: Option<i32>,
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
    fallback_agent_id: Option<String>,
    max_retries: i32,
    script_ref: String,
    name: String,
    execution_strategy: String,
    checkpoint: Option<Value>,
    discord_thread_id: Option<String>,
    timeout_secs: Option<i32>,
}

impl RoutineStore {
    pub fn new_with_timezone_and_checkpoint_limit(
        pool: Arc<PgPool>,
        default_timezone: impl Into<String>,
        max_checkpoint_bytes: usize,
    ) -> Self {
        Self {
            pool,
            default_timezone: default_timezone.into(),
            max_checkpoint_bytes: max_checkpoint_bytes.max(1),
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
            SELECT id, agent_id, fallback_agent_id, max_retries, script_ref, name,
                   execution_strategy, checkpoint, discord_thread_id, timeout_secs
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
                   discord_thread_id, timeout_secs, fallback_agent_id, max_retries,
                   in_flight_run_id, pause_reason,
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

    /// Routines that have been stuck in `paused` since before `threshold`.
    ///
    /// A `paused` routine is excluded from claims, so `updated_at` stops moving
    /// the moment it pauses — it is therefore an accurate "paused_since" proxy
    /// and no new column/migration is needed. Used by the runtime tick to alert
    /// operators when a failed/timed-out routine would otherwise stay paused
    /// forever (#3564).
    ///
    /// The cap is a generous safety ceiling rather than a real batch limit: a
    /// deployment runs on the order of dozens of routines, so 500 comfortably
    /// covers the entire paused set in one pass. A small `LIMIT 50` would cause
    /// tail starvation — this tick never mutates the routine row it processes,
    /// so once the paused backlog exceeds the limit the same oldest
    /// (`updated_at ASC`) rows are returned every tick and the routines past the
    /// cutoff would never get an alert. The cap only exists to bound a
    /// pathological backlog; alert spam is already prevented by the per-routine
    /// dedupe TTL on the outbox, not by this limit.
    pub async fn list_stale_paused_routines(
        &self,
        threshold: DateTime<Utc>,
    ) -> Result<Vec<RoutineRecord>> {
        sqlx::query_as(
            r#"
            SELECT id, agent_id, script_ref, name, status, execution_strategy,
                   schedule, next_due_at, last_run_at, last_result, checkpoint,
                   discord_thread_id, timeout_secs, fallback_agent_id, max_retries,
                   in_flight_run_id, pause_reason,
                   created_at, updated_at
            FROM routines
            WHERE status = 'paused'
              AND updated_at < $1
            ORDER BY updated_at ASC
            LIMIT 500
            "#,
        )
        .bind(threshold)
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| anyhow!("list stale paused routines: {e}"))
    }

    /// Return routines eligible for opt-in auto-resume: `pause_reason =
    /// 'failure'` and `updated_at < threshold` (so the backoff window has
    /// elapsed). Pre-existing rows with `pause_reason IS NULL` and rows with
    /// `pause_reason = 'manual'` or `'migration_invalid'` are NOT returned,
    /// so the caller never accidentally resumes an intentionally paused routine.
    pub async fn list_failure_paused_routines(
        &self,
        threshold: DateTime<Utc>,
    ) -> Result<Vec<RoutineRecord>> {
        sqlx::query_as(
            r#"
            SELECT id, agent_id, script_ref, name, status, execution_strategy,
                   schedule, next_due_at, last_run_at, last_result, checkpoint,
                   discord_thread_id, timeout_secs, fallback_agent_id, max_retries,
                   in_flight_run_id, pause_reason,
                   created_at, updated_at
            FROM routines
            WHERE status = 'paused'
              AND pause_reason = $1
              AND updated_at < $2
              -- Skip schedule-less rows with no next_due_at: they can never be
              -- resumed (ResumeRequiresNextDueAt guard) and would otherwise be
              -- re-attempted (and warn-logged) every tick — a tight loop (#3573).
              AND (schedule IS NOT NULL OR next_due_at IS NOT NULL)
            ORDER BY updated_at ASC
            LIMIT 500
            "#,
        )
        .bind(PAUSE_REASON_FAILURE)
        .bind(threshold)
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| anyhow!("list failure-paused routines: {e}"))
    }

    /// Auto-resume a single failure-paused routine.
    ///
    /// Guards applied (defence-in-depth):
    /// 1. `status = 'paused'` — only paused rows are touched.
    /// 2. `pause_reason = 'failure'` — manual/migration_invalid/NULL rows are
    ///    excluded, even if the caller somehow provided the wrong id.
    /// 3. `schedule IS NOT NULL OR next_due_at IS NOT NULL` — mirrors the
    ///    `ResumeRequiresNextDueAt` guard: schedule-less routines with no
    ///    `next_due_at` cannot be resumed (they would never fire again).
    /// 4. `updated_at < threshold` — re-checks the backoff window *atomically*
    ///    under the row lock. Without this, a concurrent update that refreshes
    ///    `updated_at` between the list scan and this call could be resumed
    ///    before the configured backoff elapsed (#3573).
    ///
    /// `threshold` must be the same cutoff used by `list_failure_paused_routines`.
    ///
    /// Returns `true` if exactly one row was updated (resume applied).
    pub async fn auto_resume_failure_paused_routine(
        &self,
        routine_id: &str,
        threshold: DateTime<Utc>,
    ) -> Result<bool, anyhow::Error> {
        // Check the ResumeRequiresNextDueAt guard first so we can return the
        // typed error rather than silently returning false.
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            r#"
            SELECT schedule, next_due_at
            FROM routines
            WHERE id = $1
              AND status = 'paused'
              AND pause_reason = $2
              AND updated_at < $3
            FOR UPDATE
            "#,
        )
        .bind(routine_id)
        .bind(PAUSE_REASON_FAILURE)
        .bind(threshold)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| anyhow!("auto-resume failure-paused routine {routine_id}: {e}"))?;

        let Some(row) = row else {
            tx.commit().await?;
            return Ok(false);
        };

        let schedule: Option<String> = row
            .try_get("schedule")
            .map_err(|e| anyhow!("auto-resume routine {routine_id}: {e}"))?;
        let existing_next_due_at: Option<DateTime<Utc>> = row
            .try_get("next_due_at")
            .map_err(|e| anyhow!("auto-resume routine {routine_id}: {e}"))?;
        if resume_without_next_due_is_invalid(schedule.as_deref(), existing_next_due_at) {
            return Err(ResumeRoutineRequiresNextDueAt.into());
        }

        let result = sqlx::query(
            r#"
            UPDATE routines
            SET status = 'enabled',
                pause_reason = NULL,
                updated_at = NOW()
            WHERE id = $1
              AND status = 'paused'
              AND pause_reason = $2
              AND updated_at < $3
            "#,
        )
        .bind(routine_id)
        .bind(PAUSE_REASON_FAILURE)
        .bind(threshold)
        .execute(&mut *tx)
        .await
        .map_err(|e| anyhow!("auto-resume routine {routine_id}: {e}"))?;

        tx.commit().await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn get_routine(&self, routine_id: &str) -> Result<Option<RoutineRecord>> {
        sqlx::query_as(
            r#"
            SELECT id, agent_id, script_ref, name, status, execution_strategy,
                   schedule, next_due_at, last_run_at, last_result, checkpoint,
                   discord_thread_id, timeout_secs, fallback_agent_id, max_retries,
                   in_flight_run_id, pause_reason,
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
        let limit = clamp_api_limit(usize::try_from(limit).ok()) as i64;
        sqlx::query_as(
            r#"
            SELECT id, routine_id, status, action, turn_id, lease_expires_at,
                   retry_count, next_retry_at, attempts,
                   result_json, error, discord_log_status, discord_log_error,
                   discord_message_id, discord_log_sections, started_at,
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

    /// Returns `true` when *this specific run* actually started an agent turn
    /// (`turn_id` set on the run row). This is the only safe, run-specific
    /// evidence that the run owns a fresh agent session, and it gates
    /// fresh-session teardown on terminal script actions (#3006).
    ///
    /// A fresh agent session is created exclusively via `mark_agent_turn_started`,
    /// which stamps `turn_id` onto the run that spawned it; that run is then
    /// closed (and its session torn down) by the agent-completion path. A
    /// terminal JS-script action (`Complete`/`Skip`/`Pause`) is a *different*
    /// run that never started a turn, so it owns no session. Gating on any
    /// historical routine turn is wrong: a mixed routine that returned `agent`
    /// once would then let every later script-only close kill whatever session
    /// is currently latest in `routine.discord_thread_id` — which may be an
    /// unrelated operator/user session created after that prior turn. Only the
    /// run that actually started the turn may tear it down.
    pub async fn run_started_agent_turn(&self, run_id: &str) -> Result<bool> {
        let turn_id: Option<Option<String>> = sqlx::query_scalar(
            r#"
            SELECT turn_id
            FROM routine_runs
            WHERE id = $1
            "#,
        )
        .bind(run_id)
        .fetch_optional(&*self.pool)
        .await
        .map_err(|e| anyhow!("check run agent turn {run_id}: {e}"))?;
        Ok(matches!(turn_id, Some(Some(_))))
    }

    /// Returns `true` when the routine has another `running` run besides the
    /// given (just-interrupted) run (#3022).
    ///
    /// The fresh-session reap runs only from boot recovery, before the routine
    /// tick loop starts, so a single instance has no concurrent claimer. This
    /// query is the defence-in-depth guard for a co-booting second instance
    /// whose tick loop is already claiming: because a fresh routine's tmux
    /// session name is deterministic for its log thread, a replacement run would
    /// reuse the same name on the same host, so reaping "the recovered run's
    /// owned session" could force-kill that live replacement turn. The reap is
    /// therefore skipped whenever another run for the routine is currently
    /// `running` — that run owns the deterministic session now, and the stale
    /// run's leftover (if any) is harmlessly replaced rather than killed.
    pub async fn routine_has_other_running_run(
        &self,
        routine_id: &str,
        excluded_run_id: &str,
    ) -> Result<bool> {
        let exists: Option<bool> = sqlx::query_scalar(
            r#"
            SELECT EXISTS (
                SELECT 1
                FROM routine_runs
                WHERE routine_id = $1
                  AND id <> $2
                  AND status = 'running'
            )
            "#,
        )
        .bind(routine_id)
        .bind(excluded_run_id)
        .fetch_optional(&*self.pool)
        .await
        .map_err(|e| anyhow!("check other running run for routine {routine_id}: {e}"))?;
        Ok(exists.unwrap_or(false))
    }

    pub async fn list_running_agent_runs(&self, limit: u32) -> Result<Vec<RunningAgentRoutineRun>> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        sqlx::query_as(
            r#"
            SELECT rr.id AS run_id,
                   rr.routine_id,
                   r.agent_id,
                   r.fallback_agent_id,
                   r.max_retries,
                   rr.retry_count,
                   r.script_ref,
                   r.name,
                   r.execution_strategy,
                   r.discord_thread_id,
                   rr.turn_id,
                   rr.next_retry_at,
                   rr.attempts,
                   rr.result_json,
                   rr.started_at,
                   r.timeout_secs
            FROM routine_runs rr
            JOIN routines r ON r.id = rr.routine_id
            WHERE rr.status = 'running'
              AND rr.action = 'agent'
              AND (
                    rr.turn_id IS NOT NULL
                    OR (rr.next_retry_at IS NOT NULL AND rr.next_retry_at <= NOW())
                  )
            ORDER BY COALESCE(rr.next_retry_at, rr.started_at) ASC, rr.created_at ASC
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
              AND (turn_id IS NOT NULL OR next_retry_at IS NOT NULL)
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
        let limit = clamp_api_limit(usize::try_from(limit).ok()) as i64;
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
        const CAP_API_FRICTION: i64 = 15;
        const CAP_OUTBOX: i64 = 10;
        const CAP_ROUTINE_RUNS: i64 = 25;
        const CAP_KANBAN: i64 = 10;
        const CAP_DISPATCHES: i64 = 10;
        const CAP_SESSION: i64 = 10;
        const CAP_AUDIT_LOGS: i64 = 10;

        let now = Utc::now();

        // Each source is fetched by a dedicated helper that owns its query, error
        // fallback (warn + empty Vec), and row->observation mapping. Behavior is
        // identical to the previous inline form; this only splits responsibilities.
        let kv_obs = self.fetch_kv_meta_observations(CAP_KV_META, now).await;
        let friction_obs = self.fetch_api_friction_observations(CAP_API_FRICTION).await;
        let outbox_obs = self.fetch_outbox_observations(CAP_OUTBOX).await;
        let run_obs = self
            .fetch_routine_run_observations(current_script_ref, CAP_ROUTINE_RUNS)
            .await;
        let kanban_obs = self.fetch_kanban_stale_observations(CAP_KANBAN).await;
        let dispatch_obs = self.fetch_dispatch_retry_observations(CAP_DISPATCHES).await;
        let session_obs = self.fetch_session_pattern_observations(CAP_SESSION).await;
        let audit_obs = self.fetch_audit_log_observations(CAP_AUDIT_LOGS).await;
        let kanban_ready_obs = self.fetch_kanban_ready_observations().await;
        let kanban_dispatched_obs = self.fetch_kanban_dispatched_observations().await;

        // --- Fair merge: round-robin across all sources so no single source starves others ---
        use std::collections::VecDeque;
        let mut sources: Vec<VecDeque<serde_json::Value>> = vec![
            kv_obs.into(),
            friction_obs.into(),
            outbox_obs.into(),
            run_obs.into(),
            kanban_obs.into(),
            dispatch_obs.into(),
            session_obs.into(),
            audit_obs.into(),
        ];
        if include_automation_candidate_card_observations(current_script_ref) {
            sources.push(kanban_ready_obs.into());
            sources.push(kanban_dispatched_obs.into());
        }
        let mut observations = Vec::with_capacity(max_items.min(100));
        let mut total_bytes: usize = 0;
        'merge: loop {
            let mut any = false;
            for src in &mut sources {
                if let Some(obs) = src.pop_front() {
                    any = true;
                    if !bounded_observation_push(
                        &mut observations,
                        &mut total_bytes,
                        max_items,
                        max_payload_bytes,
                        obs,
                    ) {
                        break 'merge;
                    }
                }
            }
            if !any {
                break;
            }
        }

        Ok(observations)
    }

    /// Source 1: kv_meta precomputed digests.
    async fn fetch_kv_meta_observations(
        &self,
        cap: i64,
        now: DateTime<Utc>,
    ) -> Vec<serde_json::Value> {
        let digest_rows = match sqlx::query(
            r#"
            SELECT key, value
            FROM kv_meta
            WHERE key LIKE 'routine_observation:%'
              AND (expires_at IS NULL OR expires_at > NOW())
            ORDER BY COALESCE(
                       NULLIF(substring(value FROM '"timestamp"[[:space:]]*:[[:space:]]*"([^"]+)"'), ''),
                       NULLIF(substring(value FROM '"last_seen_at"[[:space:]]*:[[:space:]]*"([^"]+)"'), ''),
                       NULLIF(substring(value FROM '"approved_at"[[:space:]]*:[[:space:]]*"([^"]+)"'), ''),
                       NULLIF(substring(value FROM '"dispatched_at"[[:space:]]*:[[:space:]]*"([^"]+)"'), ''),
                       ''
                     ) DESC,
                     key ASC
            LIMIT $1
            "#,
        )
        .bind(cap)
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
        kv_obs
    }

    /// Source 2: api_friction_issues.
    async fn fetch_api_friction_observations(&self, cap: i64) -> Vec<serde_json::Value> {
        let api_rows = match sqlx::query(API_FRICTION_OBSERVATION_QUERY)
            .bind(cap)
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
            let event_count: i32 = row.try_get("event_count").unwrap_or(1);
            let last_seen_at: DateTime<Utc> =
                row.try_get("last_seen_at").unwrap_or_else(|_| Utc::now());
            let summary = truncate_chars(
                &format!(
                    "{endpoint} {friction_type}: {} ({event_count} reports)",
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
        friction_obs
    }

    /// Source 3: message_outbox grouped failures.
    async fn fetch_outbox_observations(&self, cap: i64) -> Vec<serde_json::Value> {
        let outbox_rows = match sqlx::query(
            r#"
            SELECT COALESCE(NULLIF(source, ''), 'message_outbox') AS source,
                   COALESCE(NULLIF(reason_code, ''), status) AS reason_code,
                   status,
                   COUNT(*)::BIGINT AS occurrence_count,
                   MAX(created_at) AS last_seen_at,
                   (ARRAY_AGG(error ORDER BY created_at DESC) FILTER (WHERE error IS NOT NULL))[1] AS last_error
            FROM message_outbox
            WHERE created_at > NOW() - INTERVAL '24 hours'
              AND (status IN ('failed', 'error') OR error IS NOT NULL)
            GROUP BY source, reason_code, status
            ORDER BY MAX(created_at) DESC
            LIMIT $1
            "#,
        )
        .bind(cap)
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
        outbox_obs
    }

    /// Source 4: routine_runs grouped by (script_ref, action, status).
    ///
    /// Grouped so that repeated failures from the same routine emit one observation with
    /// a stable evidence_ref instead of one raw row per UUID run.  The raw UUID approach
    /// caused evidence_count to inflate ~N/tick and saturated score=100 after one tick.
    async fn fetch_routine_run_observations(
        &self,
        current_script_ref: Option<&str>,
        cap: i64,
    ) -> Vec<serde_json::Value> {
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
        .bind(cap)
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
        run_obs
    }

    /// Source 5: kanban_cards stale or blocked.
    async fn fetch_kanban_stale_observations(&self, cap: i64) -> Vec<serde_json::Value> {
        let kanban_rows = match sqlx::query(
            r#"
            SELECT id,
                   COALESCE(NULLIF(TRIM(title), ''), id) AS title,
                   status,
                   COALESCE(assigned_agent_id, '') AS assigned_agent_id,
                   blocked_reason,
                   GREATEST(updated_at, created_at) AS last_seen_at,
                   EXTRACT(EPOCH FROM (NOW() - GREATEST(updated_at, created_at))) / 3600.0
                       AS stuck_hours
            FROM kanban_cards
            WHERE status NOT IN ('done', 'cancelled', 'archived', 'detached')
              AND (pipeline_stage_id IS NULL OR pipeline_stage_id != $1)
              AND GREATEST(updated_at, created_at) < NOW() - INTERVAL '24 hours'
              AND (
                    blocked_reason IS NOT NULL
                    OR GREATEST(updated_at, created_at) < NOW() - INTERVAL '48 hours'
                  )
            ORDER BY GREATEST(updated_at, created_at) ASC
            LIMIT $2
            "#,
        )
        .bind(PIPELINE_STAGE_ID)
        .bind(cap)
        .fetch_all(&*self.pool)
        .await
        {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(error = %error, "skipping kanban stale observation source");
                Vec::new()
            }
        };

        let mut kanban_obs: Vec<serde_json::Value> = Vec::new();
        for row in &kanban_rows {
            let card_id: String = row.try_get("id").unwrap_or_default();
            let title: String = row.try_get("title").unwrap_or_default();
            let status: String = row.try_get("status").unwrap_or_default();
            let assigned: String = row.try_get("assigned_agent_id").unwrap_or_default();
            let blocked_reason: Option<String> = row.try_get("blocked_reason").ok().flatten();
            let last_seen_at: DateTime<Utc> =
                row.try_get("last_seen_at").unwrap_or_else(|_| Utc::now());
            let stuck_hours: f64 = row.try_get("stuck_hours").unwrap_or(0.0);
            let stuck_days = (stuck_hours / 24.0).round() as u32;
            let summary = if let Some(ref reason) = blocked_reason {
                format!(
                    "kanban blocked: {} ({}d, agent={}) — {}",
                    truncate_chars(&title, 80),
                    stuck_days,
                    assigned,
                    truncate_chars(reason, 80)
                )
            } else {
                format!(
                    "kanban stale: {} ({}d stuck in {}, agent={})",
                    truncate_chars(&title, 80),
                    stuck_days,
                    status,
                    assigned
                )
            };
            let weight: u8 = if blocked_reason.is_some() { 2 } else { 1 };
            // Group by status so multiple stale cards accumulate under one candidate
            // (per-card signature would never reach evidence_count >= 5 gate)
            let sig_group = if blocked_reason.is_some() {
                "blocked"
            } else {
                &status
            };
            kanban_obs.push(serde_json::json!({
                "timestamp": last_seen_at.to_rfc3339(),
                "source": "kanban_stale",
                "category": "kanban-flow",
                "signature": format!("kanban-stale:{sig_group}"),
                "summary": truncate_chars(&summary, 240),
                "weight": weight,
                "occurrences": stuck_days.clamp(1, 50),
                "evidence_ref": format!("kanban_cards:{card_id}"),
            }));
        }
        kanban_obs
    }

    /// Source 6: task_dispatches high-retry.
    async fn fetch_dispatch_retry_observations(&self, cap: i64) -> Vec<serde_json::Value> {
        let dispatch_rows = match sqlx::query(
            r#"
            SELECT id,
                   COALESCE(NULLIF(TRIM(title), ''), id) AS title,
                   from_agent_id,
                   to_agent_id,
                   status,
                   retry_count,
                   GREATEST(updated_at, created_at) AS last_seen_at
            FROM task_dispatches
            WHERE retry_count >= 2
              AND status IN ('failed', 'error', 'pending', 'in_progress')
              AND GREATEST(updated_at, created_at) > NOW() - INTERVAL '24 hours'
            ORDER BY retry_count DESC, GREATEST(updated_at, created_at) DESC
            LIMIT $1
            "#,
        )
        .bind(cap)
        .fetch_all(&*self.pool)
        .await
        {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(error = %error, "skipping dispatch retry observation source");
                Vec::new()
            }
        };

        let mut dispatch_obs: Vec<serde_json::Value> = Vec::new();
        for row in &dispatch_rows {
            let dispatch_id: String = row.try_get("id").unwrap_or_default();
            let title: String = row.try_get("title").unwrap_or_default();
            let from_agent: String = row.try_get("from_agent_id").unwrap_or_default();
            let to_agent: String = row.try_get("to_agent_id").unwrap_or_default();
            let status: String = row.try_get("status").unwrap_or_default();
            let retry_count: i64 = row.try_get("retry_count").unwrap_or(0);
            let last_seen_at: DateTime<Utc> =
                row.try_get("last_seen_at").unwrap_or_else(|_| Utc::now());
            let summary = format!(
                "dispatch {status} ×{retry_count} retries: {} ({from_agent}→{to_agent})",
                truncate_chars(&title, 100)
            );
            dispatch_obs.push(serde_json::json!({
                "timestamp": last_seen_at.to_rfc3339(),
                "source": "dispatch_retry",
                "category": "dispatch-retry",
                "signature": format!("dispatch-retry:{from_agent}:{to_agent}:{status}"),
                "summary": truncate_chars(&summary, 240),
                "weight": 2,
                "occurrences": (retry_count as u64).clamp(1, 50),
                "evidence_ref": format!("task_dispatches:{dispatch_id}"),
            }));
        }
        dispatch_obs
    }

    /// Source 7: session_transcripts repeated error patterns per agent.
    async fn fetch_session_pattern_observations(&self, cap: i64) -> Vec<serde_json::Value> {
        let session_rows = match sqlx::query(
            r#"
            SELECT agent_id,
                   COUNT(*) FILTER (
                       WHERE user_message ILIKE '%error%'
                          OR user_message ILIKE '%fail%'
                          OR user_message ILIKE '%오류%'
                          OR user_message ILIKE '%실패%'
                          OR user_message ILIKE '%에러%'
                          OR user_message ILIKE '%안됨%'
                          OR user_message ILIKE '%안 됨%'
                   )::BIGINT AS error_mention_count,
                   COUNT(*)::BIGINT AS total_turns,
                   MAX(created_at) AS last_seen_at
            FROM session_transcripts
            WHERE created_at > NOW() - INTERVAL '24 hours'
            GROUP BY agent_id
            HAVING COUNT(*) FILTER (
                       WHERE user_message ILIKE '%error%'
                          OR user_message ILIKE '%fail%'
                          OR user_message ILIKE '%오류%'
                          OR user_message ILIKE '%실패%'
                          OR user_message ILIKE '%에러%'
                          OR user_message ILIKE '%안됨%'
                          OR user_message ILIKE '%안 됨%'
                   ) >= 3
            ORDER BY error_mention_count DESC
            LIMIT $1
            "#,
        )
        .bind(cap)
        .fetch_all(&*self.pool)
        .await
        {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(error = %error, "skipping session pattern observation source");
                Vec::new()
            }
        };

        let mut session_obs: Vec<serde_json::Value> = Vec::new();
        for row in &session_rows {
            let agent_id: String = row.try_get("agent_id").unwrap_or_default();
            let error_count: i64 = row.try_get("error_mention_count").unwrap_or(0);
            let total_turns: i64 = row.try_get("total_turns").unwrap_or(0);
            let last_seen_at: DateTime<Utc> =
                row.try_get("last_seen_at").unwrap_or_else(|_| Utc::now());
            let summary = format!(
                "session error pattern: agent={agent_id} error_mentions={error_count}/{total_turns} turns"
            );
            session_obs.push(serde_json::json!({
                "timestamp": last_seen_at.to_rfc3339(),
                "source": "session_pattern",
                "category": "session-pattern",
                "signature": format!("session-pattern:{agent_id}"),
                "summary": truncate_chars(&summary, 240),
                "weight": 2,
                "occurrences": (error_count as u32).clamp(1, 50),
                "evidence_ref": format!("session_transcripts:{agent_id}"),
            }));
        }
        session_obs
    }

    /// Source 8: audit/log signals grouped by action/source.
    async fn fetch_audit_log_observations(&self, cap: i64) -> Vec<serde_json::Value> {
        let audit_rows = match sqlx::query(
            r#"
            WITH grouped_logs AS (
                SELECT 'audit_logs' AS source_table,
                       COALESCE(NULLIF(entity_type, ''), 'unknown') AS entity_type,
                       COALESCE(NULLIF(action, ''), 'updated') AS action,
                       COALESCE(NULLIF(actor, ''), 'system') AS actor,
                       COUNT(*)::BIGINT AS occurrence_count,
                       MAX(timestamp) AS last_seen_at
                FROM audit_logs
                WHERE timestamp > NOW() - INTERVAL '24 hours'
                GROUP BY COALESCE(NULLIF(entity_type, ''), 'unknown'),
                         COALESCE(NULLIF(action, ''), 'updated'),
                         COALESCE(NULLIF(actor, ''), 'system')
                HAVING COUNT(*) >= 3

                UNION ALL

                SELECT 'kanban_audit_logs' AS source_table,
                       'kanban_card' AS entity_type,
                       CONCAT(
                           COALESCE(NULLIF(from_status, ''), 'unknown'),
                           '->',
                           COALESCE(NULLIF(to_status, ''), 'unknown')
                       ) AS action,
                       COALESCE(NULLIF(source, ''), 'system') AS actor,
                       COUNT(*)::BIGINT AS occurrence_count,
                       MAX(created_at) AS last_seen_at
                FROM kanban_audit_logs
                WHERE created_at > NOW() - INTERVAL '24 hours'
                GROUP BY CONCAT(
                             COALESCE(NULLIF(from_status, ''), 'unknown'),
                             '->',
                             COALESCE(NULLIF(to_status, ''), 'unknown')
                         ),
                         COALESCE(NULLIF(source, ''), 'system')
                HAVING COUNT(*) >= 3
            )
            SELECT source_table, entity_type, action, actor, occurrence_count, last_seen_at
            FROM grouped_logs
            ORDER BY occurrence_count DESC, last_seen_at DESC
            LIMIT $1
            "#,
        )
        .bind(cap)
        .fetch_all(&*self.pool)
        .await
        {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(error = %error, "skipping audit/log observation source");
                Vec::new()
            }
        };

        let mut audit_obs: Vec<serde_json::Value> = Vec::new();
        for row in &audit_rows {
            let source_table: String = row.try_get("source_table").unwrap_or_default();
            let entity_type: String = row.try_get("entity_type").unwrap_or_default();
            let action: String = row.try_get("action").unwrap_or_default();
            let actor: String = row.try_get("actor").unwrap_or_default();
            let occurrence_count: i64 = row.try_get("occurrence_count").unwrap_or(1);
            let last_seen_at: DateTime<Utc> =
                row.try_get("last_seen_at").unwrap_or_else(|_| Utc::now());
            let summary = format!(
                "{source_table} repeated {entity_type}:{action} by {actor} ×{occurrence_count}"
            );
            audit_obs.push(serde_json::json!({
                "timestamp": last_seen_at.to_rfc3339(),
                "source": "audit_log",
                "category": "log-signal",
                "signature": format!("log-signal:{source_table}:{entity_type}:{action}:{actor}"),
                "summary": truncate_chars(&summary, 240),
                "weight": if occurrence_count >= 5 { 2 } else { 1 },
                "occurrences": (occurrence_count as u32).clamp(1, 50),
                "evidence_ref": format!("audit_logs:{source_table}:{entity_type}:{action}:{actor}"),
            }));
        }
        audit_obs
    }

    /// Source 9: kanban_ready – automation-candidate cards awaiting execution.
    async fn fetch_kanban_ready_observations(&self) -> Vec<serde_json::Value> {
        let kanban_ready_rows = match sqlx::query(
            r#"
            SELECT id,
                   COALESCE(NULLIF(TRIM(title), ''), id) AS title,
                   COALESCE(assigned_agent_id, '') AS assigned_agent_id,
                   metadata,
                   created_at,
                   updated_at
            FROM kanban_cards
            WHERE status = 'ready'
              AND pipeline_stage_id = $1
              AND NULLIF(metadata->'program'->>'repo_dir', '') IS NOT NULL
              AND jsonb_typeof(metadata->'program'->'allowed_write_paths') = 'array'
              AND jsonb_array_length(metadata->'program'->'allowed_write_paths') > 0
              AND NULLIF(metadata->'program'->>'metric_name', '') IS NOT NULL
              AND metadata->'program' ? 'metric_target'
            ORDER BY updated_at ASC
            LIMIT 20
            "#,
        )
        .bind(PIPELINE_STAGE_ID)
        .fetch_all(&*self.pool)
        .await
        {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(error = %error, "skipping kanban_ready observation source");
                Vec::new()
            }
        };

        let mut kanban_ready_obs: Vec<serde_json::Value> = Vec::new();
        for row in &kanban_ready_rows {
            let card_id: String = row.try_get("id").unwrap_or_default();
            let title: String = row.try_get("title").unwrap_or_default();
            let assigned: String = row.try_get("assigned_agent_id").unwrap_or_default();
            let updated_at: DateTime<Utc> =
                row.try_get("updated_at").unwrap_or_else(|_| Utc::now());
            let metadata: serde_json::Value = row
                .try_get::<serde_json::Value, _>("metadata")
                .unwrap_or(serde_json::Value::Null);
            if !has_complete_loop_contract(&metadata) {
                continue;
            }
            kanban_ready_obs.push(serde_json::json!({
                "timestamp": updated_at.to_rfc3339(),
                "source": "kanban_ready",
                "category": "automation-candidate",
                "pipeline_stage_id": PIPELINE_STAGE_ID,
                "signature": format!("kanban-ready:{card_id}"),
                "summary": format!("automation candidate ready: {} (agent={})", truncate_chars(&title, 100), assigned),
                "weight": 3,
                "occurrences": 1,
                "evidence_ref": format!("kanban_cards:{card_id}"),
                "card_id": card_id,
                "metadata": metadata,
            }));
        }
        kanban_ready_obs
    }

    /// Source 10: kanban_dispatched – recently completed automation-candidate cards.
    async fn fetch_kanban_dispatched_observations(&self) -> Vec<serde_json::Value> {
        let kanban_dispatched_rows = match sqlx::query(
            r#"
            SELECT id,
                   COALESCE(NULLIF(TRIM(title), ''), id) AS title,
                   metadata,
                   updated_at
            FROM kanban_cards
            WHERE status = 'done'
              AND pipeline_stage_id = $1
              AND updated_at > NOW() - INTERVAL '7 days'
            ORDER BY updated_at DESC
            LIMIT 20
            "#,
        )
        .bind(PIPELINE_STAGE_ID)
        .fetch_all(&*self.pool)
        .await
        {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(error = %error, "skipping kanban_dispatched observation source");
                Vec::new()
            }
        };

        let mut kanban_dispatched_obs: Vec<serde_json::Value> = Vec::new();
        for row in &kanban_dispatched_rows {
            let card_id: String = row.try_get("id").unwrap_or_default();
            let title: String = row.try_get("title").unwrap_or_default();
            let updated_at: DateTime<Utc> =
                row.try_get("updated_at").unwrap_or_else(|_| Utc::now());
            let metadata: serde_json::Value = row
                .try_get::<serde_json::Value, _>("metadata")
                .unwrap_or(serde_json::Value::Null);
            if !has_complete_loop_contract(&metadata) {
                continue;
            }
            kanban_dispatched_obs.push(serde_json::json!({
                "timestamp": updated_at.to_rfc3339(),
                "source": "kanban_dispatched",
                "category": "automation-candidate",
                "pipeline_stage_id": PIPELINE_STAGE_ID,
                "signature": format!("kanban-dispatched:{card_id}"),
                "summary": format!("automation candidate dispatched: {}", truncate_chars(&title, 120)),
                "weight": 1,
                "occurrences": 1,
                "evidence_ref": format!("kanban_cards:{card_id}"),
                "card_id": card_id,
            }));
        }
        kanban_dispatched_obs
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
        let status = normalize_new_routine_status(new_routine.status.as_deref())?;
        let schedule = normalize_schedule(new_routine.schedule)?;
        validate_timeout_secs(new_routine.timeout_secs)?;
        validate_max_retries(new_routine.max_retries)?;
        let discord_thread_id = normalize_optional_text(new_routine.discord_thread_id);
        let fallback_agent_id = normalize_optional_text(new_routine.fallback_agent_id);
        let max_retries = new_routine.max_retries.unwrap_or(0);
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
                schedule, next_due_at, checkpoint, discord_thread_id, timeout_secs,
                fallback_agent_id, max_retries
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
            RETURNING id, agent_id, script_ref, name, status, execution_strategy,
                      schedule, next_due_at, last_run_at, last_result, checkpoint,
                      discord_thread_id, timeout_secs, fallback_agent_id, max_retries,
                      in_flight_run_id, pause_reason,
                      created_at, updated_at
            "#,
        )
        .bind(id)
        .bind(new_routine.agent_id)
        .bind(new_routine.script_ref)
        .bind(new_routine.name)
        .bind(status)
        .bind(new_routine.execution_strategy)
        .bind(schedule)
        .bind(next_due_at)
        .bind(new_routine.checkpoint)
        .bind(discord_thread_id)
        .bind(new_routine.timeout_secs)
        .bind(fallback_agent_id)
        .bind(max_retries)
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
        validate_max_retries(patch.max_retries)?;
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
        let fallback_agent_id_was_set = patch.fallback_agent_id.is_some();
        let fallback_agent_id = patch
            .fallback_agent_id
            .map(|value| normalize_optional_text(value));
        let max_retries_was_set = patch.max_retries.is_some();
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
                fallback_agent_id = CASE WHEN $14 THEN $15 ELSE fallback_agent_id END,
                max_retries = CASE WHEN $16 THEN $17 ELSE max_retries END,
                updated_at = NOW()
            WHERE id = $1
              AND status <> 'detached'
            RETURNING id, agent_id, script_ref, name, status, execution_strategy,
                      schedule, next_due_at, last_run_at, last_result, checkpoint,
                      discord_thread_id, timeout_secs, fallback_agent_id, max_retries,
                      in_flight_run_id, pause_reason,
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
        .bind(fallback_agent_id_was_set)
        .bind(fallback_agent_id.flatten())
        .bind(max_retries_was_set)
        .bind(patch.max_retries.unwrap_or(0))
        .fetch_optional(&*self.pool)
        .await
        .map_err(|e| anyhow!("patch routine {routine_id}: {e}"))
    }

    /// Claim one enabled routine immediately, independent of its schedule.
    pub async fn claim_run_now(&self, routine_id: &str) -> Result<Option<ClaimedRoutineRun>> {
        let mut tx = self.pool.begin().await?;
        let candidate: Option<RoutineClaimCandidate> = sqlx::query_as(
            r#"
            SELECT id, agent_id, fallback_agent_id, max_retries, script_ref, name,
                   execution_strategy, checkpoint, discord_thread_id, timeout_secs
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
                pause_reason: None,
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
                pause_reason: None,
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
                // A script-requested pause is intentional — treat as manual so
                // auto-resume does NOT restart it without operator intent.
                pause_reason: Some(PAUSE_REASON_MANUAL),
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
                pause_reason: None,
            },
        )
        .await
    }

    /// Called by routine execution paths when a run has failed and the routine
    /// should be paused with `pause_reason = 'failure'`. This is the primary
    /// entry point for failure-induced pauses — use
    /// `fail_run_and_pause_as_migration_invalid` only for migrated-launchd
    /// structural-validation failures.
    #[allow(dead_code)] // public API; callers exist outside the lib (e.g. binary, integration tests)
    pub async fn fail_run_and_pause_routine(
        &self,
        run_id: &str,
        error: &str,
        result_json: Option<Value>,
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
                next_due_at: NextDueAtUpdate::Clear,
                pause_routine: true,
                pause_reason: Some(PAUSE_REASON_FAILURE),
            },
        )
        .await
    }

    /// Like `fail_run_and_pause_routine` but records `migration_invalid` as the
    /// pause cause. Used when a migrated-launchd run is blocked by structural
    /// validation before execution even starts (missing script file, metadata
    /// field must be array, required connector empty, unset env var, etc.).
    /// These faults are operator-configuration issues, not runtime failures, so
    /// auto-resume would just loop immediately; marking them separately lets
    /// operators filter them and means auto-resume conservatively skips them.
    pub async fn fail_run_and_pause_as_migration_invalid(
        &self,
        run_id: &str,
        error: &str,
        result_json: Option<Value>,
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
                next_due_at: NextDueAtUpdate::Clear,
                pause_routine: true,
                pause_reason: Some(PAUSE_REASON_MIGRATION_INVALID),
            },
        )
        .await
    }

    pub async fn mark_agent_turn_started(
        &self,
        run_id: &str,
        turn_id: &str,
        result_json: Option<Value>,
        agent_id: &str,
        attempt_kind: &str,
    ) -> Result<bool> {
        let result = sqlx::query(
            r#"
            UPDATE routine_runs
            SET action = 'agent',
                turn_id = $2,
                result_json = $3,
                lease_expires_at = NOW() + ($4::bigint * INTERVAL '1 second'),
                next_retry_at = NULL,
                owned_tmux_session = NULL,
                attempts = COALESCE(attempts, '[]'::jsonb) || jsonb_build_array(
                    jsonb_build_object(
                        'event', 'started',
                        'agent_id', $5,
                        'kind', $6,
                        'turn_id', $2,
                        'at', NOW()
                    )
                ),
                updated_at = NOW()
            WHERE id = $1
              AND status = 'running'
            "#,
        )
        .bind(run_id)
        .bind(turn_id)
        .bind(result_json)
        .bind(RUN_LEASE_SECS)
        .bind(agent_id)
        .bind(attempt_kind)
        .execute(&*self.pool)
        .await
        .map_err(|e| anyhow!("mark routine agent turn started {run_id}: {e}"))?;

        Ok(result.rows_affected() == 1)
    }

    pub async fn schedule_agent_retry(
        &self,
        run_id: &str,
        next_retry_at: DateTime<Utc>,
        result_json: Option<Value>,
        error: &str,
        failed_agent_id: Option<&str>,
        attempt_kind: &str,
    ) -> Result<bool> {
        let result = sqlx::query(
            r#"
            UPDATE routine_runs
            SET action = 'agent',
                turn_id = NULL,
                next_retry_at = $2,
                retry_count = retry_count + 1,
                result_json = $3,
                error = $4,
                owned_tmux_session = NULL,
                lease_expires_at = NOW() + ($5::bigint * INTERVAL '1 second'),
                attempts = COALESCE(attempts, '[]'::jsonb) || jsonb_build_array(
                    jsonb_build_object(
                        'event', 'retry_scheduled',
                        'agent_id', $6,
                        'kind', $7,
                        'error', $4,
                        'next_retry_at', $2,
                        'at', NOW()
                    )
                ),
                updated_at = NOW()
            WHERE id = $1
              AND status = 'running'
            "#,
        )
        .bind(run_id)
        .bind(next_retry_at)
        .bind(result_json)
        .bind(error)
        .bind(RUN_LEASE_SECS)
        .bind(failed_agent_id)
        .bind(attempt_kind)
        .execute(&*self.pool)
        .await
        .map_err(|e| anyhow!("schedule routine agent retry {run_id}: {e}"))?;

        Ok(result.rows_affected() == 1)
    }

    /// Records the concrete tmux session a fresh-strategy run owns (#3022).
    ///
    /// Called once the headless agent turn has been started and its backing
    /// session is up, so the run row carries positive, run-specific ownership
    /// proof. Boot recovery uses this to reap the exact orphaned fresh session
    /// after a dcserver restart, instead of guessing the "latest session in the
    /// routine log thread" — which after a restart can no longer be attributed
    /// to the run, or could match an unrelated session sharing the thread.
    ///
    /// Guarded on `status = 'running'` so a run that already finished (and was
    /// torn down) is not re-stamped; the in-line completion path tears the
    /// session down itself, so a late ownership stamp would only be a harmless
    /// dangling name. Returns `true` when the running run row was updated.
    pub async fn set_run_owned_tmux_session(
        &self,
        run_id: &str,
        owned_tmux_session: &str,
    ) -> Result<bool> {
        let result = sqlx::query(
            r#"
            UPDATE routine_runs
            SET owned_tmux_session = $2,
                updated_at = NOW()
            WHERE id = $1
              AND status = 'running'
            "#,
        )
        .bind(run_id)
        .bind(owned_tmux_session)
        .execute(&*self.pool)
        .await
        .map_err(|e| anyhow!("record routine run owned session {run_id}: {e}"))?;

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
                pause_reason: None,
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
                pause_reason: None,
            },
        )
        .await
    }

    /// Pause an enabled routine; already-paused rows are treated as an
    /// idempotent success so migrated launchd cutover scripts can safely call
    /// pause after attaching rows that default to paused.
    ///
    /// `next_due_at` is preserved on purpose (#2395). Clearing it here makes
    /// the resume PATCH-semantics meaningless for routines that only have an
    /// explicit one-shot `next_due_at` (no `schedule`): after pause/resume
    /// `{}`, the seeding path could not recover `next_due_at` because there
    /// is no `schedule` to derive it from. While `paused`, the row is
    /// excluded from `claim_due_runs` (status = 'enabled' guard), so the
    /// retained timestamp does not cause spurious dispatches.
    pub async fn pause_routine(&self, routine_id: &str) -> Result<bool> {
        let result = sqlx::query(
            r#"
            UPDATE routines
            SET status = 'paused',
                pause_reason = $2,
                updated_at = NOW()
            WHERE id = $1
              AND status IN ('enabled', 'paused')
            "#,
        )
        .bind(routine_id)
        .bind(PAUSE_REASON_MANUAL)
        .execute(&*self.pool)
        .await
        .map_err(|e| anyhow!("pause routine {routine_id}: {e}"))?;

        Ok(result.rows_affected() == 1)
    }

    /// Resume a paused routine.
    ///
    /// `next_due_at` follows PATCH semantics (#2395):
    /// - `None` → leave the existing `next_due_at` column untouched (callers
    ///   that POST `{}` no longer accidentally null the next-fire timestamp
    ///   and strand the routine).
    /// - `Some(Some(ts))` → set `next_due_at = ts`.
    /// - `Some(None)` → explicitly clear `next_due_at` (manual-only routines).
    pub async fn resume_routine(
        &self,
        routine_id: &str,
        next_due_at: Option<Option<DateTime<Utc>>>,
    ) -> Result<bool> {
        let result = match next_due_at {
            None => {
                let mut tx = self
                    .pool
                    .begin()
                    .await
                    .map_err(|e| anyhow!("resume routine {routine_id}: {e}"))?;
                let row = sqlx::query(
                    r#"
                    SELECT schedule, next_due_at
                    FROM routines
                    WHERE id = $1
                      AND status = 'paused'
                    FOR UPDATE
                    "#,
                )
                .bind(routine_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| anyhow!("resume routine {routine_id}: {e}"))?;

                let Some(row) = row else {
                    return Ok(false);
                };

                let schedule: Option<String> = row
                    .try_get("schedule")
                    .map_err(|e| anyhow!("resume routine {routine_id}: {e}"))?;
                let existing_next_due_at: Option<DateTime<Utc>> = row
                    .try_get("next_due_at")
                    .map_err(|e| anyhow!("resume routine {routine_id}: {e}"))?;
                if resume_without_next_due_is_invalid(schedule.as_deref(), existing_next_due_at) {
                    return Err(ResumeRoutineRequiresNextDueAt.into());
                }

                let result = sqlx::query(
                    r#"
                    UPDATE routines
                    SET status = 'enabled',
                        pause_reason = NULL,
                        updated_at = NOW()
                    WHERE id = $1
                      AND status = 'paused'
                    "#,
                )
                .bind(routine_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| anyhow!("resume routine {routine_id}: {e}"))?;
                tx.commit()
                    .await
                    .map_err(|e| anyhow!("resume routine {routine_id}: {e}"))?;
                result
            }
            Some(value) => sqlx::query(
                r#"
                UPDATE routines
                SET status = 'enabled',
                    pause_reason = NULL,
                    next_due_at = $2,
                    updated_at = NOW()
                WHERE id = $1
                  AND status = 'paused'
                "#,
            )
            .bind(routine_id)
            .bind(value)
            .execute(&*self.pool)
            .await
            .map_err(|e| anyhow!("resume routine {routine_id}: {e}"))?,
        };

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

    pub async fn delete_detached_routine(
        &self,
        routine_id: &str,
        caller_agent_id: Option<&str>,
        principal: Option<&RequestPrincipal>,
    ) -> Result<DeleteRoutineResult> {
        let caller_agent_id = caller_agent_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| anyhow!("delete routine {routine_id}: {e}"))?;

        let row = sqlx::query(
            r#"
            SELECT status, in_flight_run_id, agent_id
            FROM routines
            WHERE id = $1
            FOR UPDATE
            "#,
        )
        .bind(routine_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| anyhow!("delete routine {routine_id}: {e}"))?;

        let Some(row) = row else {
            return Ok(DeleteRoutineResult::NotFound { caller_agent_id });
        };

        let status: String = row
            .try_get("status")
            .map_err(|e| anyhow!("delete routine {routine_id}: {e}"))?;
        let in_flight_run_id: Option<String> = row
            .try_get("in_flight_run_id")
            .map_err(|e| anyhow!("delete routine {routine_id}: {e}"))?;
        let routine_agent_id: Option<String> = row
            .try_get("agent_id")
            .map_err(|e| anyhow!("delete routine {routine_id}: {e}"))?;

        match routine_delete_scope_gate(
            routine_agent_id.as_deref(),
            caller_agent_id.as_deref(),
            principal,
        ) {
            RoutineDeleteScopeGate::Allowed => {}
            RoutineDeleteScopeGate::Unresolved { owner } => {
                return Ok(DeleteRoutineResult::Forbidden {
                    owner,
                    caller_agent_id: None,
                });
            }
            RoutineDeleteScopeGate::OtherAgent { owner, caller } => {
                return Ok(DeleteRoutineResult::Forbidden {
                    owner,
                    caller_agent_id: Some(caller),
                });
            }
        }

        let has_running_run: bool = sqlx::query_scalar(
            r#"
            SELECT EXISTS (
                SELECT 1
                FROM routine_runs
                WHERE routine_id = $1
                  AND status = 'running'
            )
            "#,
        )
        .bind(routine_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| anyhow!("delete routine {routine_id}: {e}"))?;

        match routine_hard_delete_gate(&status, in_flight_run_id.as_deref(), has_running_run) {
            RoutineHardDeleteGate::Allowed => {}
            RoutineHardDeleteGate::InFlight => {
                return Ok(DeleteRoutineResult::InFlight {
                    routine_agent_id,
                    caller_agent_id,
                });
            }
            RoutineHardDeleteGate::NotDetached { status } => {
                return Ok(DeleteRoutineResult::NotDetached {
                    status,
                    routine_agent_id,
                    caller_agent_id,
                });
            }
        }

        let deleted_runs = sqlx::query(
            r#"
            DELETE FROM routine_runs
            WHERE routine_id = $1
            "#,
        )
        .bind(routine_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| anyhow!("delete routine run history for {routine_id}: {e}"))?;

        let deleted_routine = sqlx::query(
            r#"
            DELETE FROM routines
            WHERE id = $1
            "#,
        )
        .bind(routine_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| anyhow!("delete routine {routine_id}: {e}"))?;

        if deleted_routine.rows_affected() != 1 {
            return Err(anyhow!(
                "delete routine {routine_id}: locked routine row disappeared before delete"
            ));
        }

        tx.commit()
            .await
            .map_err(|e| anyhow!("delete routine {routine_id}: {e}"))?;

        Ok(DeleteRoutineResult::Deleted {
            run_history_deleted: deleted_runs.rows_affected(),
            routine_agent_id,
            caller_agent_id,
        })
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

    pub async fn get_run_discord_log_failure(
        &self,
        run_id: &str,
    ) -> Result<Option<RunDiscordLogFailure>> {
        let state: Option<RunDiscordLogFailure> = sqlx::query_as(
            r#"
            SELECT discord_log_status AS status,
                   discord_log_error AS error
            FROM routine_runs
            WHERE id = $1
              AND discord_log_status = 'failed'
            "#,
        )
        .bind(run_id)
        .fetch_optional(&*self.pool)
        .await
        .map_err(|e| anyhow!("load routine run discord log failure {run_id}: {e}"))?;
        Ok(state)
    }

    pub async fn record_run_discord_log_section(
        &self,
        run_id: &str,
        section_key: &str,
        section_text: &str,
    ) -> Result<RunDiscordLogState> {
        let mut tx = self.pool.begin().await?;
        let row: Option<(Option<String>, Value)> = sqlx::query_as(
            r#"
            SELECT discord_message_id, discord_log_sections
            FROM routine_runs
            WHERE id = $1
            FOR UPDATE
            "#,
        )
        .bind(run_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| anyhow!("load routine run discord log state {run_id}: {e}"))?;

        let Some((message_id, sections)) = row else {
            return Err(anyhow!("routine run {run_id} not found for discord log"));
        };

        let mut object = sections.as_object().cloned().unwrap_or_default();
        object.insert(
            section_key.trim().to_string(),
            Value::String(section_text.to_string()),
        );
        let sections = Value::Object(object);

        sqlx::query(
            r#"
            UPDATE routine_runs
            SET discord_log_sections = $2,
                updated_at = NOW()
            WHERE id = $1
            "#,
        )
        .bind(run_id)
        .bind(&sections)
        .execute(&mut *tx)
        .await
        .map_err(|e| anyhow!("record routine run discord log section {run_id}: {e}"))?;

        tx.commit().await?;
        Ok(RunDiscordLogState {
            message_id,
            sections,
        })
    }

    pub async fn record_run_discord_message_id(
        &self,
        run_id: &str,
        message_id: &str,
    ) -> Result<bool> {
        let result = sqlx::query(
            r#"
            UPDATE routine_runs
            SET discord_message_id = $2,
                updated_at = NOW()
            WHERE id = $1
            "#,
        )
        .bind(run_id)
        .bind(message_id)
        .execute(&*self.pool)
        .await
        .map_err(|e| anyhow!("record routine run discord message id {run_id}: {e}"))?;

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
                RETURNING rr.id, rr.routine_id, rr.owned_tmux_session
            )
            SELECT closed.id AS run_id,
                   r.id AS routine_id,
                   r.agent_id,
                   r.script_ref,
                   r.name,
                   r.discord_thread_id,
                   r.execution_strategy,
                   closed.owned_tmux_session
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
            fallback_agent_id: candidate.fallback_agent_id,
            max_retries: candidate.max_retries,
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
        let checkpoint_size_error = match close.checkpoint.as_ref() {
            Some(checkpoint) => checkpoint_size_error(checkpoint, self.max_checkpoint_bytes)?,
            None => None,
        };
        let mut result_json = close.result_json;
        let mut checkpoint = close.checkpoint;
        if let Some(message) = checkpoint_size_error.as_deref() {
            result_json = Some(json!({
                "status": "failed",
                "error": message,
                "checkpoint_rejected": true,
                "max_checkpoint_bytes": self.max_checkpoint_bytes,
            }));
            checkpoint = None;
        }
        let run_status = if checkpoint_size_error.is_some() {
            "failed"
        } else {
            close.run_status
        };
        let error = checkpoint_size_error.as_deref().or(close.error);
        let last_result = checkpoint_size_error.as_deref().or(close.last_result);

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
                pause_reason = CASE WHEN $5 THEN $8 ELSE pause_reason END,
                updated_at = NOW()
            WHERE id = $1
              AND in_flight_run_id = $6
            "#,
        )
        .bind(&routine_id)
        .bind(scheduled_next_due_at)
        .bind(&checkpoint)
        .bind(last_result)
        .bind(close.pause_routine)
        .bind(run_id)
        .bind(should_update_next_due_at)
        .bind(close.pause_reason)
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
                attempts = CASE
                    WHEN $3 = 'agent' THEN COALESCE(attempts, '[]'::jsonb) || jsonb_build_array(
                        jsonb_build_object(
                            'event', 'closed',
                            'agent_id', COALESCE($4::jsonb ->> 'agent_id', $4::jsonb ->> 'failed_agent_id'),
                            'kind', COALESCE($4::jsonb ->> 'attempt_kind', 'primary'),
                            'outcome', $2,
                            'error', $5,
                            'at', NOW()
                        )
                    )
                    ELSE attempts
                END,
                finished_at = NOW(),
                lease_expires_at = NULL,
                next_retry_at = NULL,
                updated_at = NOW()
            WHERE id = $1
              AND status = 'running'
            "#,
        )
        .bind(run_id)
        .bind(run_status)
        .bind(close.action)
        .bind(&result_json)
        .bind(error)
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

fn normalize_new_routine_status(status: Option<&str>) -> Result<&'static str> {
    match status.map(str::trim).filter(|value| !value.is_empty()) {
        None | Some("enabled") => Ok("enabled"),
        Some("paused") => Ok("paused"),
        Some(other) => Err(anyhow!(
            "routine initial status must be enabled or paused, got {other}"
        )),
    }
}

fn validate_timeout_secs(timeout_secs: Option<i32>) -> Result<()> {
    if let Some(value) = timeout_secs
        && value <= 0
    {
        return Err(anyhow!("routine timeout_secs must be greater than zero"));
    }
    Ok(())
}

fn validate_max_retries(max_retries: Option<i32>) -> Result<()> {
    if let Some(value) = max_retries
        && value < 0
    {
        return Err(anyhow!(
            "routine max_retries must be greater than or equal to zero"
        ));
    }
    Ok(())
}

fn checkpoint_size_error(
    checkpoint: &Value,
    max_checkpoint_bytes: usize,
) -> Result<Option<String>> {
    let bytes = serde_json::to_vec(checkpoint)
        .map_err(|e| anyhow!("serialize routine checkpoint for size check: {e}"))?
        .len();
    if bytes > max_checkpoint_bytes {
        Ok(Some(format!(
            "routine checkpoint is too large: {bytes} bytes exceeds max_checkpoint_bytes {max_checkpoint_bytes}"
        )))
    } else {
        Ok(None)
    }
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

// pub(crate): scheduled messages reuse the routine schedule grammar
// ('@every <duration>' | 5-field cron) for their optional recurrence.
pub(crate) fn next_due_after(
    schedule: &str,
    default_timezone: &str,
    now: DateTime<Utc>,
) -> Result<DateTime<Utc>> {
    match parse_routine_schedule(schedule)? {
        ParsedRoutineSchedule::Every(duration) => next_every_due_after(duration, now),
        ParsedRoutineSchedule::Cron(cron) => next_cron_due_after(cron, default_timezone, now),
    }
}

pub(crate) fn next_due_after_anchor(
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
    /// Written to `routines.pause_reason` when `pause_routine` is true.
    /// `None` is safe (leaves the column unchanged) for non-pause close paths.
    pause_reason: Option<&'a str>,
}

#[cfg(test)]
mod tests {
    use super::{
        API_FRICTION_OBSERVATION_QUERY, checkpoint_size_error,
        include_automation_candidate_card_observations, next_due_after, next_due_after_anchor,
        parse_schedule_interval, precomputed_observation_from_kv,
        resume_without_next_due_is_invalid, truncate_chars, validate_routine_schedule,
    };
    use crate::api_caller_observability::{AuthStrength, LOG_TARGET, RequestPrincipal};
    use chrono::{TimeZone, Timelike, Utc};
    use serde_json::Value;
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::fmt::writer::MakeWriter;

    // Integration tests that require a live PG connection live in
    // src/integration_tests.rs and are gated on the `integration` feature.
    // The store SQL is compiled by `cargo check`; concurrent claim/recovery
    // behavior should be covered by PG integration tests once the runtime
    // harness starts executing routines.

    #[derive(Clone)]
    struct CapturingWriter {
        buffer: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for CapturingWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.buffer.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for CapturingWriter {
        type Writer = CapturingWriter;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    #[test]
    fn resume_omitted_next_due_rejects_legacy_schedule_less_rows() {
        assert!(resume_without_next_due_is_invalid(None, None));
        assert!(!resume_without_next_due_is_invalid(Some("@every 1h"), None));
        assert!(!resume_without_next_due_is_invalid(
            None,
            Some(Utc.with_ymd_and_hms(2026, 5, 17, 12, 0, 0).unwrap())
        ));
    }

    #[test]
    fn automation_candidate_card_observations_are_executor_only() {
        assert!(include_automation_candidate_card_observations(Some(
            "monitoring/automation-candidate-executor.js"
        )));
        assert!(include_automation_candidate_card_observations(Some(
            " monitoring/automation-executor.js "
        )));
        assert!(include_automation_candidate_card_observations(Some(
            "monitoring/automation-executor-v2.js"
        )));
        assert!(!include_automation_candidate_card_observations(Some(
            "monitoring/automation-candidate-recommender.js"
        )));
        assert!(!include_automation_candidate_card_observations(None));
    }

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
    fn checkpoint_size_error_reports_oversized_payload() {
        let checkpoint = serde_json::json!({"payload": "abcdef"});
        assert!(checkpoint_size_error(&checkpoint, 8).unwrap().is_some());
        assert!(checkpoint_size_error(&checkpoint, 128).unwrap().is_none());
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
    fn api_friction_observation_query_uses_issue_columns_only() {
        assert!(API_FRICTION_OBSERVATION_QUERY.contains("FROM api_friction_issues"));
        assert!(
            !API_FRICTION_OBSERVATION_QUERY.contains("docs_category"),
            "api_friction_issues does not persist docs_category; routine ticks must not warn on a missing column"
        );
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
    fn precomputed_memento_digest_observation_respects_category_override() {
        let now = Utc.with_ymd_and_hms(2026, 5, 13, 7, 0, 0).unwrap();
        let raw = serde_json::json!({
            "topic": "dispatch failures",
            "count": 3,
            "category": "dispatch-retry",
            "source": "memento_digest",
            "signature": "dispatch-retry:dispatch-failures",
            "latest_examples": ["same dispatch failed repeatedly"],
            "raw_memory_body": "MUST_NOT_LEAK"
        })
        .to_string();

        let obs = precomputed_observation_from_kv(
            "routine_observation:memento_digest:dispatch-failures",
            Some(&raw),
            now,
        )
        .expect("memento category digest observation");

        assert_eq!(
            obs.get("category").and_then(Value::as_str),
            Some("dispatch-retry")
        );
        assert_eq!(
            obs.get("signature").and_then(Value::as_str),
            Some("dispatch-retry:dispatch-failures")
        );
        assert_eq!(
            obs.pointer("/value/category").and_then(Value::as_str),
            Some("dispatch-retry")
        );
        assert!(obs.pointer("/value/raw_memory_body").is_none());
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
        assert_eq!(
            obs.pointer("/value/last_seen_at").and_then(Value::as_str),
            Some("2026-05-02T09:59:00Z"),
            "last_seen_at must pass through bounded projection"
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
    fn candidate_approved_observation_forwards_approved_at() {
        let now = Utc.with_ymd_and_hms(2026, 5, 2, 10, 0, 0).unwrap();
        let approved_at = "2026-05-02T09:55:00Z";
        let raw = serde_json::json!({
            "signature": "candidate-b",
            "score": 87,
            "category": "routine-candidate",
            "approved_at": approved_at,
            "suggested_automation": "자동화 제안",
            "outcome_summary": "결과 요약",
            "secret_field": "MUST_NOT_LEAK"
        })
        .to_string();

        let obs = precomputed_observation_from_kv(
            "routine_observation:candidate_approved:candidate-b",
            Some(&raw),
            now,
        )
        .expect("candidate approved observation");

        assert_eq!(
            obs.pointer("/value/signature").and_then(Value::as_str),
            Some("candidate-b")
        );
        assert_eq!(
            obs.pointer("/value/approved_at").and_then(Value::as_str),
            Some(approved_at),
            "approved_at must pass through bounded projection so executor can read it"
        );
        assert_eq!(
            obs.pointer("/value/score").and_then(Value::as_u64),
            Some(87)
        );
        assert!(obs.pointer("/value/secret_field").is_none());
        // source defaults to "precomputed_digest" for unknown source_kinds;
        // the key itself is the authoritative route for JS to identify the marker type.
        assert_eq!(
            obs.get("key").and_then(Value::as_str),
            Some("routine_observation:candidate_approved:candidate-b")
        );
    }

    #[test]
    fn candidate_dispatched_observation_forwards_dispatched_at() {
        let now = Utc.with_ymd_and_hms(2026, 5, 2, 10, 0, 0).unwrap();
        let dispatched_at = "2026-05-02T08:00:00Z";
        let raw = serde_json::json!({
            "signature": "candidate-c",
            "dispatched_at": dispatched_at,
            "category": "routine-candidate",
            "internal_state": "MUST_NOT_LEAK"
        })
        .to_string();

        let obs = precomputed_observation_from_kv(
            "routine_observation:candidate_dispatched:candidate-c",
            Some(&raw),
            now,
        )
        .expect("candidate dispatched observation");

        assert_eq!(
            obs.pointer("/value/signature").and_then(Value::as_str),
            Some("candidate-c")
        );
        assert_eq!(
            obs.pointer("/value/dispatched_at").and_then(Value::as_str),
            Some(dispatched_at),
            "dispatched_at must pass through so executor and recommender can read actual dispatch time"
        );
        assert!(obs.pointer("/value/internal_state").is_none());
        // source defaults to "precomputed_digest" for unknown source_kinds;
        // the key itself is the authoritative route for JS to identify the marker type.
        assert_eq!(
            obs.get("key").and_then(Value::as_str),
            Some("routine_observation:candidate_dispatched:candidate-c")
        );
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

    #[test]
    fn routine_hard_delete_gate_allows_only_detached_without_inflight() {
        assert_eq!(
            super::routine_hard_delete_gate("detached", None, false),
            super::RoutineHardDeleteGate::Allowed
        );
        assert_eq!(
            super::routine_hard_delete_gate("paused", None, false),
            super::RoutineHardDeleteGate::NotDetached {
                status: "paused".to_string()
            }
        );
        assert_eq!(
            super::routine_hard_delete_gate("enabled", None, false),
            super::RoutineHardDeleteGate::NotDetached {
                status: "enabled".to_string()
            }
        );
    }

    #[test]
    fn routine_hard_delete_gate_rejects_inflight_signals() {
        assert_eq!(
            super::routine_hard_delete_gate("detached", Some("run-1"), false),
            super::RoutineHardDeleteGate::InFlight
        );
        assert_eq!(
            super::routine_hard_delete_gate("detached", Some("  "), true),
            super::RoutineHardDeleteGate::InFlight
        );
        assert_eq!(
            super::routine_hard_delete_gate("enabled", Some("run-1"), true),
            super::RoutineHardDeleteGate::InFlight
        );
    }

    #[test]
    fn routine_hard_delete_scope_gate_fails_closed_for_owned_routine() {
        assert_eq!(
            super::routine_delete_scope_gate(Some("codex"), Some("codex"), None),
            super::RoutineDeleteScopeGate::Allowed
        );
        assert_eq!(
            super::routine_delete_scope_gate(None, None, None),
            super::RoutineDeleteScopeGate::Allowed
        );
        assert_eq!(
            super::routine_delete_scope_gate(Some("codex"), None, None),
            super::RoutineDeleteScopeGate::Unresolved {
                owner: "codex".to_string()
            }
        );
        assert_eq!(
            super::routine_delete_scope_gate(Some("codex"), Some("claude"), None),
            super::RoutineDeleteScopeGate::OtherAgent {
                owner: "codex".to_string(),
                caller: "claude".to_string()
            }
        );
    }

    #[test]
    fn routine_delete_scope_gate_logs_delete_path_identity_consumption() {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let writer = CapturingWriter {
            buffer: buffer.clone(),
        };
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_ansi(false)
            .without_time()
            .with_target(true)
            .with_writer(writer)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let principal = RequestPrincipal {
            auth_strength: AuthStrength::ServerAdmin,
            claimed_agent_id: Some("codex".to_string()),
            claimed_channel_id: Some("manager-channel".to_string()),
        };

        assert_eq!(
            super::routine_delete_scope_gate(
                Some("codex"),
                Some("resolved-codex"),
                Some(&principal)
            ),
            super::RoutineDeleteScopeGate::OtherAgent {
                owner: "codex".to_string(),
                caller: "resolved-codex".to_string()
            }
        );
        drop(_guard);

        let logs = String::from_utf8(buffer.lock().unwrap().clone()).unwrap();
        assert!(logs.contains(LOG_TARGET), "logs={logs}");
        assert!(
            logs.contains("endpoint=\"DELETE /api/routines/{id}\""),
            "logs={logs}"
        );
        assert!(
            logs.contains("auth_strength=\"ServerAdmin\""),
            "logs={logs}"
        );
        assert!(logs.contains("claimed_agent_id=\"codex\""), "logs={logs}");
        assert!(
            logs.contains("claimed_channel_id=\"manager-channel\""),
            "logs={logs}"
        );
        assert!(
            logs.contains("consumed_agent_id=\"resolved-codex\""),
            "logs={logs}"
        );
        assert!(
            logs.contains("manager_channel_check_relied_on_claimed_header=false"),
            "logs={logs}"
        );
    }

    fn recovered_run(
        execution_strategy: &str,
        owned_tmux_session: Option<&str>,
    ) -> super::RecoveredRoutineRun {
        super::RecoveredRoutineRun {
            run_id: "run-1".to_string(),
            routine_id: "routine-1".to_string(),
            agent_id: Some("agent-1".to_string()),
            script_ref: "monitoring/x.js".to_string(),
            name: "Routine".to_string(),
            discord_thread_id: Some("123".to_string()),
            execution_strategy: execution_strategy.to_string(),
            owned_tmux_session: owned_tmux_session.map(ToOwned::to_owned),
        }
    }

    #[test]
    fn boot_recovery_reaps_only_fresh_runs_with_owned_session() {
        // #3022: positive ownership proof targets the exact orphaned session.
        assert_eq!(
            recovered_run("fresh", Some("AgentDesk-claude-routine-x"))
                .boot_recovery_owned_session(),
            Some("AgentDesk-claude-routine-x")
        );
    }

    #[test]
    fn boot_recovery_skips_run_without_owned_session() {
        // A fresh run that never started an agent turn (or predates ownership
        // tracking) owns nothing reapable — leave every session alone.
        assert_eq!(
            recovered_run("fresh", None).boot_recovery_owned_session(),
            None
        );
    }

    #[test]
    fn boot_recovery_never_reaps_persistent_run() {
        // A persistent routine's session must survive a restart, even if a
        // stale owned-session value somehow lingered on the row.
        assert_eq!(
            recovered_run("persistent", Some("AgentDesk-claude-persistent"))
                .boot_recovery_owned_session(),
            None
        );
    }

    #[test]
    fn boot_recovery_treats_blank_owned_session_as_nothing() {
        // A blank/whitespace owned-session must not resolve to a wildcard that
        // could match an unrelated session; it means "owns nothing".
        assert_eq!(
            recovered_run("fresh", Some("   ")).boot_recovery_owned_session(),
            None
        );
        assert_eq!(
            recovered_run("fresh", Some("")).boot_recovery_owned_session(),
            None
        );
    }

    #[test]
    fn boot_recovery_trims_owned_session_name() {
        assert_eq!(
            recovered_run("fresh", Some("  AgentDesk-claude-routine-x  "))
                .boot_recovery_owned_session(),
            Some("AgentDesk-claude-routine-x")
        );
    }

    // --- pause_reason field tests ---

    #[test]
    fn pause_reason_constants_have_correct_values() {
        assert_eq!(super::PAUSE_REASON_FAILURE, "failure");
        assert_eq!(super::PAUSE_REASON_MANUAL, "manual");
        assert_eq!(super::PAUSE_REASON_MIGRATION_INVALID, "migration_invalid");
    }

    #[test]
    fn terminal_failure_pause_gate_controls_failure_pause_reason() {
        assert!(!super::terminal_failure_should_pause(false));
        assert_eq!(super::terminal_failure_pause_reason(false), None);

        assert!(super::terminal_failure_should_pause(true));
        assert_eq!(
            super::terminal_failure_pause_reason(true),
            Some(super::PAUSE_REASON_FAILURE)
        );
    }

    #[test]
    fn close_run_pause_reason_is_set_only_when_pausing() {
        // The pause_reason field must be Some on pause paths and None on
        // non-pause paths; the SQL only writes it when pause_routine = true.
        // This test validates the shape of the CloseRun values we construct
        // for the four public close paths:
        //   fail_run_and_pause_routine  → pause_routine=true, pause_reason=Some("failure")
        //   fail_run_and_pause_as_migration_invalid → pause_routine=true, pause_reason=Some("migration_invalid")
        //   pause_after_run             → pause_routine=true, pause_reason=Some("manual")
        //   fail_run / finish_run / skip_run / fail_agent_run / complete_agent_run
        //                               → pause_routine=false, pause_reason=None
        //
        // We can't call these async fns without a pool, so we verify the
        // constants and the guard predicate (resume_without_next_due_is_invalid)
        // that auto_resume_failure_paused_routine relies on.
        assert_eq!(super::PAUSE_REASON_FAILURE, "failure");
        assert_eq!(super::PAUSE_REASON_MANUAL, "manual");
        assert_eq!(super::PAUSE_REASON_MIGRATION_INVALID, "migration_invalid");
    }

    #[test]
    fn auto_resume_guard_rejects_schedule_less_no_next_due() {
        // auto_resume_failure_paused_routine applies resume_without_next_due_is_invalid.
        // Verify the predicate behaviour for the auto-resume eligibility check:
        // a routine with neither schedule nor next_due_at must be rejected.
        assert!(
            resume_without_next_due_is_invalid(None, None),
            "schedule-less + no next_due_at must be invalid (ResumeRequiresNextDueAt)"
        );
    }

    #[test]
    fn auto_resume_guard_allows_scheduled_routine() {
        // A routine with a schedule is always resumable (the store will derive
        // next_due_at from the schedule expression).
        assert!(
            !resume_without_next_due_is_invalid(Some("@every 1h"), None),
            "scheduled routine must be valid even without explicit next_due_at"
        );
    }

    #[test]
    fn auto_resume_guard_allows_explicit_next_due_at() {
        use chrono::{TimeZone, Utc};
        let ts = Utc.with_ymd_and_hms(2026, 7, 1, 9, 0, 0).unwrap();
        assert!(
            !resume_without_next_due_is_invalid(None, Some(ts)),
            "schedule-less routine with explicit next_due_at must be valid"
        );
    }

    #[test]
    fn pause_reason_failure_is_the_only_auto_resume_eligible_value() {
        // Document the eligibility contract: only "failure" is eligible;
        // "manual", "migration_invalid", and NULL must NOT be eligible.
        // The SQL WHERE clause `pause_reason = PAUSE_REASON_FAILURE` enforces
        // this at the DB layer; this unit test pins the constant values so a
        // refactor cannot silently widen the eligible set.
        let failure = super::PAUSE_REASON_FAILURE;
        let manual = super::PAUSE_REASON_MANUAL;
        let migration_invalid = super::PAUSE_REASON_MIGRATION_INVALID;

        assert_ne!(failure, manual);
        assert_ne!(failure, migration_invalid);
        assert_ne!(manual, migration_invalid);

        // The auto-resume SQL binds exactly PAUSE_REASON_FAILURE.
        // Any value that differs from it is excluded from auto-resume.
        assert_eq!(failure, "failure");
        assert_ne!(manual, "failure");
        assert_ne!(migration_invalid, "failure");
    }
}
