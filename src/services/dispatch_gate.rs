//! Rate-limit-aware dispatch gate (feature: rate-limit-aware-dispatch-gate).
//!
//! AgentDesk caches per-provider rate-limit telemetry in the `rate_limit_cache`
//! Postgres table (refreshed every ~120s by `rate_limit_sync_loop`). Before this
//! module, that pressure was observational only: auto-queue activation could
//! still claim work for a provider that is known to be rate-limited, producing
//! doomed turns and wasted recovery cycles.
//!
//! This module adds a **pure, lock-free, in-memory** provider-pressure
//! evaluator plus the process-wide snapshots it reads from. The auto-queue hot
//! dispatch path calls [`evaluate_agent_provider_pressure`] which performs
//! **zero database queries** — it only reads two `RwLock`-guarded snapshots that
//! are refreshed off the hot path by the rate-limit sync loop:
//!
//! * [`PROVIDER_PRESSURE`] — provider -> [`ProviderPressureSnapshot`]
//! * [`AGENT_PROVIDER`]     — agent_id -> provider string
//!
//! Design constraints honored (see PRD/spec + adversarial review):
//! * O(1) in-memory cache read on the dispatch path, no DB round-trip, no
//!   blocking I/O, no probe spawning.
//! * Stale / missing / malformed / unsupported telemetry degrades to
//!   `UnknownAllow` (never blocks unrelated providers, never panics).
//! * Gemini's unknown utilization (used==0, no reset) degrades to
//!   `UnknownAllow`.
//! * The gate enable flag and the danger threshold are read live from
//!   `config_live_reload::current()` (hot-reload, no restart), so disabling the
//!   gate cleanly resumes dispatch on the next activation.
//! * Output is a serializable [`ProviderPressureDecision`] with stable reason
//!   codes — never string parsing in route handlers, never the terminal
//!   auto-queue `skipped` status.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{OnceLock, RwLock};

use serde::Serialize;
use serde_json::Value;

/// Default danger threshold (utilization %) used when no runtime override is
/// configured. Mirrors the dashboard `rateLimitDangerPct` default (95) so the
/// gate and the dashboard agree on what "danger" means.
pub const DEFAULT_DANGER_PCT: u64 = 95;

/// Default staleness window (seconds). A cache row older than this is treated as
/// stale and degrades to `UnknownAllow`. Mirrors the dashboard
/// `rateLimitStaleSec` default (600).
pub const DEFAULT_STALE_SEC: i64 = 600;

/// Stable reason codes for a gate decision. Serialized as snake_case strings in
/// the additive `/dispatch-next` and `/api/health/detail` payloads. Additive
/// only: never remove or retype an existing variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PressureReasonCode {
    /// Gate is disabled by runtime config; always allow.
    GateDisabled,
    /// Fresh telemetry shows utilization below the danger threshold.
    BelowDanger,
    /// Fresh telemetry shows utilization at or above the danger threshold.
    DangerThreshold,
    /// No cache row for the provider; allow (fail-open).
    NoTelemetry,
    /// Cache row is older than the stale window; allow (fail-open).
    StaleTelemetry,
    /// Cache row is malformed / unparseable; allow (fail-open).
    MalformedTelemetry,
    /// Provider has no rate-limit telemetry source (OpenCode/Qwen); allow.
    UnsupportedProvider,
    /// Provider utilization is structurally unknown (e.g. Gemini); allow.
    UnknownUtilization,
    /// Could not resolve a provider for the agent; allow (fail-open).
    ProviderUnresolved,
}

/// Coarse classification of a decision. `allow` -> dispatch may proceed;
/// `defer` -> dispatch is held (entry stays pending).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PressureVerdict {
    Allow,
    Defer,
}

impl PressureVerdict {
    pub fn is_defer(self) -> bool {
        matches!(self, PressureVerdict::Defer)
    }
}

