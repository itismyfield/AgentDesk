//! Regression alert rule engine (#1104 / 911-4).
//!
//! Pipeline (mirrors the SLO design from #1072):
//!
//! 1. [`compute_regressions`] reads the latest row per agent from
//!    `agent_quality_daily`, computes `delta = baseline_30d - current_7d`
//!    for each tracked metric, and emits a [`Regression`] when the delta
//!    crosses the metric-specific drop threshold AND the per-window sample size is at
//!    least [`MIN_SAMPLE_SIZE`].
//! 2. [`should_fire_alert`] consults `quality_regression_cooldowns` and
//!    only allows an alert through when the previous fire is older than
//!    [`ALERT_COOLDOWN_MS`].
//! 3. [`dispatch_alert`] formats a Discord payload (agent / metric /
//!    expected vs actual / drill-down) and inserts it into
//!    `message_outbox` with `bot=notify`, then advances the cooldown.
//!
//! The orchestrator [`run_regression_alerter_pg`] is invoked hourly from
//! the maintenance scheduler. PG is the canonical backend; sqlite-only
//! deployments rely on the existing #1101 path until the rollup learns to
//! emit `agent_quality_daily` into sqlite (out of scope for #1104).

use anyhow::{Result, anyhow};
use serde::Serialize;
use sqlx::{PgPool, Row};

use crate::services::message_outbox::{OutboxMessage, enqueue_outbox_pg_on_tx_with_ttl};

/// Minimum 7d sample window required before an alert can fire. Mirrors the
/// `QUALITY_SAMPLE_GUARD` used by the rollup itself in
/// `services::observability`.
pub const MIN_SAMPLE_SIZE: i64 = 5;

/// Policy preserved from the retired rollup-coupled producer: turn success is
/// more sensitive (15 percentage points) while review pass uses 20 points.
pub const TURN_DROP_THRESHOLD: f64 = 0.15;
pub const REVIEW_DROP_THRESHOLD: f64 = 0.20;

/// 24h cooldown per (agent_id, metric).
pub const ALERT_COOLDOWN_MS: i64 = 24 * 60 * 60 * 1000;

/// Optional drill-down URL prefix; the agent id is appended to form the
/// final link. Configurable so dev / release / external dashboards can
/// each point at their own endpoint.
pub const DRILL_DOWN_BASE_ENV: &str = "ADK_QUALITY_ALERT_DRILL_BASE";
pub const DEFAULT_DRILL_DOWN_BASE: &str = "/dashboard/agent-quality";

/// Metrics tracked by the rule engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum QualityMetric {
    TurnSuccessRate,
    ReviewPassRate,
}

impl QualityMetric {
    pub fn as_str(&self) -> &'static str {
        match self {
            QualityMetric::TurnSuccessRate => "turn_success_rate",
            QualityMetric::ReviewPassRate => "review_pass_rate",
        }
    }
}

/// One regression candidate. Captured before the cooldown filter; the
/// caller decides whether to fire based on [`should_fire_alert`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Regression {
    pub agent_id: String,
    pub metric: QualityMetric,
    /// `agent_quality_daily.<metric>_30d` (baseline window).
    pub baseline: f64,
    /// `agent_quality_daily.<metric>_7d` (current window).
    pub current: f64,
    /// `baseline - current` (always positive when this struct exists; the
    /// detector filters out non-regressions).
    pub delta: f64,
    pub sample_size_7d: i64,
    pub sample_size_30d: i64,
}

impl Regression {
    pub fn delta_pct_points(&self) -> f64 {
        self.delta * 100.0
    }
}

fn normalize_channel_target(channel: &str) -> Option<String> {
    let channel = channel.trim();
    if channel.is_empty() {
        return None;
    }
    Some(if channel.starts_with("channel:") {
        channel.to_string()
    } else {
        format!("channel:{channel}")
    })
}

