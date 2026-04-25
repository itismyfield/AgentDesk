//! Regression alert rule engine (#1104 / 911-4).
//!
//! Pipeline (mirrors the SLO design from #1072):
//!
//! 1. [`compute_regressions`] reads the latest row per agent from
//!    `agent_quality_daily`, computes `delta = baseline_30d - current_7d`
//!    for each tracked metric, and emits a [`Regression`] when the delta
//!    crosses [`DROP_THRESHOLD`] AND the per-window sample size is at
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

use crate::services::message_outbox::{OutboxMessage, enqueue_outbox_pg};

/// Minimum 7d sample window required before an alert can fire. Mirrors the
/// `QUALITY_SAMPLE_GUARD` used by the rollup itself in
/// `services::observability`.
pub const MIN_SAMPLE_SIZE: i64 = 5;

/// Drop threshold expressed as an absolute fraction (0.20 == 20 percentage
/// points). Applied to both review and turn metrics so the rule engine is
/// uniform — the legacy path in observability uses a split 15%/20%p mix
/// that pre-dates this rule engine.
pub const DROP_THRESHOLD: f64 = 0.20;

/// 24h cooldown per (agent_id, metric).
pub const ALERT_COOLDOWN_MS: i64 = 24 * 60 * 60 * 1000;

/// Env var that overrides the alert channel; falls back to
/// [`FALLBACK_ALERT_CHANNEL`] (adk-cc) when unset/empty.
pub const ALERT_CHANNEL_ENV: &str = "ADK_QUALITY_ALERT_CHANNEL";