/// Serializable decision returned by the evaluator. Stable contract consumed by
/// the auto-queue activation loop and the additive API payloads.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderPressureDecision {
    pub verdict: PressureVerdict,
    pub reason_code: PressureReasonCode,
    /// Resolved provider, when known (None when unresolved).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Observed utilization percent that drove a `defer`, when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub utilization_pct: Option<u64>,
    /// Danger threshold that was applied.
    pub danger_pct: u64,
    /// Age of the telemetry row in seconds at evaluation time, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_age_sec: Option<i64>,
    /// Best-effort reset timestamp hint (unix seconds) for the busy bucket.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_reset_at: Option<i64>,
}

impl ProviderPressureDecision {
    fn allow(reason_code: PressureReasonCode, provider: Option<String>, danger_pct: u64) -> Self {
        Self {
            verdict: PressureVerdict::Allow,
            reason_code,
            provider,
            utilization_pct: None,
            danger_pct,
            cache_age_sec: None,
            estimated_reset_at: None,
        }
    }
}

/// Snapshot of one provider's pressure, derived from a `rate_limit_cache` row.
/// Stored in-memory so the dispatch path never touches Postgres.
#[derive(Debug, Clone)]
pub struct ProviderPressureSnapshot {
    /// Highest utilization percent across the provider's buckets, when known.
    pub max_utilization_pct: Option<u64>,
    /// Reset timestamp (unix seconds) of the bucket with the highest
    /// utilization, when known.
    pub busy_reset_at: Option<i64>,
    /// Unix-seconds timestamp the underlying cache row was fetched.
    pub fetched_at: i64,
    /// Provider has no telemetry source (OpenCode/Qwen) — always allow.
    pub unsupported: bool,
    /// Row existed but no parseable utilization could be derived (e.g. Gemini
    /// quota-only buckets) — degrade to UnknownAllow.
    pub malformed: bool,
}

/// Process-wide provider -> pressure snapshot, refreshed off the hot path by
/// `rate_limit_sync_loop`. Keys are lowercased provider names.
static PROVIDER_PRESSURE: OnceLock<RwLock<HashMap<String, ProviderPressureSnapshot>>> =
    OnceLock::new();

/// Process-wide agent_id -> provider snapshot, refreshed off the hot path by
/// `rate_limit_sync_loop`. Keys are agent ids; values are lowercased provider
/// names resolved from agent/channel bindings.
static AGENT_PROVIDER: OnceLock<RwLock<HashMap<String, String>>> = OnceLock::new();

fn pressure_map() -> &'static RwLock<HashMap<String, ProviderPressureSnapshot>> {
    PROVIDER_PRESSURE.get_or_init(|| RwLock::new(HashMap::new()))
}

fn agent_provider_map() -> &'static RwLock<HashMap<String, String>> {
    AGENT_PROVIDER.get_or_init(|| RwLock::new(HashMap::new()))
}

// ── In-memory diagnostics counters (no DB persistence in P0) ────────────────

static GATE_EVALUATIONS: AtomicU64 = AtomicU64::new(0);
static GATE_DEFERRALS: AtomicU64 = AtomicU64::new(0);
static GATE_BYPASSES: AtomicU64 = AtomicU64::new(0);
/// Unix-seconds timestamp of the last `defer` decision (0 == never).
static LAST_DEFER_AT: AtomicU64 = AtomicU64::new(0);

/// Aggregate, credential-free diagnostics for `/api/health/detail`. Counts are
/// process-lifetime, in-memory only (no DB, no token data).
#[derive(Debug, Clone, Serialize)]
pub struct DispatchGateDiagnostics {
    pub enabled: bool,
    pub danger_pct: u64,
    pub evaluations: u64,
    pub deferrals: u64,
    pub bypasses: u64,
    /// Number of providers currently held in the in-memory pressure snapshot.
    pub providers_tracked: usize,
    /// Unix-seconds of the most recent defer decision, when one has occurred.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_defer_at: Option<i64>,
}