/// Preserve the retired producer's operator policy: the dedicated quality
/// channel wins, then the shared human-alert channel. No configured target is
/// an intentional off-switch; a hard-coded fallback would silently page a
/// different channel after authority consolidation.
pub(crate) async fn resolve_alert_channel_pg(pool: &PgPool) -> Result<Option<String>> {
    let value = sqlx::query_scalar::<_, String>(
        "SELECT value
         FROM kv_meta
         WHERE key IN ('agent_quality_monitoring_channel_id', 'kanban_human_alert_channel_id')
           AND value IS NOT NULL
           AND btrim(value) <> ''
         ORDER BY CASE key
                      WHEN 'agent_quality_monitoring_channel_id' THEN 0
                      ELSE 1
                  END
         LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .map_err(|error| anyhow!("load agent quality alert target: {error}"))?;
    Ok(value.as_deref().and_then(normalize_channel_target))
}

pub(crate) async fn resolve_alert_channel_with_env_pg(
    pool: &PgPool,
    env_key: &str,
) -> Result<Option<String>> {
    if let Ok(value) = std::env::var(env_key) {
        if let Some(target) = normalize_channel_target(&value) {
            return Ok(Some(target));
        }
    }
    resolve_alert_channel_pg(pool).await
}

/// Resolve the drill-down base URL (env override or fallback const).
pub fn resolve_drill_down_base() -> String {
    std::env::var(DRILL_DOWN_BASE_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_DRILL_DOWN_BASE.to_string())
}

/// Compose the drill-down link for `agent_id`.
pub fn drill_down_link(agent_id: &str) -> String {
    let base = resolve_drill_down_base().trim_end_matches('/').to_string();
    format!("{base}/{agent_id}")
}

/// Format a Discord-bound alert message for `regression`. Includes
/// agent id, metric label, expected (baseline) vs actual (current),
/// sample sizes, and a drill-down link as required by DoD.
/// Decode `column` from `row`, surfacing genuine decode faults instead of
/// silently substituting a default (see [`decode_with_fallback`] for the
/// fail-closed rationale). The name is retained for API stability; the
/// "fallback" now applies only to the sqlx-native `Ok(None)` returned for a
/// SQL `NULL` decoded into an `Option<T>` target — never to a `ColumnDecode`.
pub fn explicit_decode_fallback<'r, T, R>(row: &'r R, column: &str) -> Result<T>
where
    R: Row,
    T: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    for<'a> &'a str: sqlx::ColumnIndex<R>,
{
    decode_with_fallback(column, || row.try_get(column))
}

/// Decode a column, distinguishing a *legitimately absent / optional* value
/// from a *genuine decode failure*.
///
/// IMPORTANT — fail-closed contract: a [`sqlx::Error::ColumnDecode`] means the
/// column was present but its bytes could not be decoded into `T` (type
/// mismatch, malformed payload, or an unexpected SQL `NULL` decoded into a
/// non-`Option` target). Silently swallowing that into `default_val` would let
/// a real data fault masquerade as "metric absent / insufficient samples" and
/// **suppress** the very regression alert this module exists to emit
/// (fail-OPEN). We therefore propagate `ColumnDecode` as an error so
/// [`compute_regressions`] surfaces the fault instead of hiding a regression.
///
/// Legitimately-absent optional columns are *not* affected: a SQL `NULL`
/// decoded into an `Option<T>` target is `Ok(None)` at the sqlx layer and never
/// reaches the `ColumnDecode` arm, so the existing `None` / `0` defaults for
/// genuinely-missing data are preserved for the success path.
fn decode_with_fallback<T, F>(column: &str, decode: F) -> Result<T>
where
    F: FnOnce() -> std::result::Result<T, sqlx::Error>,
{
    match decode() {
        Ok(val) => Ok(val),
        Err(e @ sqlx::Error::ColumnDecode { .. }) => {
            tracing::warn!(
                column = column,
                error = %e,
                "[quality] column decode error surfaced (fail-closed; not suppressing regression)"
            );
            Err(anyhow!("decode {}: {}", column, e))
        }
        Err(e) => Err(anyhow!("decode {}: {}", column, e)),
    }
}

pub fn format_alert_message(regression: &Regression) -> String {
    format!(
        "[quality] regression on agent `{agent}` :: metric={metric} \
         expected (30d baseline)={expected:.1}% \
         actual (7d current)={actual:.1}% \
         drop={drop:.1}%p \
         sample={s7}/{s30} \
         drill={link}",
        agent = regression.agent_id,
        metric = regression.metric.as_str(),
        expected = regression.baseline * 100.0,
        actual = regression.current * 100.0,
        drop = regression.delta_pct_points(),
        s7 = regression.sample_size_7d,
        s30 = regression.sample_size_30d,
        link = drill_down_link(&regression.agent_id),
    )
}