/// Fallback Discord channel id (adk-cc) used when env var is unset.
/// Same id as `services::slo::FALLBACK_ALERT_CHANNEL` so first-boot
/// behaviour is consistent across alert pipelines.
pub const FALLBACK_ALERT_CHANNEL: &str = "1479671298497183835";

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

    pub fn label(&self) -> &'static str {
        match self {
            QualityMetric::TurnSuccessRate => "turn success rate",
            QualityMetric::ReviewPassRate => "review pass rate",
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

/// Resolve the alert channel id (env override or fallback const).
pub fn resolve_alert_channel() -> String {
    std::env::var(ALERT_CHANNEL_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| FALLBACK_ALERT_CHANNEL.to_string())
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
/// [`DROP_THRESHOLD`] with sample_size ≥ [`MIN_SAMPLE_SIZE`] in *both*
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

        let turn_7d: Option<f64> = row.try_get("turn_success_rate_7d").unwrap_or(None);
        let turn_30d: Option<f64> = row.try_get("turn_success_rate_30d").unwrap_or(None);
        let turn_s7: i64 = row.try_get("turn_sample_size_7d").unwrap_or(0);
        let turn_s30: i64 = row.try_get("turn_sample_size_30d").unwrap_or(0);
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

        let review_7d: Option<f64> = row.try_get("review_pass_rate_7d").unwrap_or(None);
        let review_30d: Option<f64> = row.try_get("review_pass_rate_30d").unwrap_or(None);
        let review_s7: i64 = row.try_get("review_sample_size_7d").unwrap_or(0);
        let review_s30: i64 = row.try_get("review_sample_size_30d").unwrap_or(0);
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
    if delta < DROP_THRESHOLD {
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

/// Upsert the cooldown row to advance the 24h window after a successful
/// dispatch.
pub async fn record_alert_sent(pool: &PgPool, regression: &Regression, now_ms: i64) -> Result<()> {
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
    .execute(pool)
    .await
    .map_err(|error| anyhow!("record regression cooldown: {error}"))?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────
// Dispatch (PG)
// ─────────────────────────────────────────────────────────────────────────

/// Render and enqueue a single regression alert. Inserts into
/// `message_outbox` with `bot=notify` then advances the cooldown.
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

    let enqueued = enqueue_outbox_pg(
        pool,
        OutboxMessage {
            target: target_channel,
            content: &content,
            bot: "notify",
            source: "quality_regression_alerter",
            reason_code: Some("agent_quality.regression"),
            session_key: Some(&session_key),
        },
    )
    .await
    .map_err(|error| anyhow!("enqueue regression alert: {error}"))?;

    if enqueued {
        record_alert_sent(pool, regression, now_ms).await?;
    }
    Ok(enqueued)
}

// ─────────────────────────────────────────────────────────────────────────
// Orchestrator
// ─────────────────────────────────────────────────────────────────────────

/// Hourly entry point used by the maintenance scheduler. Returns the
/// number of alerts actually dispatched (post-cooldown).
pub async fn run_regression_alerter_pg(pool: &PgPool) -> Result<u64> {
    let regressions = compute_regressions(pool).await?;
    if regressions.is_empty() {
        return Ok(0);
    }
    let target = resolve_alert_channel();
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
mod tests {
    use super::*;

    #[test]
    fn detection_flags_when_drop_meets_threshold_and_samples_sufficient() {
        let regression = build_regression(
            "agent-a",
            QualityMetric::TurnSuccessRate,
            Some(0.55),
            Some(0.80),
            10,
            40,
        )
        .expect("threshold breach should produce regression");
        assert_eq!(regression.metric, QualityMetric::TurnSuccessRate);
        assert!((regression.delta - 0.25).abs() < 1e-9);
        assert_eq!(regression.sample_size_7d, 10);
        assert_eq!(regression.sample_size_30d, 40);
    }

    #[test]
    fn detection_skips_when_drop_below_threshold() {
        let result = build_regression(
            "agent-a",
            QualityMetric::ReviewPassRate,
            Some(0.65),
            Some(0.80),
            10,
            40,
        );
        // 15%p drop — below the 20%p threshold.
        assert!(result.is_none(), "15%p drop must NOT trigger regression");
    }

    #[test]
    fn detection_skips_when_sample_size_below_minimum() {
        // 7d sample below MIN_SAMPLE_SIZE
        let result = build_regression(
            "agent-a",
            QualityMetric::TurnSuccessRate,
            Some(0.40),
            Some(0.80),
            4, // below MIN_SAMPLE_SIZE
            40,
        );
        assert!(
            result.is_none(),
            "sample_size_7d < {} must suppress alert",
            MIN_SAMPLE_SIZE
        );

        // 30d sample below MIN_SAMPLE_SIZE
        let result = build_regression(
            "agent-a",
            QualityMetric::TurnSuccessRate,
            Some(0.40),
            Some(0.80),
            10,
            3,
        );
        assert!(
            result.is_none(),
            "sample_size_30d < {} must suppress alert",
            MIN_SAMPLE_SIZE
        );
    }

    #[test]
    fn detection_skips_when_baseline_or_current_missing() {
        assert!(
            build_regression(
                "agent-a",
                QualityMetric::TurnSuccessRate,
                None,
                Some(0.80),
                10,
                40,
            )
            .is_none()
        );
        assert!(
            build_regression(
                "agent-a",
                QualityMetric::TurnSuccessRate,
                Some(0.50),
                None,
                10,
                40,
            )
            .is_none()
        );
    }

    #[test]
    fn cooldown_allows_first_alert_then_suppresses_within_24h() {
        let now = 1_777_000_000_000_i64;
        // No previous alert → should fire.
        assert!(should_fire_alert_pure(
            QualityMetric::TurnSuccessRate,
            "agent-a",
            None,
            now
        ));

        // Just sent an alert.
        let last = now;
        // 1 hour later → suppressed.
        assert!(!should_fire_alert_pure(
            QualityMetric::TurnSuccessRate,
            "agent-a",
            Some(last),
            now + 60 * 60 * 1000
        ));

        // 23 hours later → still suppressed.
        assert!(!should_fire_alert_pure(
            QualityMetric::TurnSuccessRate,
            "agent-a",
            Some(last),
            now + 23 * 60 * 60 * 1000
        ));

        // 24h+1m later → released.
        assert!(should_fire_alert_pure(
            QualityMetric::TurnSuccessRate,
            "agent-a",
            Some(last),
            now + 24 * 60 * 60 * 1000 + 60_000
        ));
    }

    #[test]
    fn formatter_includes_required_dod_fields() {
        let regression = Regression {
            agent_id: "agent-x".to_string(),
            metric: QualityMetric::TurnSuccessRate,
            baseline: 0.92,
            current: 0.65,
            delta: 0.27,
            sample_size_7d: 14,
            sample_size_30d: 80,
        };
        let msg = format_alert_message(&regression);
        // DoD: ID / metric / expected / actual / drill-down
        assert!(msg.contains("agent-x"), "must include agent id");
        assert!(
            msg.contains("turn_success_rate"),
            "must include metric name"
        );
        assert!(msg.contains("92.0%"), "must include expected (baseline)");
        assert!(msg.contains("65.0%"), "must include actual (current)");
        assert!(
            msg.contains("27.0%p"),
            "must include drop in percentage points"
        );
        assert!(msg.contains("sample=14/80"), "must include sample sizes");
        assert!(msg.contains("drill="), "must include drill-down link");
        assert!(
            msg.contains("agent-x"),
            "drill-down link must reference agent id"
        );
    }

    #[test]
    fn channel_resolution_prefers_env_override_then_falls_back_to_adk_cc() {
        // Reset env to a known state.
        let prev = std::env::var(ALERT_CHANNEL_ENV).ok();
        // SAFETY: tests are single-threaded for env mutation; cleanup at end.
        unsafe {
            std::env::remove_var(ALERT_CHANNEL_ENV);
        }
        assert_eq!(resolve_alert_channel(), FALLBACK_ALERT_CHANNEL);

        unsafe {
            std::env::set_var(ALERT_CHANNEL_ENV, "1234567890");
        }
        assert_eq!(resolve_alert_channel(), "1234567890");

        // Empty / whitespace falls back to default.
        unsafe {
            std::env::set_var(ALERT_CHANNEL_ENV, "   ");
        }
        assert_eq!(resolve_alert_channel(), FALLBACK_ALERT_CHANNEL);

        // restore
        unsafe {
            match prev {
                Some(value) => std::env::set_var(ALERT_CHANNEL_ENV, value),
                None => std::env::remove_var(ALERT_CHANNEL_ENV),
            }
        }
    }
}