/// Replace the in-memory provider-pressure snapshot. Called off the hot path by
/// the rate-limit sync loop after it refreshes the Postgres cache.
pub fn set_provider_pressure_snapshot(snapshot: HashMap<String, ProviderPressureSnapshot>) {
    let lock = pressure_map();
    *lock.write().unwrap_or_else(|p| p.into_inner()) = snapshot;
}

/// Replace the in-memory agent_id -> provider snapshot. Called off the hot path
/// by the rate-limit sync loop.
pub fn set_agent_provider_snapshot(snapshot: HashMap<String, String>) {
    let lock = agent_provider_map();
    *lock.write().unwrap_or_else(|p| p.into_inner()) = snapshot;
}

/// Build a [`ProviderPressureSnapshot`] from a single provider's cache-payload
/// JSON `Value` (the same `{provider, buckets, fetched_at, stale, unsupported}`
/// shape produced by `build_rate_limit_provider_payloads_pg`). Pure helper so it
/// is unit-testable without a database.
pub fn snapshot_from_provider_payload(
    payload: &Value,
) -> Option<(String, ProviderPressureSnapshot)> {
    let provider = payload
        .get("provider")
        .and_then(Value::as_str)?
        .to_lowercase();
    let fetched_at = payload
        .get("fetched_at")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let unsupported = payload
        .get("unsupported")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    if unsupported {
        return Some((
            provider,
            ProviderPressureSnapshot {
                max_utilization_pct: None,
                busy_reset_at: None,
                fetched_at,
                unsupported: true,
                malformed: false,
            },
        ));
    }

    let buckets = payload.get("buckets").and_then(Value::as_array);
    let Some(buckets) = buckets else {
        return Some((
            provider,
            ProviderPressureSnapshot {
                max_utilization_pct: None,
                busy_reset_at: None,
                fetched_at,
                unsupported: false,
                malformed: true,
            },
        ));
    };

    // Derive utilization% from each bucket. Buckets are
    // `{name, limit, used, remaining, reset}`. Claude/Codex encode utilization
    // as `used` with `limit == 100`. Gemini encodes quota-only buckets with
    // `used == 0` and a non-100 `limit`, which is NOT a real utilization signal,
    // so those are treated as unknown (malformed) and fail open.
    let mut max_util: Option<u64> = None;
    let mut busy_reset: Option<i64> = None;
    let mut had_util_bucket = false;
    for bucket in buckets {
        let limit = bucket.get("limit").and_then(Value::as_i64);
        let used = bucket.get("used").and_then(Value::as_i64);
        let reset = bucket
            .get("reset")
            .and_then(Value::as_i64)
            .filter(|reset| *reset > 0);
        let util = match (limit, used) {
            // Percent-encoded bucket (Claude/Codex): limit == 100, used is %.
            (Some(100), Some(used)) => Some(used.clamp(0, 100) as u64),
            // Generic ratio bucket: derive percent from used/limit.
            (Some(limit), Some(used)) if limit > 0 && used > 0 => Some(
                ((used as f64 / limit as f64) * 100.0)
                    .round()
                    .clamp(0.0, 100.0) as u64,
            ),
            _ => None,
        };
        if let Some(util) = util {
            had_util_bucket = true;
            if max_util.is_none_or(|current| util > current) {
                max_util = Some(util);
                busy_reset = reset;
            }
        }
    }

    Some((
        provider,
        ProviderPressureSnapshot {
            max_utilization_pct: max_util,
            busy_reset_at: busy_reset,
            fetched_at,
            unsupported: false,
            // No bucket carried a usable utilization signal -> unknown.
            malformed: !had_util_bucket,
        },
    ))
}