// ─────────────────────────────────────────────────────────────────────────
// Detection (PG)
// ─────────────────────────────────────────────────────────────────────────

/// Read the latest row per agent from `agent_quality_daily` and emit
/// [`Regression`] entries for each (agent, metric) pair that crosses the
/// its metric-specific threshold with sample_size ≥ [`MIN_SAMPLE_SIZE`] in *both*
/// the 7d window (current) and the 30d window (baseline).
///
/// `measurement_unavailable_*d` flags from the rollup are honoured:
/// rows where either window is flagged unavailable are skipped so we
/// never page on under-sampled noise.
pub async fn compute_regressions(pool: &PgPool) -> Result<Vec<Regression>> {
    let rows = sqlx::query(
        "WITH latest AS (
             SELECT DISTINCT ON (agent_id)
                    agent_id,
                    turn_success_rate_7d,
                    turn_success_rate_30d,
                    review_pass_rate_7d,
                    review_pass_rate_30d,
                    turn_sample_size_7d,
                    turn_sample_size_30d,
                    review_sample_size_7d,
                    review_sample_size_30d,
                    measurement_unavailable_7d,
                    measurement_unavailable_30d
             FROM agent_quality_daily
             ORDER BY agent_id, day DESC
         )
         SELECT *
         FROM latest
         WHERE measurement_unavailable_7d = FALSE
           AND measurement_unavailable_30d = FALSE",
    )
    .fetch_all(pool)
    .await
    .map_err(|error| anyhow!("query agent_quality_daily for regressions: {error}"))?;

    let mut out = Vec::new();
    for row in rows {
        let agent_id: String = row
            .try_get("agent_id")
            .map_err(|error| anyhow!("decode agent_id: {error}"))?;

        let turn_7d: Option<f64> = explicit_decode_fallback(&row, "turn_success_rate_7d")?;
        let turn_30d: Option<f64> = explicit_decode_fallback(&row, "turn_success_rate_30d")?;
        let turn_s7: i64 = explicit_decode_fallback(&row, "turn_sample_size_7d")?;
        let turn_s30: i64 = explicit_decode_fallback(&row, "turn_sample_size_30d")?;
        if let Some(reg) = build_regression(
            &agent_id,
            QualityMetric::TurnSuccessRate,
            turn_7d,
            turn_30d,
            turn_s7,
            turn_s30,
        ) {
            out.push(reg);
        }

        let review_7d: Option<f64> = explicit_decode_fallback(&row, "review_pass_rate_7d")?;
        let review_30d: Option<f64> = explicit_decode_fallback(&row, "review_pass_rate_30d")?;
        let review_s7: i64 = explicit_decode_fallback(&row, "review_sample_size_7d")?;
        let review_s30: i64 = explicit_decode_fallback(&row, "review_sample_size_30d")?;
        if let Some(reg) = build_regression(
            &agent_id,
            QualityMetric::ReviewPassRate,
            review_7d,
            review_30d,
            review_s7,
            review_s30,
        ) {
            out.push(reg);
        }
    }
    Ok(out)
}

/// Pure detection helper exposed for unit testing.
pub fn build_regression(
    agent_id: &str,
    metric: QualityMetric,
    current_7d: Option<f64>,
    baseline_30d: Option<f64>,
    sample_7d: i64,
    sample_30d: i64,
) -> Option<Regression> {
    let current = current_7d?;
    let baseline = baseline_30d?;
    let delta = baseline - current;
    let threshold = match metric {
        QualityMetric::TurnSuccessRate => TURN_DROP_THRESHOLD,
        QualityMetric::ReviewPassRate => REVIEW_DROP_THRESHOLD,
    };
    if delta <= threshold {
        return None;
    }
    if sample_7d < MIN_SAMPLE_SIZE || sample_30d < MIN_SAMPLE_SIZE {
        return None;
    }
    Some(Regression {
        agent_id: agent_id.to_string(),
        metric,
        baseline,
        current,
        delta,
        sample_size_7d: sample_7d,
        sample_size_30d: sample_30d,
    })
}