/// Convert a list of provider payloads (from
/// `build_rate_limit_provider_payloads_pg`) into the in-memory pressure map.
pub fn pressure_snapshot_from_payloads(
    payloads: &[Value],
) -> HashMap<String, ProviderPressureSnapshot> {
    let mut map = HashMap::new();
    for payload in payloads {
        if let Some((provider, snapshot)) = snapshot_from_provider_payload(payload) {
            map.insert(provider, snapshot);
        }
    }
    map
}

/// Read the live gate-enabled flag. Defaults to `true` (gate ON) when the live
/// config snapshot is unavailable or the field is unset (`None`). Reading the
/// in-memory `config_live_reload` snapshot is lock-cheap and does NOT touch the
/// database, so this is safe on the hot path.
pub fn gate_enabled() -> bool {
    crate::config_live_reload::current()
        .and_then(|config| config.runtime.dispatch_rate_limit_gate_enabled)
        .unwrap_or(true)
}

/// Read the live danger threshold (utilization %). Falls back to
/// [`DEFAULT_DANGER_PCT`] when unset.
pub fn danger_pct() -> u64 {
    crate::config_live_reload::current()
        .and_then(|config| config.runtime.rate_limit_danger_pct)
        .unwrap_or(DEFAULT_DANGER_PCT)
}

/// Read the live staleness window (seconds). Falls back to
/// [`DEFAULT_STALE_SEC`] when unset.
pub fn stale_sec() -> i64 {
    crate::config_live_reload::current()
        .and_then(|config| config.runtime.rate_limit_stale_sec)
        .map(|value| value as i64)
        .unwrap_or(DEFAULT_STALE_SEC)
}

/// Pure evaluator: given a (possibly absent) provider pressure snapshot and the
/// effective thresholds, decide whether to allow or defer. No I/O, no globals —
/// fully unit-testable. `now` is unix seconds.
pub fn evaluate_provider_pressure(
    provider: &str,
    snapshot: Option<&ProviderPressureSnapshot>,
    danger_pct: u64,
    stale_sec: i64,
    now: i64,
) -> ProviderPressureDecision {
    let provider_owned = Some(provider.to_string());
    let Some(snapshot) = snapshot else {
        return ProviderPressureDecision::allow(
            PressureReasonCode::NoTelemetry,
            provider_owned,
            danger_pct,
        );
    };

    if snapshot.unsupported {
        return ProviderPressureDecision::allow(
            PressureReasonCode::UnsupportedProvider,
            provider_owned,
            danger_pct,
        );
    }

    let cache_age = now.saturating_sub(snapshot.fetched_at);
    if cache_age > stale_sec {
        let mut decision = ProviderPressureDecision::allow(
            PressureReasonCode::StaleTelemetry,
            provider_owned,
            danger_pct,
        );
        decision.cache_age_sec = Some(cache_age);
        return decision;
    }

    if snapshot.malformed {
        return ProviderPressureDecision::allow(
            PressureReasonCode::MalformedTelemetry,
            provider_owned,
            danger_pct,
        );
    }

    let Some(util) = snapshot.max_utilization_pct else {
        return ProviderPressureDecision::allow(
            PressureReasonCode::UnknownUtilization,
            provider_owned,
            danger_pct,
        );
    };

    if util >= danger_pct {
        ProviderPressureDecision {
            verdict: PressureVerdict::Defer,
            reason_code: PressureReasonCode::DangerThreshold,
            provider: provider_owned,
            utilization_pct: Some(util),
            danger_pct,
            cache_age_sec: Some(cache_age),
            estimated_reset_at: snapshot.busy_reset_at,
        }
    } else {
        let mut decision = ProviderPressureDecision::allow(
            PressureReasonCode::BelowDanger,
            provider_owned,
            danger_pct,
        );
        decision.utilization_pct = Some(util);
        decision.cache_age_sec = Some(cache_age);
        decision
    }
}

/// Resolve the provider for `agent_id` from the in-memory snapshot. Returns
/// `None` when the snapshot has not been populated yet or the agent is unknown
/// (caller fails open). No DB query.
pub fn resolve_agent_provider(agent_id: &str) -> Option<String> {
    let lock = agent_provider_map();
    lock.read()
        .unwrap_or_else(|p| p.into_inner())
        .get(agent_id)
        .cloned()
}

/// Hot-path entry point used by auto-queue activation. Performs an O(1),
/// lock-cheap, DB-free evaluation for the given agent and records diagnostics.
///
/// Fails open on every ambiguity: gate disabled, unresolved provider, missing /
/// stale / malformed / unsupported / unknown telemetry all return `Allow`.
pub fn evaluate_agent_provider_pressure(agent_id: &str, now: i64) -> ProviderPressureDecision {
    GATE_EVALUATIONS.fetch_add(1, Ordering::Relaxed);

    let danger = danger_pct();

    if !gate_enabled() {
        return ProviderPressureDecision::allow(
            PressureReasonCode::GateDisabled,
            resolve_agent_provider(agent_id),
            danger,
        );
    }

    let Some(provider) = resolve_agent_provider(agent_id) else {
        return ProviderPressureDecision::allow(
            PressureReasonCode::ProviderUnresolved,
            None,
            danger,
        );
    };

    let decision = {
        let lock = pressure_map();
        let map = lock.read().unwrap_or_else(|p| p.into_inner());
        evaluate_provider_pressure(&provider, map.get(&provider), danger, stale_sec(), now)
    };

    if decision.verdict.is_defer() {
        GATE_DEFERRALS.fetch_add(1, Ordering::Relaxed);
        if now > 0 {
            LAST_DEFER_AT.store(now as u64, Ordering::Relaxed);
        }
    }

    decision
}

/// Record that a manual/force dispatch path intentionally bypassed the gate
/// (REQ-005). In-memory counter only; never blocks the bypass.
///
/// In P0 the gate is wired ONLY into auto-queue activation, so manual and
/// direct dispatch paths bypass it implicitly by never calling the evaluator —
/// their behavior is unchanged (REQ-005). This function is the stable
/// accounting surface a later opt-in hard-block PR will call from those paths to
/// annotate an explicit bypass; it is exposed and tested now so the diagnostics
/// contract is stable. Intentionally unwired in P0 (mirrors the codebase's
/// existing intentional-but-unwired infra convention).
#[allow(dead_code)]
pub fn record_gate_bypass() {
    GATE_BYPASSES.fetch_add(1, Ordering::Relaxed);
}