// ─────────────────────────────────────────────────────────────────────────
// Cooldown (PG)
// ─────────────────────────────────────────────────────────────────────────

/// Returns `true` if a 24h cooldown has elapsed (or no prior entry exists).
pub async fn should_fire_alert(
    pool: &PgPool,
    metric: QualityMetric,
    agent_id: &str,
    now_ms: i64,
) -> Result<bool> {
    let last_alert_ts: Option<i64> = sqlx::query_scalar(
        "SELECT alerted_at_ms
         FROM quality_regression_cooldowns
         WHERE agent_id = $1 AND metric = $2",
    )
    .bind(agent_id)
    .bind(metric.as_str())
    .fetch_optional(pool)
    .await
    .map_err(|error| anyhow!("read regression cooldown: {error}"))?;
    Ok(should_fire_alert_pure(
        metric,
        agent_id,
        last_alert_ts,
        now_ms,
    ))
}

/// Pure version of [`should_fire_alert`] for unit testing without a PG
/// connection. Signature matches DoD requirement
/// (`metric, agent, last_alert_ts -> bool`).
pub fn should_fire_alert_pure(
    _metric: QualityMetric,
    _agent_id: &str,
    last_alert_ts: Option<i64>,
    now_ms: i64,
) -> bool {
    match last_alert_ts {
        None => true,
        Some(previous) => now_ms.saturating_sub(previous) >= ALERT_COOLDOWN_MS,
    }
}

/// Upsert the cooldown row in the caller's alert-outbox transaction.
async fn record_alert_sent_on_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    regression: &Regression,
    now_ms: i64,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO quality_regression_cooldowns
             (agent_id, metric, alerted_at_ms, last_baseline,
              last_current, last_delta, last_sample_size)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         ON CONFLICT (agent_id, metric) DO UPDATE SET
             alerted_at_ms    = EXCLUDED.alerted_at_ms,
             last_baseline    = EXCLUDED.last_baseline,
             last_current     = EXCLUDED.last_current,
             last_delta       = EXCLUDED.last_delta,
             last_sample_size = EXCLUDED.last_sample_size",
    )
    .bind(&regression.agent_id)
    .bind(regression.metric.as_str())
    .bind(now_ms)
    .bind(regression.baseline)
    .bind(regression.current)
    .bind(regression.delta)
    .bind(regression.sample_size_7d)
    .execute(&mut **tx)
    .await
    .map_err(|error| anyhow!("record regression cooldown: {error}"))?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────
// Dispatch (PG)
// ─────────────────────────────────────────────────────────────────────────

/// Render and enqueue a single regression alert. The outbox obligation and
/// cooldown advance commit atomically, so neither can survive alone.
/// Returns `true` when a new outbox row was created.
pub async fn dispatch_alert(
    pool: &PgPool,
    regression: &Regression,
    target_channel: &str,
    now_ms: i64,
) -> Result<bool> {
    let content = format_alert_message(regression);
    let session_key = format!(
        "agent_quality_regression:{}:{}",
        regression.agent_id,
        regression.metric.as_str()
    );

    let mut tx = pool
        .begin()
        .await
        .map_err(|error| anyhow!("begin regression alert transaction: {error}"))?;
    let enqueued = enqueue_outbox_pg_on_tx_with_ttl(
        &mut tx,
        OutboxMessage {
            target: target_channel,
            content: &content,
            bot: "notify",
            source: "quality_regression_alerter",
            reason_code: Some("agent_quality.regression"),
            session_key: Some(&session_key),
        },
        ALERT_COOLDOWN_MS / 1000,
    )
    .await
    .map_err(|error| anyhow!("enqueue regression alert: {error}"))?
    .is_some();

    if enqueued {
        record_alert_sent_on_tx(&mut tx, regression, now_ms).await?;
    }
    tx.commit()
        .await
        .map_err(|error| anyhow!("commit regression alert transaction: {error}"))?;
    Ok(enqueued)
}

// ─────────────────────────────────────────────────────────────────────────
// Orchestrator
// ─────────────────────────────────────────────────────────────────────────