/// Build the credential-free diagnostics block for `/api/health/detail`.
pub fn diagnostics() -> DispatchGateDiagnostics {
    let providers_tracked = pressure_map()
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .len();
    let last_defer_at = match LAST_DEFER_AT.load(Ordering::Relaxed) {
        0 => None,
        value => Some(value as i64),
    };
    DispatchGateDiagnostics {
        enabled: gate_enabled(),
        danger_pct: danger_pct(),
        evaluations: GATE_EVALUATIONS.load(Ordering::Relaxed),
        deferrals: GATE_DEFERRALS.load(Ordering::Relaxed),
        bypasses: GATE_BYPASSES.load(Ordering::Relaxed),
        providers_tracked,
        last_defer_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn snap(
        max_util: Option<u64>,
        reset: Option<i64>,
        fetched_at: i64,
        unsupported: bool,
        malformed: bool,
    ) -> ProviderPressureSnapshot {
        ProviderPressureSnapshot {
            max_utilization_pct: max_util,
            busy_reset_at: reset,
            fetched_at,
            unsupported,
            malformed,
        }
    }

    // TEST-001: pure evaluator reason codes ---------------------------------

    #[test]
    fn missing_row_allows_no_telemetry() {
        let decision = evaluate_provider_pressure("claude", None, 95, 600, 1_000);
        assert_eq!(decision.verdict, PressureVerdict::Allow);
        assert_eq!(decision.reason_code, PressureReasonCode::NoTelemetry);
    }

    #[test]
    fn fresh_below_danger_allows() {
        let s = snap(Some(40), Some(2_000), 1_000, false, false);
        let decision = evaluate_provider_pressure("claude", Some(&s), 95, 600, 1_100);
        assert_eq!(decision.verdict, PressureVerdict::Allow);
        assert_eq!(decision.reason_code, PressureReasonCode::BelowDanger);
        assert_eq!(decision.utilization_pct, Some(40));
    }

    #[test]
    fn fresh_danger_defers_with_reset_hint() {
        let s = snap(Some(97), Some(5_555), 1_000, false, false);
        let decision = evaluate_provider_pressure("claude", Some(&s), 95, 600, 1_100);
        assert_eq!(decision.verdict, PressureVerdict::Defer);
        assert_eq!(decision.reason_code, PressureReasonCode::DangerThreshold);
        assert_eq!(decision.utilization_pct, Some(97));
        assert_eq!(decision.estimated_reset_at, Some(5_555));
    }

    #[test]
    fn at_threshold_defers() {
        let s = snap(Some(95), None, 1_000, false, false);
        let decision = evaluate_provider_pressure("codex", Some(&s), 95, 600, 1_050);
        assert_eq!(decision.verdict, PressureVerdict::Defer);
    }

    #[test]
    fn stale_row_allows_even_if_over_danger() {
        // fetched far in the past -> stale -> fail open.
        let s = snap(Some(99), Some(2_000), 1_000, false, false);
        let decision = evaluate_provider_pressure("claude", Some(&s), 95, 600, 5_000);
        assert_eq!(decision.verdict, PressureVerdict::Allow);
        assert_eq!(decision.reason_code, PressureReasonCode::StaleTelemetry);
        assert_eq!(decision.cache_age_sec, Some(4_000));
    }

    #[test]
    fn unsupported_provider_allows() {
        let s = snap(None, None, 1_000, true, false);
        let decision = evaluate_provider_pressure("opencode", Some(&s), 95, 600, 1_050);
        assert_eq!(decision.verdict, PressureVerdict::Allow);
        assert_eq!(
            decision.reason_code,
            PressureReasonCode::UnsupportedProvider
        );
    }

    #[test]
    fn malformed_row_allows() {
        let s = snap(None, None, 1_000, false, true);
        let decision = evaluate_provider_pressure("claude", Some(&s), 95, 600, 1_050);
        assert_eq!(decision.verdict, PressureVerdict::Allow);
        assert_eq!(decision.reason_code, PressureReasonCode::MalformedTelemetry);
    }

    #[test]
    fn unknown_utilization_allows() {
        // Row is fresh & well-formed but carries no utilization (Gemini-like).
        let s = snap(None, None, 1_000, false, false);
        let decision = evaluate_provider_pressure("gemini", Some(&s), 95, 600, 1_050);
        assert_eq!(decision.verdict, PressureVerdict::Allow);
        assert_eq!(decision.reason_code, PressureReasonCode::UnknownUtilization);
    }

    // TEST-001: payload -> snapshot derivation ------------------------------

    #[test]
    fn snapshot_from_claude_percent_buckets() {
        let payload = json!({
            "provider": "claude",
            "fetched_at": 1_000,
            "unsupported": false,
            "buckets": [
                {"name": "5h", "limit": 100, "used": 30, "remaining": 70, "reset": 2_000},
                {"name": "7d", "limit": 100, "used": 97, "remaining": 3, "reset": 9_000},
            ],
        });
        let (provider, snapshot) = snapshot_from_provider_payload(&payload).unwrap();
        assert_eq!(provider, "claude");
        assert_eq!(snapshot.max_utilization_pct, Some(97));
        assert_eq!(snapshot.busy_reset_at, Some(9_000));
        assert!(!snapshot.malformed);
    }

    #[test]
    fn snapshot_from_gemini_quota_buckets_is_unknown() {
        // Gemini buckets are quota-only (used == 0, limit != 100) -> no usable
        // utilization signal -> malformed/unknown -> fail open.
        let payload = json!({
            "provider": "gemini",
            "fetched_at": 1_000,
            "unsupported": false,
            "buckets": [
                {"name": "rpm", "limit": 60, "used": 0, "remaining": 60, "reset": 0},
                {"name": "rpd", "limit": 1000, "used": 0, "remaining": 1000, "reset": 0},
            ],
        });
        let (provider, snapshot) = snapshot_from_provider_payload(&payload).unwrap();
        assert_eq!(provider, "gemini");
        assert_eq!(snapshot.max_utilization_pct, None);
        assert!(snapshot.malformed);
    }

    #[test]
    fn snapshot_from_unsupported_provider() {
        let payload = json!({
            "provider": "opencode",
            "fetched_at": 1_000,
            "unsupported": true,
            "buckets": [],
        });
        let (provider, snapshot) = snapshot_from_provider_payload(&payload).unwrap();
        assert_eq!(provider, "opencode");
        assert!(snapshot.unsupported);
    }

    #[test]
    fn snapshot_missing_buckets_is_malformed() {
        let payload = json!({"provider": "claude", "fetched_at": 1_000, "unsupported": false});
        let (_, snapshot) = snapshot_from_provider_payload(&payload).unwrap();
        assert!(snapshot.malformed);
    }

    #[test]
    fn decision_serializes_with_stable_field_names() {
        let s = snap(Some(97), Some(5_555), 1_000, false, false);
        let decision = evaluate_provider_pressure("claude", Some(&s), 95, 600, 1_100);
        let value = serde_json::to_value(&decision).unwrap();
        assert_eq!(value["verdict"], json!("defer"));
        assert_eq!(value["reason_code"], json!("danger_threshold"));
        assert_eq!(value["provider"], json!("claude"));
        assert_eq!(value["utilization_pct"], json!(97));
        assert_eq!(value["danger_pct"], json!(95));
        assert_eq!(value["estimated_reset_at"], json!(5_555));
    }

    // ── Hot-path + rollback/disable tests (REQ-002, REQ-003, REQ-005,
    //    REQ-006) using the process-global config + snapshots ──────────────

    /// Serializes tests that mutate the process-global `config_live_reload`
    /// snapshot and the dispatch-gate snapshots so they do not race the parallel
    /// test runner.
    fn global_gate_test_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    fn install_runtime(enabled: Option<bool>, danger: Option<u64>, stale: Option<u64>) {
        let mut config = crate::config::Config::default();
        config.runtime.dispatch_rate_limit_gate_enabled = enabled;
        config.runtime.rate_limit_danger_pct = danger;
        config.runtime.rate_limit_stale_sec = stale;
        crate::config_live_reload::install(config);
    }

    #[test]
    fn gate_defers_when_provider_in_danger() {
        let _guard = global_gate_test_guard();
        install_runtime(Some(true), Some(95), Some(600));
        let now = 10_000;
        set_agent_provider_snapshot(HashMap::from([(
            "agent-x".to_string(),
            "claude".to_string(),
        )]));
        set_provider_pressure_snapshot(HashMap::from([(
            "claude".to_string(),
            snap(Some(99), Some(20_000), now - 10, false, false),
        )]));

        let decision = evaluate_agent_provider_pressure("agent-x", now);
        assert_eq!(decision.verdict, PressureVerdict::Defer);
        assert_eq!(decision.reason_code, PressureReasonCode::DangerThreshold);
        assert_eq!(decision.provider.as_deref(), Some("claude"));
    }

    #[test]
    fn rollback_disabling_gate_resumes_dispatch_for_blocked_entry() {
        // REQ-003 / TSK-P0-007: a provider in danger that would defer with the
        // gate enabled must ALLOW the moment the gate is disabled — proving the
        // disable flag cleanly resumes dispatch without any partial-block state.
        let _guard = global_gate_test_guard();
        let now = 10_000;
        set_agent_provider_snapshot(HashMap::from([(
            "agent-y".to_string(),
            "codex".to_string(),
        )]));
        set_provider_pressure_snapshot(HashMap::from([(
            "codex".to_string(),
            snap(Some(99), Some(20_000), now - 10, false, false),
        )]));

        // Enabled -> defer.
        install_runtime(Some(true), Some(95), Some(600));
        assert_eq!(
            evaluate_agent_provider_pressure("agent-y", now).verdict,
            PressureVerdict::Defer
        );

        // Disabled (rollback) -> allow, with the gate_disabled reason code.
        install_runtime(Some(false), Some(95), Some(600));
        let resumed = evaluate_agent_provider_pressure("agent-y", now);
        assert_eq!(resumed.verdict, PressureVerdict::Allow);
        assert_eq!(resumed.reason_code, PressureReasonCode::GateDisabled);
    }

    #[test]
    fn lowering_danger_threshold_below_utilization_resumes_dispatch() {
        // REQ-003 / TSK-P0-007 alternate disable path: same blocked entry
        // resumes when the danger threshold is raised above the observed
        // utilization (here we lower utilization by raising the bar to 100 while
        // util is 99 -> below danger -> allow).
        let _guard = global_gate_test_guard();
        let now = 10_000;
        set_agent_provider_snapshot(HashMap::from([(
            "agent-z".to_string(),
            "claude".to_string(),
        )]));
        set_provider_pressure_snapshot(HashMap::from([(
            "claude".to_string(),
            snap(Some(99), None, now - 10, false, false),
        )]));

        install_runtime(Some(true), Some(95), Some(600));
        assert_eq!(
            evaluate_agent_provider_pressure("agent-z", now).verdict,
            PressureVerdict::Defer
        );

        install_runtime(Some(true), Some(100), Some(600));
        let resumed = evaluate_agent_provider_pressure("agent-z", now);
        assert_eq!(resumed.verdict, PressureVerdict::Allow);
        assert_eq!(resumed.reason_code, PressureReasonCode::BelowDanger);
    }

    #[test]
    fn default_when_gate_flag_unset_is_enabled() {
        // REQ-003: None gate flag defaults to ON (gate enabled).
        let _guard = global_gate_test_guard();
        let now = 10_000;
        set_agent_provider_snapshot(HashMap::from([(
            "agent-d".to_string(),
            "claude".to_string(),
        )]));
        set_provider_pressure_snapshot(HashMap::from([(
            "claude".to_string(),
            snap(Some(99), None, now - 10, false, false),
        )]));
        install_runtime(None, None, None);
        assert!(gate_enabled());
        assert_eq!(danger_pct(), DEFAULT_DANGER_PCT);
        assert_eq!(
            evaluate_agent_provider_pressure("agent-d", now).verdict,
            PressureVerdict::Defer
        );
    }

    #[test]
    fn unresolved_agent_provider_fails_open() {
        // REQ-006: an agent with no provider binding in the snapshot allows.
        let _guard = global_gate_test_guard();
        install_runtime(Some(true), Some(95), Some(600));
        set_agent_provider_snapshot(HashMap::new());
        let decision = evaluate_agent_provider_pressure("unknown-agent", 10_000);
        assert_eq!(decision.verdict, PressureVerdict::Allow);
        assert_eq!(decision.reason_code, PressureReasonCode::ProviderUnresolved);
    }

    #[test]
    fn bypass_counter_increments() {
        // REQ-005: explicit bypass accounting surface increments the counter
        // without affecting verdicts.
        let _guard = global_gate_test_guard();
        let before = GATE_BYPASSES.load(Ordering::Relaxed);
        record_gate_bypass();
        assert_eq!(GATE_BYPASSES.load(Ordering::Relaxed), before + 1);
    }
}