/// Hourly entry point used by the maintenance scheduler. Returns the
/// number of alerts actually dispatched (post-cooldown).
pub async fn run_regression_alerter_pg(pool: &PgPool) -> Result<u64> {
    let Some(target) = resolve_alert_channel_pg(pool).await? else {
        return Ok(0);
    };
    let regressions = compute_regressions(pool).await?;
    if regressions.is_empty() {
        return Ok(0);
    }
    let now_ms = chrono::Utc::now().timestamp_millis();

    let mut sent: u64 = 0;
    for regression in regressions {
        let allowed = should_fire_alert(pool, regression.metric, &regression.agent_id, now_ms)
            .await
            .unwrap_or(true);
        if !allowed {
            tracing::debug!(
                agent = %regression.agent_id,
                metric = regression.metric.as_str(),
                "[quality] regression suppressed by 24h cooldown"
            );
            continue;
        }
        match dispatch_alert(pool, &regression, &target, now_ms).await {
            Ok(true) => {
                sent = sent.saturating_add(1);
                tracing::warn!(
                    agent = %regression.agent_id,
                    metric = regression.metric.as_str(),
                    drop_pp = regression.delta_pct_points(),
                    target = %target,
                    "[quality] regression alert dispatched"
                );
            }
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(
                    agent = %regression.agent_id,
                    metric = regression.metric.as_str(),
                    "[quality] dispatch_alert failed: {error}"
                );
            }
        }
    }
    Ok(sent)
}

// ─────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod explicit_decode_fallback_tests {
    use super::*;

    fn must_ok<T, E: std::fmt::Debug>(result: Result<T, E>, context: &str) -> T {
        match result {
            Ok(value) => value,
            Err(error) => panic!("{context}: {error:?}"),
        }
    }

    #[test]
    fn metric_thresholds_preserve_retired_authority_policy() {
        assert!(
            build_regression(
                "agent-1",
                QualityMetric::TurnSuccessRate,
                Some(0.70),
                Some(0.86),
                MIN_SAMPLE_SIZE,
                MIN_SAMPLE_SIZE,
            )
            .is_some()
        );
        assert!(
            build_regression(
                "agent-1",
                QualityMetric::ReviewPassRate,
                Some(0.70),
                Some(0.86),
                MIN_SAMPLE_SIZE,
                MIN_SAMPLE_SIZE,
            )
            .is_none()
        );
        assert!(
            build_regression(
                "agent-1",
                QualityMetric::ReviewPassRate,
                Some(0.70),
                Some(0.91),
                MIN_SAMPLE_SIZE,
                MIN_SAMPLE_SIZE,
            )
            .is_some()
        );
        assert!(
            build_regression(
                "agent-1",
                QualityMetric::TurnSuccessRate,
                Some(0.0),
                Some(TURN_DROP_THRESHOLD),
                MIN_SAMPLE_SIZE,
                MIN_SAMPLE_SIZE,
            )
            .is_none(),
            "the retired authority used a strict greater-than boundary"
        );
    }

    #[test]
    fn quality_channel_target_normalizes_without_implicit_fallback() {
        assert_eq!(
            normalize_channel_target(" 123 ").as_deref(),
            Some("channel:123")
        );
        assert_eq!(
            normalize_channel_target("channel:456").as_deref(),
            Some("channel:456")
        );
        assert_eq!(normalize_channel_target("   "), None);
    }

    #[tokio::test]
    async fn quality_channel_uses_db_precedence_and_no_target_off_switch_pg() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        assert_eq!(
            must_ok(
                resolve_alert_channel_pg(&pool).await,
                "resolve empty quality target",
            ),
            None
        );
        must_ok(
            sqlx::query(
                "INSERT INTO kv_meta (key, value)
                 VALUES ('kanban_human_alert_channel_id', 'human-channel')",
            )
            .execute(&pool)
            .await,
            "seed human alert target",
        );
        assert_eq!(
            must_ok(
                resolve_alert_channel_pg(&pool).await,
                "resolve human quality target",
            )
            .as_deref(),
            Some("channel:human-channel")
        );

        must_ok(
            sqlx::query(
                "INSERT INTO kv_meta (key, value)
                 VALUES ('agent_quality_monitoring_channel_id', 'quality-channel')",
            )
            .execute(&pool)
            .await,
            "seed dedicated quality target",
        );
        assert_eq!(
            must_ok(
                resolve_alert_channel_pg(&pool).await,
                "resolve dedicated quality target",
            )
            .as_deref(),
            Some("channel:quality-channel")
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn regression_outbox_and_cooldown_commit_or_roll_back_together_pg() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let regression = Regression {
            agent_id: "agent-atomic".to_string(),
            metric: QualityMetric::TurnSuccessRate,
            baseline: 0.95,
            current: 0.70,
            delta: 0.25,
            sample_size_7d: 20,
            sample_size_30d: 80,
        };

        assert!(
            must_ok(
                dispatch_alert(&pool, &regression, "channel:123", 1_000).await,
                "dispatch atomic regression alert",
            ),
            "first dispatch must create an outbox obligation"
        );
        let committed = must_ok(
            sqlx::query_as::<_, (i64, i64, bool)>(
                "SELECT
                    (SELECT COUNT(*) FROM message_outbox),
                    (SELECT COUNT(*) FROM quality_regression_cooldowns),
                    (SELECT dedupe_expires_at >= created_at + INTERVAL '24 hours'
                       FROM message_outbox LIMIT 1)",
            )
            .fetch_one(&pool)
            .await,
            "load committed regression alert state",
        );
        assert_eq!(committed, (1, 1, true));

        must_ok(
            sqlx::query("DELETE FROM message_outbox")
                .execute(&pool)
                .await,
            "clear outbox before rollback probe",
        );
        must_ok(
            sqlx::query("DROP TABLE quality_regression_cooldowns")
                .execute(&pool)
                .await,
            "remove cooldown table to force second statement failure",
        );
        let rejected = dispatch_alert(&pool, &regression, "channel:123", 2_000).await;
        assert!(
            rejected.is_err(),
            "cooldown write failure must reject the whole transaction"
        );
        let outbox_count = must_ok(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM message_outbox")
                .fetch_one(&pool)
                .await,
            "count outbox after rollback",
        );
        assert_eq!(
            outbox_count, 0,
            "outbox insert must roll back with cooldown"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[test]
    fn returns_value_on_success() {
        let result: Result<f64, anyhow::Error> = decode_with_fallback("some_column", || Ok(12.5));
        assert_eq!(result.unwrap(), 12.5);
    }

    /// A sqlx-native SQL `NULL` decoded into an `Option<T>` target arrives as
    /// `Ok(None)` (not a `ColumnDecode`), so a *legitimately absent* optional
    /// column is still honoured as the natural `None` default — preserving the
    /// existing behaviour for genuinely-missing data.
    #[test]
    fn preserves_none_for_legitimately_absent_optional_column() {
        let result: Result<Option<f64>, anyhow::Error> =
            decode_with_fallback("some_column", || Ok(None));
        assert_eq!(result.unwrap(), None);
    }

    /// Regression guard for the fail-OPEN bug: a genuine `ColumnDecode` (type
    /// mismatch / malformed payload / unexpected NULL into a non-`Option`
    /// sample count) must surface as an error and MUST NOT be silently
    /// swallowed into a default — otherwise a real decode fault would be
    /// treated as "metric absent" and suppress the regression alert.
    #[test]
    fn fails_closed_on_column_decode_error() {
        let result: Result<f64, anyhow::Error> = decode_with_fallback("some_column", || {
            Err(sqlx::Error::ColumnDecode {
                index: "some_column".to_string(),
                source: Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "mock decode error",
                )),
            })
        });

        // The default (42.0) must NOT be returned — the fault must surface.
        assert!(
            result.is_err(),
            "ColumnDecode must not be swallowed into a default that hides a regression"
        );
        let error = result.unwrap_err().to_string();
        assert!(error.contains("decode some_column"));
        assert!(error.contains("mock decode error"));
    }

    #[test]
    fn fails_closed_on_other_errors() {
        let result: Result<f64, anyhow::Error> = decode_with_fallback("some_column", || {
            Err(sqlx::Error::ColumnNotFound("missing".to_string()))
        });

        assert!(result.is_err());
        let error = result.unwrap_err().to_string();
        assert!(error.contains("decode some_column"));
        assert!(error.contains("missing"));
    }
}
